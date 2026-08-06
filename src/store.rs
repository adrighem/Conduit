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
    slack_timestamp_is_after, SlackConversation, SlackConversationUnreadSnapshot, SlackMessage,
    SlackUser, SlackUserStatus,
};
#[cfg(test)]
use crate::models::{SlackUnreadState, LOCAL_READ_TS_KEY};
use crate::slack_message_wire::normalize_cached_messages;
use crate::thread_catalog::{ThreadCatalog, ThreadRecord};
use crate::workspace_pipeline::{
    same_message_identity, ConversationAttentionObservation, MessageMutationKind, StoreBatch,
    StoreChange, WorkspaceRevision,
};

pub(crate) const CACHE_VERSION: u32 = 1;
const DATABASE_SCHEMA_VERSION: u32 = 2;
const DATABASE_FILENAME: &str = "state.sqlite3";
const MAX_CACHED_CHANNEL_MESSAGES: usize = 200;
const ATTENTION_DELIVERY_KIND: &str = "attention_delivery";
const ATTENTION_DELIVERY_LEDGER_KEY: &str = "__ledger__";
const MAX_ATTENTION_DELIVERIES: usize = 512;
const STORE_WRITER_QUEUE_CAPACITY: usize = 64;
const STORE_READER_QUEUE_CAPACITY: usize = 32;
const STORE_READER_COUNT: usize = 2;
const STORE_MAINTENANCE_BATCH_LIMIT: usize = 50;
const STORE_MAINTENANCE_BATCH_WINDOW: Duration = Duration::from_millis(50);
const PENDING_UNREAD_QUEUE_KEY: &str = "__queue__";

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

    pub(crate) fn category(&self) -> StoreErrorCategory {
        match self {
            Self::RejectedUpdate { .. } => StoreErrorCategory::RejectedUpdate,
            Self::IncompatibleSchema { .. } => StoreErrorCategory::IncompatibleSchema,
            Self::InvalidDerivedCache { .. } => StoreErrorCategory::CorruptData,
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
        self.dispatch(self.inner.writer.clone(), task).await
    }

    pub(crate) async fn query<T, F>(&self, task: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let reader =
            self.inner.next_reader.fetch_add(1, Ordering::Relaxed) % self.inner.readers.len();
        self.dispatch(self.inner.readers[reader].clone(), task)
            .await
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
        worker: tokio::sync::mpsc::Sender<StoreWorkerRequest>,
        task: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let admission = self.inner.admission.lock().await;
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
        drop(admission);

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
                std::thread::park_timeout(Duration::from_millis(1));
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttentionObservationStatus {
    InvalidIdentity,
    AtOrBeforeReadCursor,
    AlreadyObserved,
    Accepted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AttentionDeliveryOutcome {
    pub(crate) observation: AttentionObservationStatus,
    pub(crate) notification_claimed: bool,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SyncFreshness {
    pub(crate) refreshed_at_ms: Option<i64>,
    pub(crate) retry_count: u32,
    pub(crate) retry_after_ms: Option<i64>,
}

enum ConversationRowMutation<R> {
    Unchanged(R),
    Upsert(SlackConversation, R),
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

    pub async fn load_pending_unread_refresh(&self) -> Result<Vec<String>> {
        let workspace_key = self.workspace_key.clone();
        self.query_or_reset(Vec::new(), move |connection| {
            let mut statement = connection.prepare(
                "SELECT item_key, payload_json FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'pending_unread'",
            )?;
            let rows = statement
                .query_map([workspace_key], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(StoreError::from)?;
            let mut queue = Vec::new();
            let mut legacy = Vec::new();
            for (item_key, payload) in rows {
                if item_key == PENDING_UNREAD_QUEUE_KEY {
                    if let Ok(stored) = serde_json::from_str::<Vec<String>>(&payload) {
                        queue.extend(stored);
                    }
                } else {
                    legacy.push(item_key);
                }
            }
            legacy.sort();
            queue.extend(legacy);
            Ok(normalized_pending_unread_queue(queue))
        })
        .await
    }

    pub async fn store_pending_unread_refresh(&self, channel_ids: &[String]) -> Result<()> {
        let queue = normalized_pending_unread_queue(channel_ids.iter().cloned());
        let values = HashMap::from([(PENDING_UNREAD_QUEUE_KEY.to_string(), queue)]);
        self.store_kind_map("pending_unread", values, true).await
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

    /// Reconciles an authoritative membership response in one locked cache
    /// transaction, so concurrent realtime/read overlays cannot be replaced by
    /// an older read-modify-write cycle.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn reconcile_conversations(
        &self,
        fresh: Vec<SlackConversation>,
    ) -> Result<Vec<SlackConversation>> {
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let existing: Vec<SlackConversation> =
                    load_sqlite_kind_values(&transaction, &workspace_key, "conversation")?;
                if fresh.is_empty() && !existing.is_empty() {
                    return Err(StoreError::rejected_update(
                        "Slack returned an unexpectedly empty conversation membership snapshot",
                    ));
                }
                let mut catalog = ConversationCatalog::from_cached(existing);
                let mut snapshot = catalog.begin_membership_snapshot();
                for conversation in fresh {
                    snapshot.upsert(conversation);
                }
                catalog.commit_membership_snapshot(snapshot);
                let conversations = catalog.conversations();
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                changed |= sync_sqlite_kind(
                    &transaction,
                    &workspace_key,
                    "conversation",
                    conversations
                        .iter()
                        .map(conversation_for_cache)
                        .map(|conversation| (conversation.id.clone(), conversation)),
                )?;
                finish_sqlite_transaction(transaction, changed)?;
                Ok(conversations)
            })
            .await
    }

    /// Merges one cached conversation without replacing newer unread/read
    /// overlays or the rest of the workspace snapshot.
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
            ConversationRowMutation::Upsert(conversation, ())
        })
        .await
    }

    #[cfg(test)]
    pub async fn merge_conversation(&self, conversation: &SlackConversation) -> Result<()> {
        self.store_conversation(conversation).await
    }

    /// Applies an unread-state patch to one cached conversation atomically.
    /// Returns `false` when the state is unknown or the conversation is not in
    /// the cache, allowing callers to decide whether a full snapshot is needed.
    #[cfg(test)]
    pub async fn apply_conversation_unread_state(
        &self,
        channel_id: &str,
        unread_state: SlackUnreadState,
        server_last_read: Option<&str>,
    ) -> Result<bool> {
        self.apply_conversation_unread_snapshot(&SlackConversationUnreadSnapshot {
            channel_id: channel_id.to_string(),
            unread_state,
            last_read: server_last_read.map(str::to_string),
            ..Default::default()
        })
        .await
    }

    /// Applies a complete server unread snapshot to one cached conversation
    /// atomically, without allowing it to roll back a newer local read.
    #[cfg(test)]
    pub async fn apply_conversation_unread_snapshot(
        &self,
        snapshot: &SlackConversationUnreadSnapshot,
    ) -> Result<bool> {
        if snapshot.channel_id.trim().is_empty() || !snapshot.unread_state.known {
            return Ok(false);
        }

        let snapshot = snapshot.clone();
        let channel_id = snapshot.channel_id.clone();
        self.mutate_conversation_row(&channel_id, move |conversation| {
            let Some(mut conversation) = conversation else {
                return ConversationRowMutation::Unchanged(false);
            };
            if conversation.unread_snapshot_rewinds_read(&snapshot) {
                return ConversationRowMutation::Unchanged(false);
            }
            conversation.clear_local_read_ts();
            conversation.apply_unread_snapshot(&snapshot);
            ConversationRowMutation::Upsert(conversation, true)
        })
        .await
    }

    /// Advances one cached conversation's read cursor without assuming that
    /// messages newer than the supplied cursor have been read.
    pub async fn advance_conversation_read_cursor(
        &self,
        channel_id: &str,
        last_read: &str,
    ) -> Result<bool> {
        if channel_id.trim().is_empty() {
            return Ok(false);
        }

        let last_read = last_read.to_string();
        self.update_conversation(channel_id, move |conversation| {
            let reached_latest = conversation
                .latest_message_ts()
                .is_none_or(|latest| latest <= last_read.as_str());
            if reached_latest {
                conversation.clear_raw_unread_activity();
                if conversation.attention.is_none() {
                    conversation.clear_attention_activity();
                }
            }
            conversation.acknowledge_attention_through(&last_read);
            conversation.extra.insert(
                "last_read".to_string(),
                serde_json::Value::String(last_read.clone()),
            );
            conversation.set_local_read_ts(&last_read);
        })
        .await
    }

    pub async fn clear_conversation_unread_state(
        &self,
        channel_id: &str,
        last_read: &str,
    ) -> Result<bool> {
        self.advance_conversation_read_cursor(channel_id, last_read)
            .await
    }

    #[cfg(test)]
    pub async fn mark_conversation_unread_from_event(
        &self,
        channel_id: &str,
        message_ts: &str,
    ) -> Result<bool> {
        self.observe_conversation_attention_from_event(channel_id, message_ts, true)
            .await
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn observe_conversation_attention_from_event(
        &self,
        channel_id: &str,
        message_ts: &str,
        record_unread: bool,
    ) -> Result<bool> {
        if channel_id.trim().is_empty() || message_ts.trim().is_empty() {
            return Ok(false);
        }

        let channel_id = channel_id.to_string();
        let inserted_channel_id = channel_id.clone();
        let message_ts = message_ts.to_string();
        self.mutate_conversation_row(&channel_id, move |conversation| {
            let mut conversation = conversation.unwrap_or_else(|| SlackConversation {
                id: inserted_channel_id,
                ..Default::default()
            });
            if conversation
                .local_read_ts()
                .is_some_and(|last_read| !slack_timestamp_is_after(message_ts.as_str(), last_read))
            {
                return ConversationRowMutation::Unchanged(false);
            }
            if !conversation.observe_attention_message_at(&message_ts, record_unread) {
                return ConversationRowMutation::Unchanged(false);
            }
            ConversationRowMutation::Upsert(conversation, true)
        })
        .await
    }

    pub async fn observe_conversation_attention_batch(
        &self,
        channel_id: &str,
        observations: Vec<(String, bool)>,
    ) -> Result<Vec<String>> {
        if channel_id.trim().is_empty() {
            return Ok(Vec::new());
        }

        let observations = observations
            .into_iter()
            .filter(|(message_ts, _)| !message_ts.trim().is_empty())
            .map(
                |(message_ts, record_unread)| ConversationAttentionObservation {
                    message_ts,
                    record_unread,
                },
            )
            .collect::<Vec<_>>();
        if observations.is_empty() {
            return Ok(Vec::new());
        }

        let _guard = self.update_lock.lock().await;
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let channel_id = channel_id.to_string();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let (changed, accepted) = apply_store_attention_observations(
                    &transaction,
                    &workspace_key,
                    &workspace_id,
                    &channel_id,
                    &observations,
                )?;
                if accepted.is_empty() {
                    transaction.rollback()?;
                    return Ok(accepted);
                }
                finish_sqlite_transaction(transaction, changed)?;
                Ok(accepted)
            })
            .await
    }

    /// Atomically records a classified message and, when requested, claims its
    /// native-notification identity. This keeps a restart between the two
    /// writes from turning one realtime delivery into divergent state.
    pub async fn accept_attention_delivery(
        &self,
        channel_id: &str,
        message_ts: &str,
        record_unread: bool,
        claim_notification: bool,
    ) -> Result<AttentionDeliveryOutcome> {
        if channel_id.trim().is_empty() || message_ts.trim().is_empty() {
            return Ok(AttentionDeliveryOutcome {
                observation: AttentionObservationStatus::InvalidIdentity,
                notification_claimed: false,
            });
        }

        let _guard = self.update_lock.lock().await;
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let channel_id = channel_id.to_string();
        let message_ts = message_ts.to_string();
        self.hub()
            .await?
            .write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut conversation =
                    load_sqlite_conversation(&transaction, &workspace_key, &channel_id)?
                        .unwrap_or_else(|| SlackConversation {
                            id: channel_id.clone(),
                            ..Default::default()
                        });
                if conversation.local_read_ts().is_some_and(|last_read| {
                    !slack_timestamp_is_after(message_ts.as_str(), last_read)
                }) {
                    transaction.rollback()?;
                    return Ok(AttentionDeliveryOutcome {
                        observation: AttentionObservationStatus::AtOrBeforeReadCursor,
                        notification_claimed: false,
                    });
                }
                if !conversation.observe_attention_message_at(&message_ts, record_unread) {
                    transaction.rollback()?;
                    return Ok(AttentionDeliveryOutcome {
                        observation: AttentionObservationStatus::AlreadyObserved,
                        notification_claimed: false,
                    });
                }

                let mut changed = upsert_sqlite_conversation(
                    &transaction,
                    &workspace_key,
                    &workspace_id,
                    &conversation,
                )?;
                let mut notification_claimed = false;
                if claim_notification {
                    let identity = attention_delivery_identity(&channel_id, &message_ts)
                        .expect("validated attention identity");
                    let mut ledger = load_sqlite_item::<Vec<String>>(
                        &transaction,
                        &workspace_key,
                        ATTENTION_DELIVERY_KIND,
                        ATTENTION_DELIVERY_LEDGER_KEY,
                    )?
                    .unwrap_or_default();
                    if !ledger.iter().any(|known| known == &identity) {
                        ledger.push(identity);
                        if ledger.len() > MAX_ATTENTION_DELIVERIES {
                            ledger.drain(..ledger.len() - MAX_ATTENTION_DELIVERIES);
                        }
                        changed |= upsert_sqlite_item(
                            &transaction,
                            &workspace_key,
                            ATTENTION_DELIVERY_KIND,
                            ATTENTION_DELIVERY_LEDGER_KEY,
                            &ledger,
                        )?;
                        notification_claimed = true;
                    }
                }
                finish_sqlite_transaction(transaction, changed)?;
                Ok(AttentionDeliveryOutcome {
                    observation: AttentionObservationStatus::Accepted,
                    notification_claimed,
                })
            })
            .await
    }

    /// Atomically claims a notification identity before native delivery.
    /// `false` means this workspace has already delivered the same message.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn claim_attention_delivery(
        &self,
        channel_id: &str,
        message_ts: &str,
    ) -> Result<bool> {
        let Some(identity) = attention_delivery_identity(channel_id, message_ts) else {
            return Ok(false);
        };
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
                    return Ok(false);
                }
                ledger.push(identity);
                if ledger.len() > MAX_ATTENTION_DELIVERIES {
                    ledger.drain(..ledger.len() - MAX_ATTENTION_DELIVERIES);
                }
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                changed |= upsert_sqlite_item(
                    &transaction,
                    &workspace_key,
                    ATTENTION_DELIVERY_KIND,
                    ATTENTION_DELIVERY_LEDGER_KEY,
                    &ledger,
                )?;
                finish_sqlite_transaction(transaction, changed)?;
                Ok(true)
            })
            .await
    }

    /// Removes one cached conversation without disturbing other catalog data.
    #[allow(dead_code)]
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

    pub async fn store_user_name(&self, user_id: &str, display_name: &str) -> Result<()> {
        let mut names = HashMap::new();
        names.insert(user_id.to_string(), display_name.to_string());
        self.store_user_names(&names).await
    }

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

    pub async fn mark_thread_read(
        &self,
        channel_id: &str,
        root_ts: &str,
        last_read: &str,
    ) -> Result<Vec<String>> {
        let channel_id = channel_id.to_string();
        let root_ts = root_ts.to_string();
        let last_read = last_read.to_string();
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
                let cleared_reply_ts = catalog.mark_read(&channel_id, &root_ts, &last_read);
                let records = catalog.into_records();
                changed |= sync_sqlite_kind(
                    &transaction,
                    &workspace_key,
                    "thread_record",
                    records.into_iter().map(|record| {
                        (
                            thread_key(&record.key.channel_id, &record.key.root_ts),
                            record,
                        )
                    }),
                )?;
                if !cleared_reply_ts.is_empty() {
                    if let Some(mut conversation) =
                        load_sqlite_conversation(&transaction, &workspace_key, &channel_id)?
                    {
                        conversation.acknowledge_attention_messages(&cleared_reply_ts);
                        changed |= upsert_sqlite_conversation(
                            &transaction,
                            &workspace_key,
                            &workspace_id,
                            &conversation,
                        )?;
                    }
                }
                finish_sqlite_transaction(transaction, changed)?;
                Ok(cleared_reply_ts)
            })
            .await
    }

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
            ConversationRowMutation::Upsert(conversation, true)
        })
        .await
    }

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

fn normalized_pending_unread_queue(channel_ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    channel_ids
        .into_iter()
        .filter(|channel_id| !channel_id.trim().is_empty())
        .filter(|channel_id| seen.insert(channel_id.clone()))
        .collect()
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
    pending_unread_refresh: Vec<String>,
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
    let mut connection = open_database(directory)?;
    migrate_legacy_active_workspace(&mut connection, directory)?;
    let workspace_key = connection
        .query_row(
            "SELECT active_workspace_key FROM app_state WHERE singleton = 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let Some(workspace_key) = workspace_key else {
        return Ok(None);
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
    }))
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
        values.insert(
            key,
            serde_json::from_str(&payload)
                .with_context(|| format!("invalid cached {kind} item"))?,
        );
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
                .with_context(|| format!("invalid cached {kind} item"))
                .map_err(StoreError::from)
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
                .context("invalid cached conversation")
        })
        .transpose()
        .map_err(StoreError::from)?;
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
            "conversation" => state
                .conversations
                .push(serde_json::from_str(&payload).context("invalid cached conversation")?),
            "user_name" => {
                state.user_names.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user name")?,
                );
            }
            "user_full_name" => {
                state.user_full_names.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user full name")?,
                );
            }
            "user_aliases" => {
                state.user_search_aliases.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user aliases")?,
                );
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
                            channel_timeline_messages(normalize_cached_messages(messages)),
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
            )?;
            changed |= sync_thread_records(transaction, workspace_key, data.threads)?;
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
        StoreChange::ConversationAttentionObserved {
            channel_id,
            observations,
        } => apply_store_attention_observations(
            transaction,
            workspace_key,
            workspace_id,
            &channel_id,
            &observations,
        )
        .map(|(changed, _)| changed),
        StoreChange::ConversationRemoved { channel_id } => {
            require_store_key("conversation", &channel_id)?;
            Ok(transaction.execute(
                "DELETE FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = ?2",
                params![workspace_key, channel_id],
            )? > 0)
        }
        StoreChange::UnreadChanged { snapshot } => {
            apply_store_unread_snapshot(transaction, workspace_key, workspace_id, snapshot)
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
                &channel_timeline_messages(normalize_cached_messages(messages)),
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
                &pruned_history(normalize_cached_messages(messages)),
            )
        }
        StoreChange::ThreadCatalogReplaced(records) => {
            sync_thread_records(transaction, workspace_key, records)
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
            .context("invalid cached thread_replies item")?;
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
    let existing_last_read = existing
        .as_ref()
        .and_then(|conversation| conversation.extra.get("last_read"))
        .cloned();
    let existing_local_read = existing
        .as_ref()
        .and_then(|conversation| conversation.local_read_ts())
        .map(str::to_string);
    let mut catalog = ConversationCatalog::from_cached(existing);
    catalog.upsert_metadata(conversation);
    let mut conversation = catalog
        .conversations()
        .into_iter()
        .next()
        .expect("metadata upsert should produce one conversation");
    if let Some(last_read) = existing_last_read {
        conversation
            .extra
            .insert("last_read".to_string(), last_read);
    }
    if let Some(local_read) = existing_local_read {
        conversation.set_local_read_ts(&local_read);
    }
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

fn apply_store_attention_observations(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    workspace_id: &str,
    channel_id: &str,
    observations: &[ConversationAttentionObservation],
) -> Result<(bool, Vec<String>)> {
    require_store_key("conversation attention", channel_id)?;
    if observations.is_empty() {
        return Err(StoreError::rejected_update(
            "conversation attention observations must not be empty",
        ));
    }
    if observations
        .iter()
        .any(|observation| observation.message_ts.trim().is_empty())
    {
        return Err(StoreError::rejected_update(
            "conversation attention message timestamp must not be empty",
        ));
    }

    let mut conversation = load_sqlite_conversation(transaction, workspace_key, channel_id)?
        .unwrap_or_else(|| SlackConversation {
            id: channel_id.to_string(),
            ..Default::default()
        });
    let local_read = conversation.local_read_ts().map(str::to_string);
    let mut accepted = Vec::new();
    for observation in observations {
        if local_read
            .as_deref()
            .is_some_and(|last_read| !slack_timestamp_is_after(&observation.message_ts, last_read))
            || !conversation
                .observe_attention_message_at(&observation.message_ts, observation.record_unread)
        {
            continue;
        }
        accepted.push(observation.message_ts.clone());
    }
    if accepted.is_empty() {
        return Ok((false, accepted));
    }
    let changed =
        upsert_sqlite_conversation(transaction, workspace_key, workspace_id, &conversation)?;
    Ok((changed, accepted))
}

fn apply_store_unread_snapshot(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    workspace_id: &str,
    snapshot: SlackConversationUnreadSnapshot,
) -> Result<bool> {
    require_store_key("conversation unread", &snapshot.channel_id)?;
    if !snapshot.unread_state.known {
        return Err(StoreError::rejected_update(
            "conversation unread state must be known",
        ));
    }

    let mut conversation =
        load_sqlite_conversation(transaction, workspace_key, &snapshot.channel_id)?.unwrap_or_else(
            || SlackConversation {
                id: snapshot.channel_id.clone(),
                ..Default::default()
            },
        );
    if conversation.unread_snapshot_rewinds_read(&snapshot) {
        return Ok(false);
    }
    conversation.clear_local_read_ts();
    conversation.apply_unread_snapshot(&snapshot);
    upsert_sqlite_conversation(transaction, workspace_key, workspace_id, &conversation)
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
    if !state.pending_unread_refresh.is_empty() {
        insert_state_item(
            &mut items,
            "pending_unread",
            PENDING_UNREAD_QUEUE_KEY.to_string(),
            &normalized_pending_unread_queue(state.pending_unread_refresh.iter().cloned()),
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
    let candidate = if let Some(marked) = marked {
        Some(marked)
    } else {
        let mut candidates = legacy_states(directory)?;
        (candidates.len() == 1).then(|| candidates.remove(0))
    };
    if let Some((workspace_key, state)) = candidate {
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
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::workspace_pipeline::{
        MessageMutationKind, MutationOrigin, StoreBatch, StoreChange, WorkspaceBootstrapData,
        WorkspaceCoordinator, WorkspaceMutation, WorkspaceRevision,
    };

    #[test]
    fn store_errors_classify_recovery_relevant_failures() {
        let rejected = StoreError::rejected_update("empty membership snapshot");
        let schema = StoreError::incompatible_schema(2, 1);
        let corrupt = StoreError::from(serde_json::from_str::<serde_json::Value>("{").unwrap_err());
        let local_io = StoreError::from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cache is read-only",
        ));

        assert_eq!(rejected.category(), StoreErrorCategory::RejectedUpdate);
        assert_eq!(schema.category(), StoreErrorCategory::IncompatibleSchema);
        assert_eq!(corrupt.category(), StoreErrorCategory::CorruptData);
        assert_eq!(local_io.category(), StoreErrorCategory::LocalIo);
    }

    #[test]
    fn store_errors_preserve_database_sources() {
        let error = StoreError::from(rusqlite::Error::InvalidQuery);

        assert_eq!(error.category(), StoreErrorCategory::Unexpected);
        assert!(matches!(
            error,
            StoreError::Database(rusqlite::Error::InvalidQuery)
        ));
    }

    fn temp_cache_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("conduit-{name}-{}-{unique}", std::process::id()))
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime")
    }

    #[test]
    fn coordinator_store_batches_commit_in_revision_order_and_suppress_replays() {
        let directory = temp_cache_dir("coordinator-store-order");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let first_revision = WorkspaceRevision::INITIAL.successor();
            let first = StoreBatch::new(
                first_revision,
                vec![
                    StoreChange::ConversationUpsert(SlackConversation {
                        id: "C1".into(),
                        name: Some("general".into()),
                        ..Default::default()
                    }),
                    StoreChange::ConversationUpsert(SlackConversation {
                        id: "C2".into(),
                        name: Some("random".into()),
                        ..Default::default()
                    }),
                ],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(first.clone()).await.unwrap(),
                StoreBatchExecution::Committed
            );
            let after_first = store.hub().await.unwrap().metrics();

            assert_eq!(
                store.execute_store_batch(first).await.unwrap(),
                StoreBatchExecution::SkippedStale
            );
            let after_replay = store.hub().await.unwrap().metrics();
            assert_eq!(after_replay.transactions, after_first.transactions);

            let second_revision = first_revision.successor();
            let unchanged = StoreBatch::new(
                second_revision,
                vec![StoreChange::ConversationUpsert(SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                })],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(unchanged).await.unwrap(),
                StoreBatchExecution::Unchanged
            );
            let after_unchanged = store.hub().await.unwrap().metrics();
            assert_eq!(after_unchanged.transactions, after_replay.transactions);

            let stale = StoreBatch::new(
                second_revision,
                vec![StoreChange::ConversationRemoved {
                    channel_id: "C1".into(),
                }],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(stale).await.unwrap(),
                StoreBatchExecution::SkippedStale
            );
            let delayed_revision = second_revision.successor();
            let forward_revision = delayed_revision.successor();
            let forward = StoreBatch::new(
                forward_revision,
                vec![StoreChange::ConversationUpsert(SlackConversation {
                    id: "C4".into(),
                    name: Some("forward".into()),
                    ..Default::default()
                })],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(forward).await.unwrap(),
                StoreBatchExecution::Committed
            );
            let delayed = StoreBatch::new(
                delayed_revision,
                vec![StoreChange::ConversationRemoved {
                    channel_id: "C2".into(),
                }],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(delayed).await.unwrap(),
                StoreBatchExecution::SkippedStale
            );
            let conversations = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(
                conversations
                    .iter()
                    .map(|conversation| conversation.id.as_str())
                    .collect::<HashSet<_>>(),
                HashSet::from(["C1", "C2", "C4"])
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_store_repair_replaces_a_partial_current_revision() {
        let directory = temp_cache_dir("coordinator-store-repair");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let revision = WorkspaceRevision::INITIAL.successor();
            let partial = StoreBatch::new(
                revision,
                vec![StoreChange::ConversationUpsert(SlackConversation {
                    id: "C2".into(),
                    ..Default::default()
                })],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(partial).await.unwrap(),
                StoreBatchExecution::Committed
            );

            let repair = StoreBatch::new(
                revision,
                vec![StoreChange::ConversationsRepaired(vec![
                    SlackConversation {
                        id: "C1".into(),
                        ..Default::default()
                    },
                    SlackConversation {
                        id: "C2".into(),
                        ..Default::default()
                    },
                ])],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_repair_batch(repair).await.unwrap(),
                StoreBatchExecution::Committed
            );
            assert_eq!(
                store
                    .load_conversations()
                    .await
                    .unwrap()
                    .unwrap()
                    .into_iter()
                    .map(|conversation| conversation.id)
                    .collect::<HashSet<_>>(),
                HashSet::from(["C1".to_string(), "C2".to_string()])
            );

            let stale = StoreBatch::new(
                revision,
                vec![StoreChange::ConversationRemoved {
                    channel_id: "C1".into(),
                }],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(stale).await.unwrap(),
                StoreBatchExecution::SkippedStale
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_store_repair_preserves_a_read_queued_ahead_of_its_transaction() {
        let directory = temp_cache_dir("coordinator-store-repair-read-race");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let revision = WorkspaceRevision::INITIAL.successor();
            let stale = SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                unread_count: Some(5),
                extra: HashMap::from([
                    ("has_unreads".into(), serde_json::json!(true)),
                    ("last_read".into(), serde_json::json!("1.000")),
                ]),
                ..Default::default()
            };
            store
                .execute_store_batch(
                    StoreBatch::new(
                        revision,
                        vec![StoreChange::ConversationsReplaced(vec![stale.clone()])],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            let repair = StoreBatch::new(
                revision,
                vec![StoreChange::ConversationsRepaired(vec![stale])],
            )
            .unwrap();

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

            let legacy_read = store.advance_conversation_read_cursor("C1", "20.000");
            tokio::pin!(legacy_read);
            assert!(matches!(
                futures_util::poll!(&mut legacy_read),
                std::task::Poll::Pending
            ));
            let repair_write = store.execute_store_repair_batch(repair);
            tokio::pin!(repair_write);
            assert!(matches!(
                futures_util::poll!(&mut repair_write),
                std::task::Poll::Pending
            ));

            release_writer.send(()).unwrap();
            assert!(legacy_read.await.unwrap());
            repair_write.await.unwrap();
            blocker.await.unwrap();

            let stored = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(stored[0].last_read_ts(), Some("20.000"));
            assert_eq!(stored[0].unread_activity_count(), 0);
            assert!(!stored[0].has_unread_activity());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_store_batch_failure_rolls_back_data_and_revision() {
        let directory = temp_cache_dir("coordinator-store-rollback");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let baseline = store.hub().await.unwrap().metrics();
            let revision = WorkspaceRevision::INITIAL.successor();
            let invalid = StoreBatch::new(
                revision,
                vec![
                    StoreChange::ConversationUpsert(SlackConversation {
                        id: "C1".into(),
                        name: Some("must-roll-back".into()),
                        ..Default::default()
                    }),
                    StoreChange::ConversationRemoved {
                        channel_id: String::new(),
                    },
                ],
            )
            .unwrap();
            assert!(store.execute_store_batch(invalid).await.is_err());
            let after_failure = store.hub().await.unwrap().metrics();
            assert_eq!(after_failure.transactions, baseline.transactions);
            assert_eq!(
                after_failure.rolled_back_batches,
                baseline.rolled_back_batches + 1
            );
            assert!(store.load_conversations().await.unwrap().is_none());

            let retry = StoreBatch::new(
                revision,
                vec![StoreChange::ConversationUpsert(SlackConversation {
                    id: "C1".into(),
                    name: Some("retry-commits".into()),
                    ..Default::default()
                })],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(retry).await.unwrap(),
                StoreBatchExecution::Committed
            );
            let conversations = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(conversations.len(), 1);
            assert_eq!(conversations[0].name.as_deref(), Some("retry-commits"));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn conversation_refresh_store_batch_rolls_back_and_recovers_as_one_unit() {
        let directory = temp_cache_dir("conversation-refresh-store-rollback");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let first_revision = WorkspaceRevision::INITIAL.successor();
            store
                .execute_store_batch(
                    StoreBatch::new(
                        first_revision,
                        vec![StoreChange::ConversationUpsert(SlackConversation {
                            id: "C1".into(),
                            name: Some("old".into()),
                            is_starred: Some(true),
                            unread_count: Some(0),
                            extra: HashMap::from([("last_read".into(), serde_json::json!("1.0"))]),
                            ..Default::default()
                        })],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();

            let revision = first_revision.successor();
            let refresh_changes = vec![
                StoreChange::ConversationMetadataUpsert(SlackConversation {
                    id: "C1".into(),
                    name: Some("renamed".into()),
                    is_starred: Some(false),
                    ..Default::default()
                }),
                StoreChange::UnreadChanged {
                    snapshot: SlackConversationUnreadSnapshot {
                        channel_id: "C1".into(),
                        unread_state: SlackUnreadState::from_parts(true, true, 3),
                        last_read: Some("2.0".into()),
                        latest: Some("3.0".into()),
                        ..Default::default()
                    },
                },
            ];
            let mut failing_changes = refresh_changes.clone();
            failing_changes.push(StoreChange::ConversationRemoved {
                channel_id: String::new(),
            });
            assert!(store
                .execute_store_batch(StoreBatch::new(revision, failing_changes).unwrap())
                .await
                .is_err());

            let rolled_back = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(rolled_back[0].name.as_deref(), Some("old"));
            assert_eq!(rolled_back[0].is_starred, Some(true));
            assert_eq!(rolled_back[0].unread_activity_count(), 0);
            assert_eq!(rolled_back[0].last_read_ts(), Some("1.0"));

            assert_eq!(
                store
                    .execute_store_batch(StoreBatch::new(revision, refresh_changes).unwrap())
                    .await
                    .unwrap(),
                StoreBatchExecution::Committed
            );
            let recovered = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(recovered[0].name.as_deref(), Some("renamed"));
            assert_eq!(recovered[0].is_starred, Some(true));
            assert_eq!(recovered[0].unread_activity_count(), 3);
            assert_eq!(recovered[0].last_read_ts(), Some("2.0"));
            assert_eq!(recovered[0].latest_message_ts(), Some("3.0"));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn conversation_attention_store_changes_are_idempotent_and_local_read_safe() {
        let directory = temp_cache_dir("conversation-attention-semantic-store-change");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let mut revision = WorkspaceRevision::INITIAL.successor();
            store
                .execute_store_batch(
                    StoreBatch::new(
                        revision,
                        vec![StoreChange::ConversationUpsert(SlackConversation {
                            id: "C1".into(),
                            name: Some("general".into()),
                            is_starred: Some(true),
                            unread_count: Some(5),
                            extra: HashMap::from([
                                ("has_unreads".into(), serde_json::json!(true)),
                                ("last_read".into(), serde_json::json!("10.0")),
                                ("topic".into(), serde_json::json!("Keep me")),
                            ]),
                            ..Default::default()
                        })],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();

            revision = revision.successor();
            let observation = StoreChange::ConversationAttentionObserved {
                channel_id: "C1".into(),
                observations: vec![ConversationAttentionObservation {
                    message_ts: "11.0".into(),
                    record_unread: true,
                }],
            };
            assert_eq!(
                store
                    .execute_store_batch(
                        StoreBatch::new(revision, vec![observation.clone()]).unwrap()
                    )
                    .await
                    .unwrap(),
                StoreBatchExecution::Committed
            );
            revision = revision.successor();
            assert_eq!(
                store
                    .execute_store_batch(StoreBatch::new(revision, vec![observation]).unwrap())
                    .await
                    .unwrap(),
                StoreBatchExecution::Unchanged
            );
            let after_duplicate = store.load_conversations().await.unwrap().unwrap();
            let after_duplicate = &after_duplicate[0];
            assert_eq!(after_duplicate.unread_activity_count(), 1);
            assert_eq!(after_duplicate.raw_unread_activity_count(), 5);
            assert!(after_duplicate.is_starred());
            assert_eq!(after_duplicate.name.as_deref(), Some("general"));
            assert_eq!(
                after_duplicate.extra.get("topic"),
                Some(&serde_json::json!("Keep me"))
            );

            store
                .clear_conversation_unread_state("C1", "20.0")
                .await
                .unwrap();
            revision = revision.successor();
            assert_eq!(
                store
                    .execute_store_batch(
                        StoreBatch::new(
                            revision,
                            vec![StoreChange::ConversationAttentionObserved {
                                channel_id: "C1".into(),
                                observations: vec![ConversationAttentionObservation {
                                    message_ts: "19.0".into(),
                                    record_unread: true,
                                }],
                            }],
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap(),
                StoreBatchExecution::Unchanged
            );
            let after_stale = store.load_conversations().await.unwrap().unwrap();
            let after_stale = &after_stale[0];
            assert_eq!(after_stale.unread_activity_count(), 0);
            assert_eq!(after_stale.raw_unread_activity_count(), 0);
            assert_eq!(after_stale.last_read_ts(), Some("20.0"));
            assert_eq!(after_stale.local_read_ts(), Some("20.0"));
            assert!(after_stale.is_starred());
            assert_eq!(after_stale.name.as_deref(), Some("general"));
            assert_eq!(
                after_stale.extra.get("topic"),
                Some(&serde_json::json!("Keep me"))
            );

            revision = revision.successor();
            assert_eq!(
                store
                    .execute_store_batch(
                        StoreBatch::new(
                            revision,
                            vec![StoreChange::ConversationAttentionObserved {
                                channel_id: "C1".into(),
                                observations: vec![ConversationAttentionObservation {
                                    message_ts: "21.0".into(),
                                    record_unread: true,
                                }],
                            }],
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap(),
                StoreBatchExecution::Committed
            );
            let after_new = store.load_conversations().await.unwrap().unwrap();
            let after_new = &after_new[0];
            assert_eq!(after_new.unread_activity_count(), 1);
            assert_eq!(after_new.raw_unread_activity_count(), 0);
            assert_eq!(after_new.last_read_ts(), Some("20.0"));
            assert_eq!(after_new.local_read_ts(), Some("20.0"));
            assert!(after_new.is_starred());
            assert_eq!(after_new.name.as_deref(), Some("general"));

            store
                .clear_conversation_unread_state("C1", "20.0")
                .await
                .unwrap();
            let after_partial_read = store.load_conversations().await.unwrap().unwrap();
            let after_partial_read = &after_partial_read[0];
            assert_eq!(
                after_partial_read.unread_activity_count(),
                1,
                "a read cursor must preserve semantic unread observations after it"
            );
            assert_eq!(after_partial_read.raw_unread_activity_count(), 0);
            assert_eq!(after_partial_read.last_read_ts(), Some("20.0"));
            assert_eq!(after_partial_read.local_read_ts(), Some("20.0"));

            revision = revision.successor();
            assert!(store
                .execute_store_batch(
                    StoreBatch::new(
                        revision,
                        vec![
                            StoreChange::HistoryReplaced {
                                channel_id: "C1".into(),
                                messages: vec![SlackMessage {
                                    ts: "22.0".into(),
                                    text: Some("must roll back".into()),
                                    ..Default::default()
                                }],
                            },
                            StoreChange::ConversationAttentionObserved {
                                channel_id: "C1".into(),
                                observations: vec![ConversationAttentionObservation {
                                    message_ts: " ".into(),
                                    record_unread: true,
                                }],
                            },
                        ],
                    )
                    .unwrap(),
                )
                .await
                .is_err());
            assert!(store.load_history("C1").await.unwrap().is_none());

            assert!(store
                .execute_store_batch(
                    StoreBatch::new(
                        revision,
                        vec![
                            StoreChange::HistoryReplaced {
                                channel_id: "C1".into(),
                                messages: vec![SlackMessage {
                                    ts: "23.0".into(),
                                    text: Some("must also roll back".into()),
                                    ..Default::default()
                                }],
                            },
                            StoreChange::ConversationAttentionObserved {
                                channel_id: " ".into(),
                                observations: vec![ConversationAttentionObservation {
                                    message_ts: "23.0".into(),
                                    record_unread: true,
                                }],
                            },
                        ],
                    )
                    .unwrap(),
                )
                .await
                .is_err());
            assert!(store.load_history("C1").await.unwrap().is_none());

            assert!(store
                .execute_store_batch(
                    StoreBatch::new(
                        revision,
                        vec![
                            StoreChange::HistoryReplaced {
                                channel_id: "C1".into(),
                                messages: vec![SlackMessage {
                                    ts: "24.0".into(),
                                    text: Some("empty observations must roll back".into()),
                                    ..Default::default()
                                }],
                            },
                            StoreChange::ConversationAttentionObserved {
                                channel_id: "C1".into(),
                                observations: Vec::new(),
                            },
                        ],
                    )
                    .unwrap(),
                )
                .await
                .is_err());
            assert!(store.load_history("C1").await.unwrap().is_none());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_message_deltas_merge_with_unhydrated_sqlite_timelines() {
        let directory = temp_cache_dir("coordinator-message-delta-merge");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let first_revision = WorkspaceRevision::INITIAL.successor();
            let mut optimistic = SlackMessage {
                ts: "3.0".into(),
                text: Some("optimistic".into()),
                client_msg_id: Some("client-1".into()),
                ..Default::default()
            };
            optimistic.refresh_canonical_content();
            let root = SlackMessage {
                ts: "1.0".into(),
                text: Some("root".into()),
                ..Default::default()
            };
            let older = SlackMessage {
                ts: "2.0".into(),
                text: Some("older".into()),
                ..Default::default()
            };
            let older_reply = SlackMessage {
                ts: "1.1".into(),
                thread_ts: Some("1.0".into()),
                text: Some("older reply".into()),
                ..Default::default()
            };
            let cached_broadcast = SlackMessage {
                ts: "1.4".into(),
                thread_ts: Some("1.0".into()),
                subtype: Some("thread_broadcast".into()),
                client_msg_id: Some("projection-1".into()),
                text: Some("cached broadcast".into()),
                ..Default::default()
            };
            store
                .execute_store_batch(
                    StoreBatch::new(
                        first_revision,
                        vec![
                            StoreChange::HistoryReplaced {
                                channel_id: "C1".into(),
                                messages: vec![
                                    root.clone(),
                                    older.clone(),
                                    optimistic,
                                    cached_broadcast,
                                ],
                            },
                            StoreChange::ThreadReplaced {
                                channel_id: "C1".into(),
                                thread_ts: "1.0".into(),
                                messages: vec![root.clone(), older_reply.clone()],
                            },
                        ],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();

            let mut authoritative = SlackMessage {
                ts: "4.0".into(),
                text: Some("authoritative".into()),
                client_msg_id: Some("client-1".into()),
                ..Default::default()
            };
            authoritative.refresh_canonical_content();
            let new_reply = SlackMessage {
                ts: "1.2".into(),
                thread_ts: Some("1.0".into()),
                text: Some("new reply".into()),
                ..Default::default()
            };
            let mut normal_reply_in_channel = new_reply.clone();
            normal_reply_in_channel.text = Some("must be filtered".into());
            normal_reply_in_channel.client_msg_id = Some("projection-1".into());
            let mut broadcast = new_reply.clone();
            broadcast.ts = "1.3".into();
            broadcast.subtype = Some("thread_broadcast".into());
            broadcast.text = Some("broadcast".into());
            let mutations = [
                (authoritative, MessageMutationKind::Changed),
                (normal_reply_in_channel, MessageMutationKind::Changed),
                (broadcast, MessageMutationKind::Posted),
                (
                    SlackMessage {
                        ts: "2.0".into(),
                        text: Some("edited older".into()),
                        ..Default::default()
                    },
                    MessageMutationKind::Changed,
                ),
                (older_reply.clone(), MessageMutationKind::Deleted),
            ];
            let mut revision = first_revision;
            for (message, kind) in mutations {
                revision = revision.successor();
                store
                    .execute_store_batch(
                        StoreBatch::new(
                            revision,
                            vec![StoreChange::MessageDelta {
                                channel_id: "C1".into(),
                                message,
                                kind,
                            }],
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            }

            let history = store.load_history("C1").await.unwrap().unwrap();
            assert_eq!(
                history
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["4.0", "2.0", "1.3", "1.0"]
            );
            assert_eq!(
                history
                    .iter()
                    .find(|message| message.ts == "2.0")
                    .and_then(|message| message.text.as_deref()),
                Some("edited older")
            );
            assert_eq!(
                history
                    .iter()
                    .filter(|message| message.client_msg_id.as_deref() == Some("client-1"))
                    .count(),
                1,
                "the authoritative response must replace its optimistic identity"
            );
            assert!(!history.iter().any(|message| message.ts == "1.2"));

            let thread = store.load_thread("C1", "1.0").await.unwrap().unwrap();
            assert_eq!(
                thread
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["1.3", "1.2", "1.0"]
            );
            assert!(!thread.iter().any(|message| message.ts == older_reply.ts));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_message_reductions_merge_into_unhydrated_store_timelines() {
        let directory = temp_cache_dir("coordinator-message-reduction-merge");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_history(
                    "C1",
                    &[SlackMessage {
                        ts: "1.0".into(),
                        text: Some("persisted history".into()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            store
                .store_thread(
                    "C1",
                    "10.0",
                    &[SlackMessage {
                        ts: "10.1".into(),
                        thread_ts: Some("10.0".into()),
                        text: Some("persisted reply".into()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();

            let mut coordinator = WorkspaceCoordinator::default();
            let history_reduction = coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: "2.0".into(),
                        text: Some("new history".into()),
                        ..Default::default()
                    },
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Realtime,
                })
                .unwrap();
            store
                .execute_store_batch(history_reduction.store_batch().unwrap().clone())
                .await
                .unwrap();

            let thread_reduction = coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: "10.2".into(),
                        thread_ts: Some("10.0".into()),
                        text: Some("new reply".into()),
                        ..Default::default()
                    },
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Realtime,
                })
                .unwrap();
            store
                .execute_store_batch(thread_reduction.store_batch().unwrap().clone())
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
                vec!["2.0", "1.0"]
            );
            assert_eq!(
                store
                    .load_thread("C1", "10.0")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["10.2", "10.1"]
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_same_identity_delete_removes_authoritative_unhydrated_timestamp() {
        let directory = temp_cache_dir("coordinator-message-identity-delete");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let mut coordinator = WorkspaceCoordinator::default();
            let mut optimistic = SlackMessage {
                ts: "10.0".into(),
                text: Some("optimistic".into()),
                client_msg_id: Some("client-1".into()),
                ..Default::default()
            };
            optimistic.refresh_canonical_content();
            coordinator.apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: optimistic,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Local,
            });

            let mut authoritative = SlackMessage {
                ts: "11.0".into(),
                text: Some("authoritative".into()),
                client_msg_id: Some("client-1".into()),
                ..Default::default()
            };
            authoritative.refresh_canonical_content();
            store
                .store_history("C1", std::slice::from_ref(&authoritative))
                .await
                .unwrap();

            let deletion = coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: authoritative,
                    kind: MessageMutationKind::Deleted,
                    origin: MutationOrigin::Realtime,
                })
                .unwrap();
            store
                .execute_store_batch(deletion.store_batch().unwrap().clone())
                .await
                .unwrap();

            assert!(store.load_history("C1").await.unwrap().is_none());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_projection_edit_removes_authoritative_unhydrated_identity() {
        let directory = temp_cache_dir("coordinator-message-projection-edit");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let mut authoritative = SlackMessage {
                ts: "11.0".into(),
                thread_ts: Some("1.0".into()),
                subtype: Some("thread_broadcast".into()),
                text: Some("authoritative broadcast".into()),
                client_msg_id: Some("client-1".into()),
                ..Default::default()
            };
            authoritative.refresh_canonical_content();
            store
                .store_history("C1", std::slice::from_ref(&authoritative))
                .await
                .unwrap();
            store
                .store_thread("C1", "1.0", std::slice::from_ref(&authoritative))
                .await
                .unwrap();

            let mut coordinator = WorkspaceCoordinator::default();
            authoritative.subtype = None;
            authoritative.text = Some("normal reply".into());
            let edit = coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: authoritative,
                    kind: MessageMutationKind::Changed,
                    origin: MutationOrigin::Realtime,
                })
                .unwrap();
            store
                .execute_store_batch(edit.store_batch().unwrap().clone())
                .await
                .unwrap();

            assert!(store.load_history("C1").await.unwrap().is_none());
            let thread = store.load_thread("C1", "1.0").await.unwrap().unwrap();
            assert_eq!(thread.len(), 1);
            assert_eq!(thread[0].ts, "11.0");
            assert_eq!(thread[0].text.as_deref(), Some("normal reply"));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn sqlite_only_reply_deletes_reconcile_both_root_copies_without_drift() {
        let directory = temp_cache_dir("coordinator-message-root-delete");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let root = |ts: &str, latest_reply: &str| SlackMessage {
                ts: ts.into(),
                text: Some("root".into()),
                reply_count: Some(2),
                latest_reply: Some(latest_reply.into()),
                reply_users: Some(vec!["U1".into(), "U2".into()]),
                ..Default::default()
            };
            let reply = |ts: &str, root_ts: &str, user: &str| SlackMessage {
                ts: ts.into(),
                thread_ts: Some(root_ts.into()),
                user: Some(user.into()),
                text: Some("reply".into()),
                ..Default::default()
            };
            let root_one = root("10.0", "12.0");
            let root_two = root("20.0", "22.0");
            let reply_11 = reply("11.0", "10.0", "U1");
            let reply_12 = reply("12.0", "10.0", "U2");
            let reply_21 = reply("21.0", "20.0", "U1");
            let reply_22 = reply("22.0", "20.0", "U2");
            store
                .store_history("C1", &[root_one.clone(), root_two.clone()])
                .await
                .unwrap();
            store
                .store_thread(
                    "C1",
                    "10.0",
                    &[root_one.clone(), reply_11.clone(), reply_12],
                )
                .await
                .unwrap();
            store
                .store_thread(
                    "C1",
                    "20.0",
                    &[root_two.clone(), reply_21.clone(), reply_22.clone()],
                )
                .await
                .unwrap();

            let mut coordinator = WorkspaceCoordinator::default();
            for deleted in [reply_11.clone(), reply_22.clone()] {
                let reduction = coordinator
                    .apply(WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: deleted,
                        kind: MessageMutationKind::Deleted,
                        origin: MutationOrigin::Realtime,
                    })
                    .unwrap();
                store
                    .execute_store_batch(reduction.store_batch().unwrap().clone())
                    .await
                    .unwrap();
            }
            store
                .execute_store_batch(
                    StoreBatch::new(
                        coordinator.revision().successor(),
                        vec![StoreChange::MessageDelta {
                            channel_id: "C1".into(),
                            message: reply_22,
                            kind: MessageMutationKind::Deleted,
                        }],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();

            let history = store.load_history("C1").await.unwrap().unwrap();
            let thread_one = store.load_thread("C1", "10.0").await.unwrap().unwrap();
            let thread_two = store.load_thread("C1", "20.0").await.unwrap().unwrap();
            for root in [
                history.iter().find(|message| message.ts == "10.0").unwrap(),
                thread_one
                    .iter()
                    .find(|message| message.ts == "10.0")
                    .unwrap(),
            ] {
                assert_eq!(root.reply_count, Some(1));
                assert_eq!(root.latest_reply.as_deref(), Some("12.0"));
                assert_eq!(root.reply_users.as_deref(), Some(&["U2".to_string()][..]));
            }
            for root in [
                history.iter().find(|message| message.ts == "20.0").unwrap(),
                thread_two
                    .iter()
                    .find(|message| message.ts == "20.0")
                    .unwrap(),
            ] {
                assert_eq!(root.reply_count, Some(1));
                assert_eq!(root.latest_reply.as_deref(), Some("21.0"));
                assert_eq!(root.reply_users.as_deref(), Some(&["U1".to_string()][..]));
            }
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn partial_cached_reply_delete_preserves_users_and_unknown_delete_is_count_neutral() {
        let directory = temp_cache_dir("coordinator-message-partial-root-delete");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let root = SlackMessage {
                ts: "10.0".into(),
                reply_count: Some(3),
                latest_reply: Some("12.0".into()),
                reply_users: Some(vec!["U1".into(), "U2".into(), "U3".into()]),
                ..Default::default()
            };
            let reply = |ts: &str, user: &str| SlackMessage {
                ts: ts.into(),
                thread_ts: Some("10.0".into()),
                user: Some(user.into()),
                ..Default::default()
            };
            let persisted = reply("11.0", "U1");
            store
                .store_history("C1", std::slice::from_ref(&root))
                .await
                .unwrap();
            store
                .store_thread(
                    "C1",
                    "10.0",
                    &[root.clone(), persisted.clone(), reply("12.0", "U2")],
                )
                .await
                .unwrap();

            let first_revision = WorkspaceRevision::INITIAL.successor();
            store
                .execute_store_batch(
                    StoreBatch::new(
                        first_revision,
                        vec![StoreChange::MessageDelta {
                            channel_id: "C1".into(),
                            message: persisted,
                            kind: MessageMutationKind::Deleted,
                        }],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            store
                .execute_store_batch(
                    StoreBatch::new(
                        first_revision.successor(),
                        vec![StoreChange::MessageDelta {
                            channel_id: "C1".into(),
                            message: reply("13.0", "U4"),
                            kind: MessageMutationKind::Deleted,
                        }],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();

            let history = store.load_history("C1").await.unwrap().unwrap();
            let thread = store.load_thread("C1", "10.0").await.unwrap().unwrap();
            let history_root = history.iter().find(|message| message.ts == "10.0").unwrap();
            let thread_root = thread.iter().find(|message| message.ts == "10.0").unwrap();
            assert_eq!(history_root, thread_root);
            assert_eq!(history_root.reply_count, Some(2));
            assert_eq!(history_root.latest_reply.as_deref(), Some("12.0"));
            assert_eq!(
                history_root.reply_users.as_deref(),
                Some(&["U1".to_string(), "U2".to_string(), "U3".to_string()][..])
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn sqlite_only_post_edit_and_move_reconcile_both_root_copies() {
        let directory = temp_cache_dir("coordinator-message-root-transitions");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let first_root = SlackMessage {
                ts: "10.0".into(),
                reply_count: Some(1),
                latest_reply: Some("11.0".into()),
                reply_users: Some(vec!["U1".into()]),
                ..Default::default()
            };
            let second_root = SlackMessage {
                ts: "20.0".into(),
                reply_count: Some(0),
                reply_users: Some(Vec::new()),
                ..Default::default()
            };
            let existing = SlackMessage {
                ts: "11.0".into(),
                thread_ts: Some("10.0".into()),
                user: Some("U1".into()),
                ..Default::default()
            };
            store
                .store_history("C1", &[first_root.clone(), second_root.clone()])
                .await
                .unwrap();
            store
                .store_thread("C1", "10.0", &[first_root.clone(), existing.clone()])
                .await
                .unwrap();
            store
                .store_thread("C1", "20.0", std::slice::from_ref(&second_root))
                .await
                .unwrap();

            let mut coordinator = WorkspaceCoordinator::default();
            let mut reply = SlackMessage {
                ts: "12.0".into(),
                thread_ts: Some("10.0".into()),
                client_msg_id: Some("reply-2".into()),
                user: Some("U2".into()),
                text: Some("posted".into()),
                ..Default::default()
            };
            for kind in [
                MessageMutationKind::Posted,
                MessageMutationKind::Changed,
                MessageMutationKind::Changed,
            ] {
                if kind == MessageMutationKind::Changed && reply.user.as_deref() == Some("U2") {
                    reply.user = Some("U3".into());
                    reply.text = Some("edited".into());
                } else if kind == MessageMutationKind::Changed {
                    reply.thread_ts = Some("20.0".into());
                    reply.text = Some("moved".into());
                }
                let reduction = coordinator
                    .apply(WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: reply.clone(),
                        kind,
                        origin: MutationOrigin::Realtime,
                    })
                    .unwrap();
                store
                    .execute_store_batch(reduction.store_batch().unwrap().clone())
                    .await
                    .unwrap();
            }

            let history = store.load_history("C1").await.unwrap().unwrap();
            let first_thread = store.load_thread("C1", "10.0").await.unwrap().unwrap();
            let second_thread = store.load_thread("C1", "20.0").await.unwrap().unwrap();
            let root_copy = |messages: &[SlackMessage], root_ts: &str| {
                messages
                    .iter()
                    .find(|message| message.ts == root_ts)
                    .cloned()
                    .unwrap()
            };
            assert_eq!(
                root_copy(&history, "10.0"),
                root_copy(&first_thread, "10.0")
            );
            assert_eq!(root_copy(&history, "10.0").reply_count, Some(1));
            assert_eq!(
                root_copy(&history, "10.0").latest_reply.as_deref(),
                Some("11.0")
            );
            assert_eq!(
                root_copy(&history, "10.0").reply_users.as_deref(),
                Some(&["U1".to_string()][..])
            );
            assert_eq!(
                root_copy(&history, "20.0"),
                root_copy(&second_thread, "20.0")
            );
            assert_eq!(root_copy(&history, "20.0").reply_count, Some(1));
            assert_eq!(
                root_copy(&history, "20.0").latest_reply.as_deref(),
                Some("12.0")
            );
            assert_eq!(
                root_copy(&history, "20.0").reply_users.as_deref(),
                Some(&["U3".to_string()][..])
            );
            assert!(!first_thread.iter().any(|message| message.ts == "12.0"));
            assert_eq!(
                second_thread
                    .iter()
                    .find(|message| message.ts == "12.0")
                    .and_then(|message| message.text.as_deref()),
                Some("moved")
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn changed_thread_root_updates_its_cached_copy_without_cross_channel_scan() {
        let directory = temp_cache_dir("coordinator-message-root-edit-scope");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let root = SlackMessage {
                ts: "10.0".into(),
                thread_ts: Some("10.0".into()),
                text: Some("old root".into()),
                reply_count: Some(2),
                latest_reply: Some("12.0".into()),
                reply_users: Some(vec!["U1".into(), "U2".into()]),
                ..Default::default()
            };
            let other_root = SlackMessage {
                ts: "10.0".into(),
                thread_ts: Some("10.0".into()),
                text: Some("other channel".into()),
                ..Default::default()
            };
            store
                .store_history("C1", std::slice::from_ref(&root))
                .await
                .unwrap();
            store
                .store_thread("C1", "10.0", std::slice::from_ref(&root))
                .await
                .unwrap();
            store
                .store_thread("C10", "10.0", std::slice::from_ref(&other_root))
                .await
                .unwrap();

            let current = SlackMessage {
                ts: "10.0".into(),
                text: Some("current root".into()),
                ..Default::default()
            };
            let mut coordinator = WorkspaceCoordinator::default();
            let reduction = coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: current,
                    kind: MessageMutationKind::Changed,
                    origin: MutationOrigin::Realtime,
                })
                .unwrap();
            store
                .execute_store_batch(reduction.store_batch().unwrap().clone())
                .await
                .unwrap();

            let history = store.load_history("C1").await.unwrap().unwrap();
            let thread = store.load_thread("C1", "10.0").await.unwrap().unwrap();
            assert_eq!(history[0], thread[0]);
            assert_eq!(history[0].text.as_deref(), Some("current root"));
            assert_eq!(history[0].reply_count, Some(2));
            assert_eq!(history[0].latest_reply.as_deref(), Some("12.0"));
            assert_eq!(
                history[0].reply_users.as_deref(),
                Some(&["U1".to_string(), "U2".to_string()][..])
            );
            assert_eq!(
                store.load_thread("C10", "10.0").await.unwrap().unwrap()[0]
                    .text
                    .as_deref(),
                Some("other channel")
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn reply_aggregate_delta_preserves_projection_specific_root_content() {
        let directory = temp_cache_dir("coordinator-message-root-content");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let history_root = SlackMessage {
                ts: "10.0".into(),
                text: Some("channel snapshot".into()),
                reply_count: Some(0),
                ..Default::default()
            };
            let thread_root = SlackMessage {
                text: Some("newer thread snapshot".into()),
                ..history_root.clone()
            };
            store
                .store_history("C1", std::slice::from_ref(&history_root))
                .await
                .unwrap();
            store
                .store_thread("C1", "10.0", std::slice::from_ref(&thread_root))
                .await
                .unwrap();

            store
                .execute_store_batch(
                    StoreBatch::new(
                        WorkspaceRevision::INITIAL.successor(),
                        vec![StoreChange::MessageDelta {
                            channel_id: "C1".into(),
                            message: SlackMessage {
                                ts: "11.0".into(),
                                thread_ts: Some("10.0".into()),
                                user: Some("U1".into()),
                                ..Default::default()
                            },
                            kind: MessageMutationKind::Posted,
                        }],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();

            let history = store.load_history("C1").await.unwrap().unwrap();
            let thread = store.load_thread("C1", "10.0").await.unwrap().unwrap();
            let history_root = history.iter().find(|message| message.ts == "10.0").unwrap();
            let thread_root = thread.iter().find(|message| message.ts == "10.0").unwrap();
            assert_eq!(history_root.text.as_deref(), Some("channel snapshot"));
            assert_eq!(thread_root.text.as_deref(), Some("newer thread snapshot"));
            assert_eq!(history_root.reply_count, Some(1));
            assert_eq!(thread_root.reply_count, Some(1));
            assert_eq!(history_root.latest_reply, thread_root.latest_reply);
            assert_eq!(history_root.reply_users, thread_root.reply_users);
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn ordinary_post_skips_unrelated_same_channel_thread_payloads() {
        let directory = temp_cache_dir("coordinator-message-post-thread-scope");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_thread(
                    "C1",
                    "10.0",
                    &[SlackMessage {
                        ts: "10.0".into(),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
        });
        Connection::open(store.database_path())
            .unwrap()
            .execute(
                "UPDATE workspace_items SET payload_json = '{broken'
                 WHERE workspace_key = ?1
                   AND kind = 'thread_replies'
                   AND item_key = 'C1:10.0'",
                [&store.workspace_key],
            )
            .unwrap();

        runtime().block_on(async {
            assert_eq!(
                store
                    .execute_store_batch(
                        StoreBatch::new(
                            WorkspaceRevision::INITIAL.successor(),
                            vec![StoreChange::MessageDelta {
                                channel_id: "C1".into(),
                                message: SlackMessage {
                                    ts: "20.0".into(),
                                    text: Some("ordinary post".into()),
                                    ..Default::default()
                                },
                                kind: MessageMutationKind::Posted,
                            }],
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap(),
                StoreBatchExecution::Committed
            );
            assert_eq!(
                store.load_history("C1").await.unwrap().unwrap()[0]
                    .text
                    .as_deref(),
                Some("ordinary post")
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_message_delta_batch_rolls_back_wholly_and_can_recover() {
        let directory = temp_cache_dir("coordinator-message-delta-rollback");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let first_revision = WorkspaceRevision::INITIAL.successor();
            let root = SlackMessage {
                ts: "1.0".into(),
                text: Some("root".into()),
                ..Default::default()
            };
            store
                .execute_store_batch(
                    StoreBatch::new(
                        first_revision,
                        vec![
                            StoreChange::HistoryReplaced {
                                channel_id: "C1".into(),
                                messages: vec![root.clone()],
                            },
                            StoreChange::ThreadReplaced {
                                channel_id: "C1".into(),
                                thread_ts: "1.0".into(),
                                messages: vec![root.clone()],
                            },
                        ],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();

            let reply = SlackMessage {
                ts: "2.0".into(),
                thread_ts: Some("1.0".into()),
                text: Some("reply".into()),
                ..Default::default()
            };
            let revision = first_revision.successor();
            let deltas = vec![
                StoreChange::MessageDelta {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: "3.0".into(),
                        text: Some("new history".into()),
                        ..Default::default()
                    },
                    kind: MessageMutationKind::Posted,
                },
                StoreChange::MessageDelta {
                    channel_id: "C1".into(),
                    message: reply.clone(),
                    kind: MessageMutationKind::Posted,
                },
            ];
            let mut failing_changes = deltas.clone();
            failing_changes.push(StoreChange::ConversationRemoved {
                channel_id: String::new(),
            });
            assert!(store
                .execute_store_batch(StoreBatch::new(revision, failing_changes).unwrap())
                .await
                .is_err());
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
                vec!["1.0"]
            );

            assert_eq!(
                store
                    .execute_store_batch(StoreBatch::new(revision, deltas).unwrap())
                    .await
                    .unwrap(),
                StoreBatchExecution::Committed
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
                vec!["3.0", "1.0"]
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
                vec!["2.0", "1.0"]
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_metadata_upsert_preserves_a_newer_legacy_read_overlay() {
        let directory = temp_cache_dir("coordinator-metadata-read-overlay");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let first_revision = WorkspaceRevision::INITIAL.successor();
            let initial = StoreBatch::new(
                first_revision,
                vec![StoreChange::ConversationUpsert(SlackConversation {
                    id: "C1".into(),
                    name: Some("old-name".into()),
                    is_starred: Some(true),
                    unread_count: Some(7),
                    extra: HashMap::from([
                        ("has_unreads".into(), serde_json::json!(true)),
                        ("last_read".into(), serde_json::json!("1.000")),
                    ]),
                    ..Default::default()
                })],
            )
            .unwrap();
            store.execute_store_batch(initial).await.unwrap();

            assert!(store
                .advance_conversation_read_cursor("C1", "20.000")
                .await
                .unwrap());

            let stale_metadata = StoreBatch::new(
                first_revision.successor(),
                vec![StoreChange::ConversationMetadataUpsert(SlackConversation {
                    id: "C1".into(),
                    name: Some("new-name".into()),
                    is_starred: Some(false),
                    unread_count: Some(7),
                    extra: HashMap::from([
                        ("has_unreads".into(), serde_json::json!(true)),
                        ("last_read".into(), serde_json::json!("1.000")),
                    ]),
                    ..Default::default()
                })],
            )
            .unwrap();
            store.execute_store_batch(stale_metadata).await.unwrap();

            let stored = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(stored[0].name.as_deref(), Some("new-name"));
            assert_eq!(stored[0].is_starred, Some(true));
            assert_eq!(stored[0].last_read_ts(), Some("20.000"));
            assert_eq!(stored[0].unread_activity_count(), 0);
            assert!(!stored[0].has_unread_activity());

            let authoritative_star = StoreBatch::new(
                first_revision.successor().successor(),
                vec![StoreChange::ConversationMembershipUpsert(
                    SlackConversation {
                        id: "C1".into(),
                        is_starred: Some(false),
                        ..Default::default()
                    },
                )],
            )
            .unwrap();
            store.execute_store_batch(authoritative_star).await.unwrap();
            let stored = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(stored[0].is_starred, Some(false));
            assert_eq!(stored[0].last_read_ts(), Some("20.000"));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_membership_upsert_inserts_full_unread_state_for_a_new_row() {
        let directory = temp_cache_dir("coordinator-membership-new-unread");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let batch = StoreBatch::new(
                WorkspaceRevision::INITIAL.successor(),
                vec![StoreChange::ConversationMembershipUpsert(
                    SlackConversation {
                        id: "D1".into(),
                        is_im: Some(true),
                        unread_count: Some(3),
                        extra: HashMap::from([
                            ("has_unreads".into(), serde_json::json!(true)),
                            ("unread_count_display".into(), serde_json::json!(3)),
                            ("last_read".into(), serde_json::json!("10.000")),
                        ]),
                        ..Default::default()
                    },
                )],
            )
            .unwrap();
            store.execute_store_batch(batch).await.unwrap();

            let stored = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(stored[0].id, "D1");
            assert_eq!(stored[0].unread_activity_count(), 3);
            assert!(stored[0].has_unread_activity());
            assert_eq!(stored[0].last_read_ts(), Some("10.000"));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_user_store_changes_persist_only_safe_projections() {
        let directory = temp_cache_dir("coordinator-user-projection");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let revision = WorkspaceRevision::INITIAL.successor();
            let batch = StoreBatch::new(
                revision,
                vec![StoreChange::UsersReplaced(vec![SlackUser {
                    id: Some("U1".into()),
                    name: Some("ada".into()),
                    profile: Some(crate::models::SlackUserProfile {
                        display_name: Some("Ada".into()),
                        real_name: Some("Ada Lovelace".into()),
                        phone: Some("PrivatePhoneCanary".into()),
                        email: Some("PrivateEmailCanary".into()),
                        huddle_state_call_id: Some("PrivateHuddleCanary".into()),
                        fields: HashMap::from([(
                            "private".into(),
                            crate::models::SlackProfileField {
                                value: Some("PrivateCustomFieldCanary".into()),
                                ..Default::default()
                            },
                        )]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }])],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(batch).await.unwrap(),
                StoreBatchExecution::Committed
            );

            assert_eq!(
                store.load_user_names().await.unwrap().get("U1"),
                Some(&"Ada".to_string())
            );
            let payloads = store
                .hub()
                .await
                .unwrap()
                .query(|connection| {
                    let mut statement = connection.prepare(
                        "SELECT payload_json FROM workspace_items ORDER BY kind, item_key",
                    )?;
                    let payloads = statement
                        .query_map([], |row| row.get::<_, String>(0))?
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(payloads.join("\n"))
                })
                .await
                .unwrap();
            for private_value in [
                "PrivatePhoneCanary",
                "PrivateEmailCanary",
                "PrivateHuddleCanary",
                "PrivateCustomFieldCanary",
            ] {
                assert!(!payloads.contains(private_value));
            }
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_user_upsert_preserves_sparse_status_and_deletes_explicit_clear() {
        let directory = temp_cache_dir("coordinator-user-status-clear");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let first_revision = WorkspaceRevision::INITIAL.successor();
            let active = StoreBatch::new(
                first_revision,
                vec![StoreChange::UserUpsert(SlackUser {
                    id: Some("U1".into()),
                    profile: Some(crate::models::SlackUserProfile {
                        status_text: Some("Heads down".into()),
                        status_emoji: Some(":construction:".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(active).await.unwrap(),
                StoreBatchExecution::Committed
            );

            let sparse = StoreBatch::new(
                first_revision.successor(),
                vec![StoreChange::UserUpsert(SlackUser {
                    id: Some("U1".into()),
                    profile: Some(crate::models::SlackUserProfile {
                        huddle_state_call_id: Some("R1".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(sparse).await.unwrap(),
                StoreBatchExecution::Unchanged
            );
            assert_eq!(
                store.load_user_statuses().await.unwrap()["U1"].text,
                "Heads down"
            );

            let cleared = StoreBatch::new(
                first_revision.successor().successor(),
                vec![StoreChange::UserUpsert(SlackUser {
                    id: Some("U1".into()),
                    profile: Some(crate::models::SlackUserProfile {
                        status_text: Some(String::new()),
                        status_emoji: Some(String::new()),
                        status_expiration: Some(0),
                        ..Default::default()
                    }),
                    ..Default::default()
                })],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(cleared).await.unwrap(),
                StoreBatchExecution::Committed
            );
            assert!(!store.load_user_statuses().await.unwrap().contains_key("U1"));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_store_executor_handles_every_change_variant() {
        let directory = temp_cache_dir("coordinator-all-store-changes");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let root = SlackMessage {
                ts: "1.000".into(),
                reply_count: Some(1),
                ..Default::default()
            };
            let mut thread_catalog = ThreadCatalog::default();
            thread_catalog.observe_history("C1", std::slice::from_ref(&root));
            let first_revision = WorkspaceRevision::INITIAL.successor();
            let bootstrap = StoreBatch::new(
                first_revision,
                vec![StoreChange::BootstrapReplaced(WorkspaceBootstrapData {
                    conversations: vec![SlackConversation {
                        id: "C1".into(),
                        name: Some("bootstrap".into()),
                        ..Default::default()
                    }],
                    users: vec![SlackUser {
                        id: Some("U1".into()),
                        name: Some("bootstrap-user".into()),
                        ..Default::default()
                    }],
                    histories: HashMap::from([(
                        "C1".into(),
                        vec![SlackMessage {
                            ts: "1.000".into(),
                            text: Some("bootstrap-history".into()),
                            ..Default::default()
                        }],
                    )]),
                    threads: thread_catalog.into_records(),
                })],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(bootstrap).await.unwrap(),
                StoreBatchExecution::Committed
            );

            let second_revision = first_revision.successor();
            let replacement = StoreBatch::new(
                second_revision,
                vec![
                    StoreChange::ConversationsReplaced(vec![SlackConversation {
                        id: "C2".into(),
                        name: Some("replace".into()),
                        ..Default::default()
                    }]),
                    StoreChange::ConversationsRepaired(vec![SlackConversation {
                        id: "C2".into(),
                        name: Some("replace".into()),
                        ..Default::default()
                    }]),
                    StoreChange::ConversationUpsert(SlackConversation {
                        id: "C3".into(),
                        name: Some("upsert".into()),
                        ..Default::default()
                    }),
                    StoreChange::ConversationMetadataUpsert(SlackConversation {
                        id: "C3".into(),
                        name: Some("metadata".into()),
                        ..Default::default()
                    }),
                    StoreChange::ConversationStarChanged {
                        channel_id: "C3".into(),
                        starred: false,
                    },
                    StoreChange::ConversationMembershipUpsert(SlackConversation {
                        id: "C3".into(),
                        is_starred: Some(true),
                        ..Default::default()
                    }),
                    StoreChange::ConversationRemoved {
                        channel_id: "C2".into(),
                    },
                    StoreChange::UnreadChanged {
                        snapshot: SlackConversationUnreadSnapshot {
                            channel_id: "C3".into(),
                            unread_state: SlackUnreadState::from_parts(true, true, 2),
                            last_read: Some("1.000".into()),
                            latest: Some("2.000".into()),
                            ..Default::default()
                        },
                    },
                    StoreChange::UsersReplaced(vec![SlackUser {
                        id: Some("U2".into()),
                        name: Some("replace-user".into()),
                        ..Default::default()
                    }]),
                    StoreChange::UserUpsert(SlackUser {
                        id: Some("U3".into()),
                        name: Some("upsert-user".into()),
                        ..Default::default()
                    }),
                    StoreChange::HistoryReplaced {
                        channel_id: "C3".into(),
                        messages: vec![SlackMessage {
                            ts: "2.000".into(),
                            text: Some("replacement-history".into()),
                            ..Default::default()
                        }],
                    },
                    StoreChange::MessageDelta {
                        channel_id: "C3".into(),
                        message: SlackMessage {
                            ts: "2.100".into(),
                            text: Some("delta-history".into()),
                            ..Default::default()
                        },
                        kind: MessageMutationKind::Posted,
                    },
                    StoreChange::HistoryRemoved {
                        channel_id: "C1".into(),
                    },
                    StoreChange::ThreadReplaced {
                        channel_id: "C3".into(),
                        thread_ts: "2.000".into(),
                        messages: vec![SlackMessage {
                            ts: "2.001".into(),
                            thread_ts: Some("2.000".into()),
                            text: Some("replacement-thread".into()),
                            ..Default::default()
                        }],
                    },
                    StoreChange::ThreadCatalogReplaced(Vec::new()),
                ],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(replacement).await.unwrap(),
                StoreBatchExecution::Committed
            );

            let conversations = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(conversations.len(), 1);
            assert_eq!(conversations[0].id, "C3");
            assert_eq!(conversations[0].name.as_deref(), Some("metadata"));
            assert_eq!(conversations[0].is_starred, Some(true));
            assert_eq!(conversations[0].unread_activity_count(), 2);
            let names = store.load_user_names().await.unwrap();
            assert_eq!(names.get("U2"), Some(&"replace-user".to_string()));
            assert_eq!(names.get("U3"), Some(&"upsert-user".to_string()));
            assert!(!names.contains_key("U1"));
            assert!(store.load_history("C1").await.unwrap().is_none());
            assert_eq!(
                store.load_history("C3").await.unwrap().unwrap()[0].body_text(),
                "delta-history"
            );
            assert_eq!(
                store.load_thread("C3", "2.000").await.unwrap().unwrap()[0].body_text(),
                "replacement-thread"
            );
            assert!(store.load_thread_catalog().await.unwrap().is_empty());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_round_trips_cached_snapshots() {
        let directory = temp_cache_dir("workspace-store");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            assert!(store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .is_none());

            let conversations = vec![SlackConversation {
                id: "C123".to_string(),
                name: Some("general".to_string()),
                is_channel: Some(true),
                ..Default::default()
            }];
            store
                .store_conversations(&conversations)
                .await
                .expect("conversation store failed");
            assert_eq!(
                store
                    .load_bootstrap()
                    .await
                    .expect("workspace bootstrap load failed")
                    .expect("missing cached workspace bootstrap")
                    .workspace_id,
                "T123:U123"
            );
            assert_eq!(
                store
                    .load_conversations()
                    .await
                    .expect("conversation load failed")
                    .expect("missing cached conversations")[0]
                    .id,
                "C123"
            );

            let messages = vec![SlackMessage {
                ts: "1710000000.000100".to_string(),
                text: Some("cached".to_string()),
                ..Default::default()
            }];
            store
                .store_history("C123", &messages)
                .await
                .expect("history store failed");
            assert_eq!(
                store
                    .load_history("C123")
                    .await
                    .expect("history load failed")
                    .expect("missing cached history")[0]
                    .body_text(),
                "cached"
            );

            store
                .store_thread("C123", "1710000000.000100", &messages)
                .await
                .expect("thread store failed");
            assert_eq!(
                store
                    .load_thread("C123", "1710000000.000100")
                    .await
                    .expect("thread load failed")
                    .expect("missing cached thread")[0]
                    .ts,
                "1710000000.000100"
            );

            let emojis = HashMap::from([
                (
                    "party_parrot".to_string(),
                    "https://emoji.example/parrot.gif".to_string(),
                ),
                ("ship_it".to_string(), "alias:rocket".to_string()),
            ]);
            store
                .store_custom_emojis(&emojis)
                .await
                .expect("emoji store failed");
            assert_eq!(
                store.load_custom_emojis().await.expect("emoji load failed"),
                emojis
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn schema_v1_migrates_to_v2_without_losing_keyed_payloads() {
        let directory = temp_cache_dir("workspace-schema-v2-migration");
        std::fs::create_dir_all(&directory).unwrap();
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let connection = Connection::open(store.database_path()).unwrap();
        connection
            .execute_batch(
                "PRAGMA user_version = 1;
                 CREATE TABLE workspaces (
                     workspace_key TEXT PRIMARY KEY,
                     workspace_id TEXT NOT NULL
                 ) WITHOUT ROWID;
                 CREATE TABLE app_state (
                     singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                     active_workspace_key TEXT REFERENCES workspaces(workspace_key)
                 );
                 INSERT INTO app_state(singleton, active_workspace_key) VALUES (1, NULL);
                 CREATE TABLE workspace_items (
                     workspace_key TEXT NOT NULL REFERENCES workspaces(workspace_key) ON DELETE CASCADE,
                     kind TEXT NOT NULL,
                     item_key TEXT NOT NULL,
                     payload_json TEXT NOT NULL,
                     PRIMARY KEY (workspace_key, kind, item_key)
                 ) WITHOUT ROWID;",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO workspaces(workspace_key, workspace_id) VALUES (?1, 'T123:U123')",
                [&store.workspace_key],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
                 VALUES (?1, 'conversation', 'C1', ?2)",
                params![
                    &store.workspace_key,
                    serde_json::to_string(&SlackConversation {
                        id: "C1".into(),
                        name: Some("general".into()),
                        ..Default::default()
                    })
                    .unwrap()
                ],
            )
            .unwrap();
        drop(connection);

        let conversations = runtime()
            .block_on(store.load_conversations())
            .unwrap()
            .unwrap();
        assert_eq!(conversations[0].id, "C1");

        let connection = Connection::open(store.database_path()).unwrap();
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let metadata_columns: Vec<String> = connection
            .prepare("PRAGMA table_info(sync_metadata)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            metadata_columns,
            [
                "workspace_key",
                "operation",
                "target",
                "refreshed_at_ms",
                "retry_count",
                "retry_after_ms"
            ]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn corrupt_database_is_recreated_as_an_empty_v2_cache() {
        let directory = temp_cache_dir("workspace-corrupt-database-reset");
        std::fs::create_dir_all(&directory).unwrap();
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        std::fs::write(store.database_path(), b"not a sqlite database").unwrap();

        assert!(runtime()
            .block_on(store.load_conversations())
            .unwrap()
            .is_none());
        let connection = Connection::open(store.database_path()).unwrap();
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn failed_v1_metadata_migration_recreates_only_the_derived_cache() {
        let directory = temp_cache_dir("workspace-failed-v1-migration-reset");
        std::fs::create_dir_all(&directory).unwrap();
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let connection = Connection::open(store.database_path()).unwrap();
        connection
            .execute_batch(
                "PRAGMA user_version = 1;
                 CREATE TABLE sync_metadata (broken TEXT);",
            )
            .unwrap();
        drop(connection);
        let credentials_sentinel = directory.join("credentials-are-external");
        let drafts_sentinel = directory.join("drafts-are-external");
        std::fs::write(&credentials_sentinel, "preserve").unwrap();
        std::fs::write(&drafts_sentinel, "preserve").unwrap();

        assert!(runtime()
            .block_on(store.load_conversations())
            .unwrap()
            .is_none());
        assert_eq!(
            std::fs::read_to_string(credentials_sentinel).unwrap(),
            "preserve"
        );
        assert_eq!(
            std::fs::read_to_string(drafts_sentinel).unwrap(),
            "preserve"
        );
        let connection = Connection::open(store.database_path()).unwrap();
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn malformed_keyed_payload_resets_the_workspace_cache() {
        let directory = temp_cache_dir("workspace-malformed-payload-reset");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }])
                .await
                .unwrap();
        });
        let connection = Connection::open(store.database_path()).unwrap();
        connection
            .execute(
                "UPDATE workspace_items SET payload_json = '{broken'
                 WHERE workspace_key = ?1 AND kind = 'conversation'",
                [&store.workspace_key],
            )
            .unwrap();
        drop(connection);

        assert!(runtime()
            .block_on(store.load_conversations())
            .unwrap()
            .is_none());
        let remaining: u32 = Connection::open(store.database_path())
            .unwrap()
            .query_row("SELECT count(*) FROM workspace_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn store_hub_reuses_one_writer_and_two_query_only_readers() {
        let directory = temp_cache_dir("store-hub-connections");
        runtime().block_on(async {
            let hub = StoreHub::open(directory.clone()).await.unwrap();

            let first_writer = hub
                .write(|connection| Ok(connection as *const Connection as usize))
                .await
                .unwrap();
            let second_writer = hub
                .write(|connection| Ok(connection as *const Connection as usize))
                .await
                .unwrap();
            assert_eq!(first_writer, second_writer);

            let mut readers = Vec::new();
            for _ in 0..4 {
                readers.push(
                    hub.query(|connection| {
                        let query_only: bool =
                            connection.query_row("PRAGMA query_only", [], |row| row.get(0))?;
                        Ok((connection as *const Connection as usize, query_only))
                    })
                    .await
                    .unwrap(),
                );
            }
            assert!(readers.iter().all(|(_, query_only)| *query_only));
            assert_eq!(readers[0].0, readers[2].0);
            assert_eq!(readers[1].0, readers[3].0);
            assert_ne!(readers[0].0, readers[1].0);
            assert_ne!(first_writer, readers[0].0);
            assert_ne!(first_writer, readers[1].0);

            hub.shutdown().await.unwrap();
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn store_hub_commit_barrier_makes_writer_changes_visible_to_readers() {
        let directory = temp_cache_dir("store-hub-barrier");
        runtime().block_on(async {
            let hub = StoreHub::open(directory.clone()).await.unwrap();
            hub.write(|connection| {
                connection.execute_batch(
                    "CREATE TABLE barrier_probe (value INTEGER NOT NULL);
                     INSERT INTO barrier_probe(value) VALUES (42);",
                )?;
                Ok(())
            })
            .await
            .unwrap();
            hub.barrier().await.unwrap();

            for _ in 0..2 {
                let value: i64 = hub
                    .query(|connection| {
                        Ok(connection
                            .query_row("SELECT value FROM barrier_probe", [], |row| row.get(0))?)
                    })
                    .await
                    .unwrap();
                assert_eq!(value, 42);
            }
            hub.shutdown().await.unwrap();
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn store_hub_shutdown_drains_queued_writes_and_rejects_new_work() {
        let directory = temp_cache_dir("store-hub-shutdown");
        runtime().block_on(async {
            let hub = StoreHub::open(directory.clone()).await.unwrap();
            let (started, wait_for_start) = std::sync::mpsc::channel();
            let (release, wait_for_release) = std::sync::mpsc::channel();
            let active = {
                let hub = hub.clone();
                tokio::spawn(async move {
                    hub.write(move |connection| {
                        started.send(()).unwrap();
                        wait_for_release.recv().unwrap();
                        connection.execute_batch(
                            "CREATE TABLE shutdown_probe (value INTEGER NOT NULL);
                             INSERT INTO shutdown_probe(value) VALUES (7);",
                        )?;
                        Ok(())
                    })
                    .await
                })
            };
            tokio::task::spawn_blocking(move || wait_for_start.recv().unwrap())
                .await
                .unwrap();
            let queued = {
                let hub = hub.clone();
                tokio::spawn(async move {
                    hub.write(|connection| {
                        connection.execute("INSERT INTO shutdown_probe(value) VALUES (8)", [])?;
                        Ok(())
                    })
                    .await
                })
            };
            while hub.inner.writer.capacity() == hub.inner.writer.max_capacity() {
                tokio::task::yield_now().await;
            }
            let shutdown = {
                let hub = hub.clone();
                tokio::spawn(async move { hub.shutdown().await })
            };
            assert!(!shutdown.is_finished());
            release.send(()).unwrap();
            active.await.unwrap().unwrap();
            queued.await.unwrap().unwrap();
            shutdown.await.unwrap().unwrap();

            let connection = Connection::open(directory.join(DATABASE_FILENAME)).unwrap();
            let values: i64 = connection
                .query_row("SELECT sum(value) FROM shutdown_probe", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(values, 15);
            assert!(matches!(
                hub.write(|_| Ok(())).await,
                Err(StoreError::HubClosed)
            ));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_bootstrap_loads_all_startup_domains_in_one_projection() {
        let directory = temp_cache_dir("workspace-bootstrap");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store
                .store_user_names(&HashMap::from([("U1".into(), "Ada".into())]))
                .await
                .unwrap();
            store
                .store_user_full_names(&HashMap::from([("U1".into(), "Ada Lovelace".into())]))
                .await
                .unwrap();
            store
                .store_user_avatar_urls(&HashMap::from([(
                    "U1".into(),
                    "https://avatars.slack-edge.com/u1.png".into(),
                )]))
                .await
                .unwrap();
            store
                .store_user_search_aliases(&HashMap::from([(
                    "U1".into(),
                    vec!["ada".into(), "lovelace".into()],
                )]))
                .await
                .unwrap();
            store
                .store_custom_emojis(&HashMap::from([(
                    "party".into(),
                    "https://emoji.slack-edge.com/party.png".into(),
                )]))
                .await
                .unwrap();

            let bootstrap = store.load_bootstrap().await.unwrap().unwrap();
            assert_eq!(bootstrap.workspace_id, "T123:U123");
            assert_eq!(bootstrap.conversations[0].id, "C1");
            assert_eq!(bootstrap.user_names["U1"], "Ada");
            assert_eq!(bootstrap.user_full_names["U1"], "Ada Lovelace");
            assert!(bootstrap.user_avatar_urls["U1"].ends_with("u1.png"));
            assert_eq!(bootstrap.user_search_aliases["U1"][0], "ada");
            assert!(bootstrap.custom_emojis.contains_key("party"));
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_bootstrap_does_not_read_unrelated_timeline_rows() {
        let directory = temp_cache_dir("workspace-bootstrap-focused");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }])
                .await
                .unwrap();
            let workspace_key = store.workspace_key.clone();
            store
                .hub()
                .await
                .unwrap()
                .write(move |connection| {
                    connection.execute(
                        "INSERT OR REPLACE INTO workspace_items(
                            workspace_key, kind, item_key, payload_json
                         ) VALUES (?1, 'channel_history', 'C_BAD', 'not-json')",
                        [workspace_key],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();

            let bootstrap = store.load_bootstrap().await.unwrap().unwrap();
            assert_eq!(bootstrap.conversations[0].id, "C1");
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_round_trips_rich_bot_message_fields() {
        let directory = temp_cache_dir("rich-bot-history");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let mut message = SlackMessage {
            ts: "1710000000.000200".to_string(),
            bot_id: Some("B123".to_string()),
            app_id: Some("A123".to_string()),
            bot_profile: Some(crate::models::SlackBotProfile {
                name: Some("People assistant".to_string()),
                icons: Some(crate::models::SlackIcons {
                    image_72: Some("https://cdn.example/bot.png".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            attachments: Some(vec![crate::models::SlackAttachment {
                fallback: Some("Review this request in Slack".to_string()),
                title: Some("Review request".to_string()),
                actions: Some(vec![crate::models::SlackAttachmentAction {
                    name: Some("decision".to_string()),
                    text: Some("Approve".to_string()),
                    kind: Some("button".to_string()),
                    value: Some("test-action-value".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        };
        message.refresh_canonical_content();

        runtime().block_on(async {
            store
                .store_history("C123", std::slice::from_ref(&message))
                .await
                .expect("rich history store failed");
            let restored = store
                .load_history("C123")
                .await
                .expect("rich history load failed")
                .expect("missing rich history");
            assert_eq!(restored.len(), 1);
            assert_eq!(restored[0].author_label(), "People assistant");
            assert_eq!(restored[0].visible_text(), "Review request");
            assert_eq!(restored[0].accessible_text(), "Review request\nApprove");
            assert!(restored[0].blocks.is_none());
            assert!(restored[0].attachments.is_none());
        });

        let connection = Connection::open(store.database_path()).unwrap();
        let payload: String = connection
            .query_row(
                "SELECT payload_json FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'channel_history' AND item_key = 'C123'",
                [&store.workspace_key],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!payload.contains("test-action-value"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn legacy_cached_history_is_upgraded_to_canonical_content() {
        let directory = temp_cache_dir("legacy-rich-history");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_history(
                    "C123",
                    &[SlackMessage {
                        ts: "1710000000.000300".to_string(),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
        });
        let connection = Connection::open(store.database_path()).unwrap();
        let legacy_payload = serde_json::json!([{
            "ts": "1710000000.000300",
            "bot_profile": {"name": "People assistant"},
            "attachments": [{"title": "Review request"}]
        }])
        .to_string();
        connection
            .execute(
                "UPDATE workspace_items SET payload_json = ?1
                 WHERE workspace_key = ?2 AND kind = 'channel_history' AND item_key = 'C123'",
                params![legacy_payload, store.workspace_key],
            )
            .unwrap();
        drop(connection);

        runtime().block_on(async {
            let restored = store.load_history("C123").await.unwrap().unwrap();
            assert_eq!(
                restored[0].content_version,
                crate::rich_message::MESSAGE_CONTENT_VERSION
            );
            assert_eq!(restored[0].author_label(), "People assistant");
            assert_eq!(restored[0].visible_text(), "Review request");
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn richer_fresh_message_replaces_legacy_cached_message_with_same_timestamp() {
        let directory = temp_cache_dir("fresh-replaces-legacy");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_history(
                    "C123",
                    &[SlackMessage {
                        ts: "1710000000.000400".to_string(),
                        text: Some("Legacy fallback".to_string()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            store
                .store_merged_history(
                    "C123",
                    &[SlackMessage {
                        ts: "1710000000.000400".to_string(),
                        attachments: Some(vec![crate::models::SlackAttachment {
                            title: Some("Fresh request".to_string()),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();

            let restored = store.load_history("C123").await.unwrap().unwrap();
            assert_eq!(restored.len(), 1);
            assert_eq!(restored[0].visible_text(), "Fresh request");
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn focused_repository_reads_ignore_malformed_unrelated_domains() {
        let directory = temp_cache_dir("workspace-focused-reads");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_history(
                    "C1",
                    &[SlackMessage {
                        ts: "1.0".into(),
                        text: Some("history".into()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            store
                .store_user_names(&HashMap::from([("U1".into(), "Ada".into())]))
                .await
                .unwrap();
        });
        let connection = Connection::open(store.database_path()).unwrap();
        connection
            .execute(
                "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
                 VALUES (?1, 'conversation', 'BROKEN', '{broken')",
                [&store.workspace_key],
            )
            .unwrap();
        drop(connection);

        runtime().block_on(async {
            assert_eq!(
                store.load_history("C1").await.unwrap().unwrap()[0].body_text(),
                "history"
            );
            assert_eq!(store.load_user_names().await.unwrap()["U1"], "Ada");
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn sync_freshness_round_trips_success_and_retry_metadata() {
        let directory = temp_cache_dir("workspace-sync-freshness");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            assert_eq!(
                store
                    .load_sync_freshness("membership", "workspace")
                    .await
                    .unwrap(),
                None
            );
            let freshness = SyncFreshness {
                refreshed_at_ms: Some(1_721_500_000_000),
                retry_count: 3,
                retry_after_ms: Some(1_721_500_030_000),
            };
            store
                .store_sync_freshness("membership", "workspace", freshness.clone())
                .await
                .unwrap();
            assert_eq!(
                store
                    .load_sync_freshness("membership", "workspace")
                    .await
                    .unwrap(),
                Some(freshness)
            );
            assert_eq!(
                store.load_sync_freshness("history", "C1").await.unwrap(),
                None
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn store_hub_batches_fifty_maintenance_mutations_in_one_transaction() {
        let directory = temp_cache_dir("store-hub-maintenance-batch");
        runtime().block_on(async {
            let hub = StoreHub::open(directory.clone()).await.unwrap();
            hub.write(|connection| {
                connection.execute(
                    "CREATE TABLE maintenance_probe (value INTEGER PRIMARY KEY)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
            let baseline = hub.metrics();

            let mut writes = tokio::task::JoinSet::new();
            for value in 0..50_i64 {
                let hub = hub.clone();
                writes.spawn(async move {
                    hub.write_maintenance(move |transaction| {
                        transaction
                            .execute("INSERT INTO maintenance_probe(value) VALUES (?1)", [value])?;
                        Ok(())
                    })
                    .await
                });
            }
            while let Some(result) = writes.join_next().await {
                result.unwrap().unwrap();
            }

            let metrics = hub.metrics();
            assert_eq!(metrics.transactions - baseline.transactions, 1);
            assert_eq!(metrics.changed_rows - baseline.changed_rows, 50);
            hub.shutdown().await.unwrap();
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn store_hub_rolls_back_failed_batches_and_suppresses_unchanged_commits() {
        let directory = temp_cache_dir("store-hub-maintenance-rollback");
        runtime().block_on(async {
            let hub = StoreHub::open(directory.clone()).await.unwrap();
            hub.write(|connection| {
                connection.execute(
                    "CREATE TABLE maintenance_probe (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();

            hub.write_maintenance(|transaction| {
                transaction.execute(
                    "INSERT INTO maintenance_probe(key, value) VALUES ('stable', 'same')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
            let after_insert = hub.metrics();
            hub.write_maintenance(|transaction| {
                transaction.execute(
                    "INSERT INTO maintenance_probe(key, value) VALUES ('stable', 'same')
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value
                     WHERE maintenance_probe.value IS NOT excluded.value",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
            let after_unchanged = hub.metrics();
            assert_eq!(after_unchanged.transactions, after_insert.transactions);
            assert_eq!(after_unchanged.skipped_rows, after_insert.skipped_rows + 1);

            let first = {
                let hub = hub.clone();
                tokio::spawn(async move {
                    hub.write_maintenance(|transaction| {
                        transaction.execute(
                            "INSERT INTO maintenance_probe(key, value) VALUES ('rollback', 'yes')",
                            [],
                        )?;
                        Ok(())
                    })
                    .await
                })
            };
            let second = {
                let hub = hub.clone();
                tokio::spawn(async move {
                    hub.write_maintenance(|transaction| {
                        transaction.execute("INSERT INTO missing_table(value) VALUES (1)", [])?;
                        Ok(())
                    })
                    .await
                })
            };
            assert!(first.await.unwrap().is_err());
            assert!(second.await.unwrap().is_err());
            hub.barrier().await.unwrap();
            let exists: bool = hub
                .query(|connection| {
                    Ok(connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM maintenance_probe WHERE key = 'rollback')",
                        [],
                        |row| row.get(0),
                    )?)
                })
                .await
                .unwrap();
            assert!(!exists);
            hub.shutdown().await.unwrap();
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn focused_repository_suppresses_identical_write_commits() {
        let directory = temp_cache_dir("workspace-identical-write");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let names = HashMap::from([("U1".to_string(), "Ada".to_string())]);
            store.store_user_names(&names).await.unwrap();
            let after_insert = store.hub().await.unwrap().metrics();
            store.store_user_names(&names).await.unwrap();
            let after_identical = store.hub().await.unwrap().metrics();
            assert_eq!(after_identical.transactions, after_insert.transactions);
            assert!(after_identical.skipped_rows > after_insert.skipped_rows);
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_does_not_persist_ephemeral_huddle_metadata() {
        let directory = temp_cache_dir("workspace-huddle-privacy");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");

        runtime().block_on(async {
            let conversation: SlackConversation = serde_json::from_value(serde_json::json!({
                "id": "C123",
                "name": "general",
                "properties": {
                    "huddles": {
                        "id": "R_PRIVATE",
                        "participants": ["U_PRIVATE"]
                    },
                    "canvas": { "enabled": true }
                }
            }))
            .unwrap();
            assert!(conversation.has_huddle_metadata());

            store
                .store_conversations(std::slice::from_ref(&conversation))
                .await
                .expect("conversation snapshot store failed");
            store
                .store_conversation(&conversation)
                .await
                .expect("conversation row store failed");

            let cached = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversation");
            assert!(!cached[0].has_huddle_metadata());
            assert_eq!(
                cached[0]
                    .extra
                    .get("properties")
                    .and_then(|value| value.get("canvas"))
                    .and_then(|value| value.get("enabled"))
                    .and_then(serde_json::Value::as_bool),
                Some(true)
            );

            let connection = Connection::open(store.database_path()).unwrap();
            let payload: String = connection
                .query_row(
                    "SELECT payload_json FROM workspace_items
                     WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C123'",
                    [&store.workspace_key],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!payload.contains("R_PRIVATE"));
            assert!(!payload.contains("U_PRIVATE"));
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn ensuring_workspace_identity_upgrades_an_existing_cache() {
        let directory = temp_cache_dir("workspace-store-identity-upgrade");
        std::fs::create_dir_all(&directory).unwrap();
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        std::fs::write(
            store.path(),
            r#"{"version":1,"conversations":[{"id":"D1","is_im":true}]}"#,
        )
        .unwrap();

        runtime()
            .block_on(store.ensure_workspace_identity())
            .expect("workspace identity upgrade failed");

        let state = runtime()
            .block_on(store.load_bootstrap())
            .unwrap()
            .expect("missing upgraded bootstrap");
        assert_eq!(state.workspace_id, "T123:U123");
        assert_eq!(state.conversations[0].id, "D1");
        assert!(!store.path().exists());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_exposes_a_lightweight_active_search_snapshot() {
        let directory = temp_cache_dir("workspace-search-index");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    is_channel: Some(true),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store
                .store_history(
                    "C1",
                    &[SlackMessage {
                        ts: "1.0".into(),
                        text: Some("private message body".into()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            store.ensure_workspace_identity().await.unwrap();
        });

        let search_state = load_active_search_state(&directory).unwrap().unwrap();
        assert_eq!(search_state.workspace_id, "T123:U123");
        assert_eq!(search_state.conversations[0].id, "C1");
        assert!(store.database_path().exists());

        let connection = Connection::open(store.database_path()).unwrap();
        let stored_private_body: bool = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM workspace_items
                    WHERE workspace_key = ?1 AND kind = 'channel_history'
                      AND payload_json LIKE '%private message body%'
                )",
                [&store.workspace_key],
                |row| row.get(0),
            )
            .unwrap();
        assert!(stored_private_body);

        connection
            .execute(
                "UPDATE workspace_items SET payload_json = 'not valid JSON'
                 WHERE workspace_key = ?1 AND kind = 'channel_history'",
                [&store.workspace_key],
            )
            .unwrap();
        let search_state = load_active_search_state(&directory).unwrap().unwrap();
        assert_eq!(search_state.conversations[0].id, "C1");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn clearing_the_active_workspace_preserves_its_cached_state() {
        let directory = temp_cache_dir("workspace-clear-active");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store.ensure_workspace_identity().await.unwrap();
        });

        clear_active_workspace(&directory).unwrap();
        runtime().block_on(async {
            store
                .store_history(
                    "C1",
                    &[SlackMessage {
                        ts: "1.0".into(),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
        });

        assert!(load_active_search_state(&directory).unwrap().is_none());
        let conversations = runtime()
            .block_on(store.load_conversations())
            .unwrap()
            .unwrap();
        assert_eq!(conversations[0].id, "C1");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_updates_one_conversation_without_replacing_others() {
        let directory = temp_cache_dir("workspace-store-conversation-update");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[
                    SlackConversation {
                        id: "C1".to_string(),
                        name: Some("general".to_string()),
                        ..Default::default()
                    },
                    SlackConversation {
                        id: "C2".to_string(),
                        name: Some("random".to_string()),
                        ..Default::default()
                    },
                ])
                .await
                .expect("conversation store failed");

            store
                .store_conversation(&SlackConversation {
                    id: "C1".to_string(),
                    name: Some("renamed".to_string()),
                    ..Default::default()
                })
                .await
                .expect("conversation update failed");
            store
                .store_conversation(&SlackConversation {
                    id: "C3".to_string(),
                    name: Some("new".to_string()),
                    ..Default::default()
                })
                .await
                .expect("conversation insert failed");

            let conversations = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert_eq!(conversations.len(), 3);
            assert_eq!(
                conversations
                    .iter()
                    .find(|conversation| conversation.id == "C1")
                    .and_then(|conversation| conversation.name.as_deref()),
                Some("renamed")
            );
            assert!(conversations
                .iter()
                .any(|conversation| conversation.id == "C2"));
            assert!(conversations
                .iter()
                .any(|conversation| conversation.id == "C3"));
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_persists_sparse_conversation_star_updates() {
        let directory = temp_cache_dir("workspace-store-conversation-star-update");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");

        runtime().block_on(async {
            store
                .store_conversation(&SlackConversation {
                    id: "C1".to_string(),
                    name: Some("general".to_string()),
                    is_channel: Some(true),
                    is_starred: Some(true),
                    ..Default::default()
                })
                .await
                .unwrap();
            store
                .store_conversation(&SlackConversation {
                    id: "C1".to_string(),
                    is_starred: Some(false),
                    ..Default::default()
                })
                .await
                .unwrap();

            let conversations = store
                .load_conversations()
                .await
                .unwrap()
                .expect("missing cached conversations");
            assert_eq!(conversations.len(), 1);
            assert_eq!(conversations[0].name.as_deref(), Some("general"));
            assert_eq!(conversations[0].is_starred, Some(false));
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn conversation_row_mutations_ignore_unrelated_corrupt_rows() {
        let directory = temp_cache_dir("workspace-store-conversation-row-update");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[
                    SlackConversation {
                        id: "C1".to_string(),
                        name: Some("old".to_string()),
                        unread_count: Some(3),
                        ..Default::default()
                    },
                    SlackConversation {
                        id: "C2".to_string(),
                        name: Some("unrelated".to_string()),
                        ..Default::default()
                    },
                ])
                .await
                .expect("conversation store failed");

            let connection = Connection::open(store.database_path()).unwrap();
            connection
                .execute(
                    "UPDATE workspace_items SET payload_json = '{broken'
                     WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C2'",
                    [&store.workspace_key],
                )
                .unwrap();
            drop(connection);

            assert!(store
                .clear_conversation_unread_state("C1", "20.0")
                .await
                .expect("read update failed"));
            store
                .merge_conversation(&SlackConversation {
                    id: "C1".to_string(),
                    name: Some("renamed".to_string()),
                    unread_count: Some(8),
                    ..Default::default()
                })
                .await
                .expect("metadata update read an unrelated row");
            assert!(!store
                .apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(true, true, 4),
                    Some("10.0"),
                )
                .await
                .expect("stale unread update failed"));
            assert!(store
                .mark_conversation_unread_from_event("C1", "21.0")
                .await
                .expect("realtime update read an unrelated row"));
        });

        let connection = Connection::open(store.database_path()).unwrap();
        let updated_payload: String = connection
            .query_row(
                "SELECT payload_json FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C1'",
                [&store.workspace_key],
                |row| row.get(0),
            )
            .unwrap();
        let updated: SlackConversation = serde_json::from_str(&updated_payload).unwrap();
        assert_eq!(updated.name.as_deref(), Some("renamed"));
        assert_eq!(updated.unread_activity_count(), 1);
        assert_eq!(
            updated
                .extra
                .get(LOCAL_READ_TS_KEY)
                .and_then(serde_json::Value::as_str),
            Some("20.0")
        );
        let unrelated_payload: String = connection
            .query_row(
                "SELECT payload_json FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C2'",
                [&store.workspace_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unrelated_payload, "{broken");

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn conversation_row_mutations_do_not_follow_mismatched_payload_ids() {
        let directory = temp_cache_dir("workspace-store-conversation-row-id-mismatch");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[
                    SlackConversation {
                        id: "C0".to_string(),
                        name: Some("untouched".to_string()),
                        unread_count: Some(2),
                        ..Default::default()
                    },
                    SlackConversation {
                        id: "C1".to_string(),
                        name: Some("original".to_string()),
                        ..Default::default()
                    },
                ])
                .await
                .expect("conversation store failed");

            let mismatched = serde_json::to_string(&SlackConversation {
                id: "C0".to_string(),
                name: Some("mismatched".to_string()),
                unread_count: Some(99),
                ..Default::default()
            })
            .unwrap();
            let replace_c1_payload = |payload: &str| {
                let connection = Connection::open(store.database_path()).unwrap();
                connection
                    .execute(
                        "UPDATE workspace_items SET payload_json = ?1
                         WHERE workspace_key = ?2 AND kind = 'conversation' AND item_key = 'C1'",
                        params![payload, &store.workspace_key],
                    )
                    .unwrap();
            };
            replace_c1_payload(&mismatched);

            assert!(!store
                .apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(true, true, 7),
                    None,
                )
                .await
                .expect("mismatched unread update failed"));
            assert!(!store
                .clear_conversation_unread_state("C1", "20.0")
                .await
                .expect("mismatched read update failed"));

            store
                .store_conversation(&SlackConversation {
                    id: "C1".to_string(),
                    name: Some("metadata repaired".to_string()),
                    ..Default::default()
                })
                .await
                .expect("metadata repair failed");
            let repaired = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C0")
                    .and_then(|conversation| conversation.name.as_deref()),
                Some("untouched")
            );
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C0")
                    .map(SlackConversation::unread_activity_count),
                Some(2)
            );
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C1")
                    .and_then(|conversation| conversation.name.as_deref()),
                Some("metadata repaired")
            );

            replace_c1_payload(&mismatched);
            assert!(store
                .mark_conversation_unread_from_event("C1", "21.0")
                .await
                .expect("realtime repair failed"));
            let repaired = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C0")
                    .and_then(|conversation| conversation.name.as_deref()),
                Some("untouched")
            );
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C0")
                    .map(SlackConversation::unread_activity_count),
                Some(2)
            );
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C1")
                    .map(SlackConversation::unread_activity_count),
                Some(1)
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn conversation_metadata_updates_preserve_local_read_overlay() {
        let directory = temp_cache_dir("workspace-store-conversation-metadata-overlay");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".to_string(),
                    name: Some("old".to_string()),
                    unread_count: Some(3),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store
                .clear_conversation_unread_state("C1", "20.0")
                .await
                .unwrap();

            let stale = SlackConversation {
                id: "C1".to_string(),
                name: Some("renamed".to_string()),
                unread_count: Some(8),
                ..Default::default()
            };
            store.store_conversation(&stale).await.unwrap();
            store.merge_conversation(&stale).await.unwrap();

            let conversations = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(conversations[0].name.as_deref(), Some("renamed"));
            assert_eq!(conversations[0].unread_activity_count(), 0);
            assert_eq!(
                conversations[0]
                    .extra
                    .get(LOCAL_READ_TS_KEY)
                    .and_then(serde_json::Value::as_str),
                Some("20.0")
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_merges_sparse_enrichment_without_losing_unread_state() {
        let directory = temp_cache_dir("workspace-store-conversation-merge");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "G1".to_string(),
                    is_mpim: Some(true),
                    unread_count: Some(4),
                    ..Default::default()
                }])
                .await
                .expect("conversation store failed");
            let mut enrichment = SlackConversation {
                id: "G1".to_string(),
                is_mpim: Some(true),
                ..Default::default()
            };
            enrichment
                .extra
                .insert("members".to_string(), serde_json::json!(["U1", "U2"]));
            store
                .merge_conversation(&enrichment)
                .await
                .expect("conversation merge failed");

            let conversations = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert_eq!(conversations[0].unread_activity_count(), 4);
            assert_eq!(
                conversations[0].extra.get("members"),
                Some(&serde_json::json!(["U1", "U2"]))
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_patches_and_clears_conversation_unread_state() {
        let directory = temp_cache_dir("workspace-store-conversation-unread");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".to_string(),
                    name: Some("general".to_string()),
                    ..Default::default()
                }])
                .await
                .expect("conversation store failed");

            assert!(store
                .apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(true, true, 7),
                    None
                )
                .await
                .expect("unread update failed"));
            let unread = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert!(unread[0].has_unread_activity());
            assert_eq!(unread[0].unread_activity_count(), 7);

            assert!(store
                .clear_conversation_unread_state("C1", "2.0")
                .await
                .expect("unread clear failed"));
            let cleared = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert!(!cleared[0].has_unread_activity());
            assert_eq!(cleared[0].unread_activity_count(), 0);

            assert!(!store
                .apply_conversation_unread_state(
                    "missing",
                    SlackUnreadState::from_parts(true, true, 1),
                    None,
                )
                .await
                .expect("missing unread update failed"));
            assert!(!store
                .apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(false, true, 1),
                    None,
                )
                .await
                .expect("unknown unread update failed"));
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn realtime_conversation_unread_events_are_idempotent_and_upsert_unknown_ids() {
        let directory = temp_cache_dir("workspace-store-realtime-unread");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            assert!(store
                .mark_conversation_unread_from_event("D1", "1710000001.000001")
                .await
                .expect("first realtime update failed"));
            assert!(!store
                .mark_conversation_unread_from_event("D1", "1710000001.000001")
                .await
                .expect("duplicate realtime update failed"));
            assert!(store
                .mark_conversation_unread_from_event("D1", "1710000002.000001")
                .await
                .expect("second realtime update failed"));

            let conversations = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert_eq!(conversations.len(), 1);
            assert_eq!(conversations[0].id, "D1");
            assert_eq!(conversations[0].unread_activity_count(), 2);
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn classified_noise_does_not_become_unread_after_raw_reconciliation() {
        let directory = temp_cache_dir("workspace-store-attention-noise");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".to_string(),
                    ..Default::default()
                }])
                .await
                .unwrap();
            assert!(store
                .observe_conversation_attention_from_event("C1", "10.0", false)
                .await
                .unwrap());
            assert!(store
                .apply_conversation_unread_snapshot(&SlackConversationUnreadSnapshot {
                    channel_id: "C1".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, true, 1),
                    latest: Some("10.0".to_string()),
                    ..Default::default()
                })
                .await
                .unwrap());

            let conversation = store
                .load_conversations()
                .await
                .unwrap()
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(conversation.raw_unread_activity_count(), 1);
            assert!(!conversation.has_unread_activity());
            assert_eq!(conversation.unread_activity_count(), 0);
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn reconciled_attention_batch_persists_filtered_message_identities() {
        let directory = temp_cache_dir("workspace-store-attention-reconciliation");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let accepted = store
                .observe_conversation_attention_batch(
                    "C1",
                    vec![("10.0".to_string(), false), ("11.0".to_string(), true)],
                )
                .await
                .unwrap();
            assert_eq!(accepted, ["10.0", "11.0"]);
            assert!(store
                .observe_conversation_attention_batch(
                    "C1",
                    vec![("10.0".to_string(), false), ("11.0".to_string(), true)],
                )
                .await
                .unwrap()
                .is_empty());

            let conversation = store
                .load_conversations()
                .await
                .unwrap()
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(conversation.unread_activity_count(), 1);
            assert!(conversation.has_observed_attention_message("10.0"));
            assert!(conversation.has_observed_attention_message("11.0"));
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn attention_delivery_claim_is_atomic_and_survives_reopen() {
        let directory = temp_cache_dir("workspace-store-attention-delivery");
        let runtime = runtime();

        runtime.block_on(async {
            let store = WorkspaceStore::new(directory.clone(), "T123:U123");
            assert!(store
                .claim_attention_delivery("D1", "1710000001.000001")
                .await
                .unwrap());
            assert!(!store
                .claim_attention_delivery("D1", "1710000001.000001")
                .await
                .unwrap());
            assert!(store
                .claim_attention_delivery("D1", "1710000002.000001")
                .await
                .unwrap());

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
            assert!(!reopened
                .claim_attention_delivery("D1", "1710000001.000001")
                .await
                .unwrap());
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn realtime_attention_observation_and_notification_claim_share_one_transaction() {
        let directory = temp_cache_dir("workspace-store-attention-acceptance");
        let runtime = runtime();

        runtime.block_on(async {
            let store = WorkspaceStore::new(directory.clone(), "T123:U123");
            assert_eq!(
                store
                    .accept_attention_delivery("", "1710000001.000001", true, true)
                    .await
                    .unwrap(),
                AttentionDeliveryOutcome {
                    observation: AttentionObservationStatus::InvalidIdentity,
                    notification_claimed: false,
                }
            );
            let first = store
                .accept_attention_delivery("D1", "1710000001.000001", true, true)
                .await
                .unwrap();
            assert_eq!(
                first,
                AttentionDeliveryOutcome {
                    observation: AttentionObservationStatus::Accepted,
                    notification_claimed: true,
                }
            );
            assert_eq!(
                store
                    .accept_attention_delivery("D1", "1710000001.000001", true, true)
                    .await
                    .unwrap(),
                AttentionDeliveryOutcome {
                    observation: AttentionObservationStatus::AlreadyObserved,
                    notification_claimed: false,
                }
            );

            drop(store);
            let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
            assert_eq!(
                reopened
                    .accept_attention_delivery("D1", "1710000001.000001", true, true)
                    .await
                    .unwrap(),
                AttentionDeliveryOutcome {
                    observation: AttentionObservationStatus::AlreadyObserved,
                    notification_claimed: false,
                }
            );
            let conversation = reopened
                .load_conversations()
                .await
                .unwrap()
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(conversation.unread_activity_count(), 1);
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn local_read_marker_rejects_older_server_and_realtime_updates() {
        let directory = temp_cache_dir("workspace-store-read-ordering");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".to_string(),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store
                .clear_conversation_unread_state("C1", "20.0")
                .await
                .unwrap();
            assert_eq!(
                store
                    .accept_attention_delivery("C1", "10.0", true, true)
                    .await
                    .unwrap(),
                AttentionDeliveryOutcome {
                    observation: AttentionObservationStatus::AtOrBeforeReadCursor,
                    notification_claimed: false,
                }
            );
            assert!(!store
                .apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(true, true, 4),
                    Some("10.0"),
                )
                .await
                .unwrap());
            assert!(!store
                .mark_conversation_unread_from_event("C1", "19.0")
                .await
                .unwrap());
            assert!(store
                .mark_conversation_unread_from_event("C1", "21.0")
                .await
                .unwrap());
            let conversations = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(conversations[0].unread_activity_count(), 1);
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn unread_snapshot_preserves_local_read_and_latest_ordering_across_restart() {
        let directory = temp_cache_dir("workspace-store-unread-snapshot-ordering");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[serde_json::from_value(serde_json::json!({
                    "id": "D1",
                    "is_im": true,
                    "latest": "30.0"
                }))
                .unwrap()])
                .await
                .unwrap();
            store
                .clear_conversation_unread_state("D1", "20.0")
                .await
                .unwrap();

            assert!(!store
                .apply_conversation_unread_snapshot(&SlackConversationUnreadSnapshot {
                    channel_id: "D1".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, true, 0),
                    last_read: Some("19.0".to_string()),
                    latest: Some("31.0".to_string()),
                    mention_count: Some(4),
                    is_open: Some(true),
                })
                .await
                .unwrap());
            assert!(store
                .apply_conversation_unread_snapshot(&SlackConversationUnreadSnapshot {
                    channel_id: "D1".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, true, 0),
                    last_read: Some("20.0".to_string()),
                    latest: Some("29.0".to_string()),
                    mention_count: Some(4),
                    is_open: Some(true),
                })
                .await
                .unwrap());
            assert!(!store
                .apply_conversation_unread_snapshot(&SlackConversationUnreadSnapshot {
                    channel_id: "D1".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, false, 0),
                    last_read: Some("19.0".to_string()),
                    latest: Some("31.0".to_string()),
                    ..Default::default()
                })
                .await
                .unwrap());
        });

        let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime.block_on(async {
            let conversations = reopened.load_conversations().await.unwrap().unwrap();
            let conversation = &conversations[0];
            assert!(conversation.has_unread_activity());
            assert_eq!(conversation.unread_activity_count(), 0);
            assert_eq!(conversation.last_read_ts(), Some("20.0"));
            assert_eq!(conversation.latest_message_ts(), Some("30.0"));
            assert!(conversation.has_active_direct_message_hint());
            assert!(!conversation.extra.contains_key(LOCAL_READ_TS_KEY));
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn atomic_membership_reconciliation_preserves_unread_overlay_and_pending_work() {
        let directory = temp_cache_dir("workspace-store-atomic-membership");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".to_string(),
                    name: Some("old".to_string()),
                    unread_count: Some(5),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store
                .store_pending_unread_refresh(&["C1".to_string(), "D2".to_string()])
                .await
                .unwrap();
            let committed = store
                .reconcile_conversations(vec![SlackConversation {
                    id: "C1".to_string(),
                    name: Some("renamed".to_string()),
                    ..Default::default()
                }])
                .await
                .unwrap();
            assert_eq!(committed[0].name.as_deref(), Some("renamed"));
            assert_eq!(committed[0].unread_activity_count(), 5);
            assert_eq!(
                store.load_pending_unread_refresh().await.unwrap(),
                vec!["C1".to_string(), "D2".to_string()]
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_preserves_pending_unread_refresh_queue_order() {
        let directory = temp_cache_dir("workspace-store-pending-unread-order");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();
        let pending = vec![
            "D-zebra".to_string(),
            "C-alpha".to_string(),
            "D-middle".to_string(),
        ];

        runtime.block_on(async {
            store.store_pending_unread_refresh(&pending).await.unwrap();

            assert_eq!(store.load_pending_unread_refresh().await.unwrap(), pending);
        });

        let connection = Connection::open(store.database_path()).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT item_key, payload_json FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'pending_unread'",
            )
            .unwrap();
        let rows = statement
            .query_map([&store.workspace_key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, PENDING_UNREAD_QUEUE_KEY);
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&rows[0].1).unwrap(),
            pending
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_loads_and_replaces_legacy_pending_unread_rows() {
        let directory = temp_cache_dir("workspace-store-legacy-pending-unread");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();
        runtime.block_on(store.ensure_workspace_identity()).unwrap();

        {
            let connection = Connection::open(store.database_path()).unwrap();
            for channel_id in ["D-zebra", "C-alpha", "D-middle"] {
                connection
                    .execute(
                        "INSERT INTO workspace_items(
                            workspace_key, kind, item_key, payload_json
                         ) VALUES (?1, 'pending_unread', ?2, 'null')",
                        params![&store.workspace_key, channel_id],
                    )
                    .unwrap();
            }
        }

        let expected = vec![
            "C-alpha".to_string(),
            "D-middle".to_string(),
            "D-zebra".to_string(),
        ];
        runtime.block_on(async {
            assert_eq!(store.load_pending_unread_refresh().await.unwrap(), expected);
            store.store_pending_unread_refresh(&expected).await.unwrap();
        });

        let connection = Connection::open(store.database_path()).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT item_key, payload_json FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'pending_unread'",
            )
            .unwrap();
        let rows = statement
            .query_map([&store.workspace_key], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, PENDING_UNREAD_QUEUE_KEY);
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&rows[0].1).unwrap(),
            expected
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_serializes_individual_conversation_updates_across_clones() {
        let directory = temp_cache_dir("workspace-store-conversation-concurrent");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let cloned_store = store.clone();
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".to_string(),
                    ..Default::default()
                }])
                .await
                .expect("conversation store failed");

            let (unread_result, insert_result) = futures_util::future::join(
                store.apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(true, true, 3),
                    None,
                ),
                cloned_store.store_conversation(&SlackConversation {
                    id: "C2".to_string(),
                    ..Default::default()
                }),
            )
            .await;
            assert!(unread_result.expect("unread update failed"));
            insert_result.expect("conversation insert failed");

            let conversations = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert_eq!(conversations.len(), 2);
            assert_eq!(
                conversations
                    .iter()
                    .find(|conversation| conversation.id == "C1")
                    .map(SlackConversation::unread_activity_count),
                Some(3)
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_removes_one_conversation() {
        let directory = temp_cache_dir("workspace-store-conversation-remove");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[
                    SlackConversation {
                        id: "C1".to_string(),
                        ..Default::default()
                    },
                    SlackConversation {
                        id: "C2".to_string(),
                        ..Default::default()
                    },
                ])
                .await
                .expect("conversation store failed");

            assert!(store
                .remove_conversation("C1")
                .await
                .expect("conversation removal failed"));
            assert!(!store
                .remove_conversation("C1")
                .await
                .expect("duplicate conversation removal failed"));
            let conversations = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert_eq!(conversations.len(), 1);
            assert_eq!(conversations[0].id, "C2");
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_round_trips_user_names() {
        let directory = temp_cache_dir("workspace-store-user-names");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            assert!(store
                .load_user_names()
                .await
                .expect("user name load failed")
                .is_empty());

            store
                .store_user_name("U123", "Ada Lovelace")
                .await
                .expect("user name store failed");

            assert_eq!(
                store
                    .load_user_names()
                    .await
                    .expect("user name load failed")
                    .get("U123")
                    .map(String::as_str),
                Some("Ada Lovelace")
            );

            store
                .store_user_full_names(&HashMap::from([(
                    "U123".to_string(),
                    "Augusta Ada King".to_string(),
                )]))
                .await
                .expect("user full name store failed");
            assert_eq!(
                store
                    .load_user_full_names()
                    .await
                    .expect("user full name load failed")
                    .get("U123")
                    .map(String::as_str),
                Some("Augusta Ada King")
            );

            let avatar_urls = HashMap::from([(
                "U123".to_string(),
                "https://avatars.slack-edge.com/ada.png".to_string(),
            )]);
            store
                .store_user_avatar_urls(&avatar_urls)
                .await
                .expect("user avatar URL store failed");
            assert_eq!(
                store
                    .load_user_avatar_urls()
                    .await
                    .expect("user avatar URL load failed"),
                avatar_urls
            );

            let aliases = HashMap::from([(
                "U123".to_string(),
                vec!["Ada".to_string(), "Ada Lovelace".to_string()],
            )]);
            store
                .store_user_search_aliases(&aliases)
                .await
                .expect("user search alias store failed");
            assert_eq!(
                store
                    .load_user_search_aliases()
                    .await
                    .expect("user search alias load failed"),
                aliases
            );

            let status = SlackUserStatus {
                text: "In a meeting".to_string(),
                emoji: ":calendar:".to_string(),
                expiration: 200,
            };
            store
                .store_user_status("U123", Some(status.clone()))
                .await
                .expect("user status store failed");
            assert_eq!(
                store
                    .load_user_statuses()
                    .await
                    .expect("user status load failed")
                    .get("U123"),
                Some(&status)
            );
            store
                .store_user_status("U123", None)
                .await
                .expect("user status removal failed");
            assert!(store
                .load_user_statuses()
                .await
                .expect("user status load failed")
                .is_empty());
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_serializes_concurrent_updates_from_clones() {
        let directory = temp_cache_dir("workspace-store-concurrent-updates");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let cloned_store = store.clone();
        let runtime = runtime();

        runtime.block_on(async {
            let conversations = vec![SlackConversation {
                id: "C123".to_string(),
                name: Some("general".to_string()),
                ..Default::default()
            }];
            let messages = vec![SlackMessage {
                ts: "1710000000.000100".to_string(),
                text: Some("cached".to_string()),
                ..Default::default()
            }];

            let (conversations_result, history_result) = futures_util::future::join(
                store.store_conversations(&conversations),
                cloned_store.store_history("C123", &messages),
            )
            .await;
            conversations_result.expect("conversation store failed");
            history_result.expect("history store failed");

            assert_eq!(
                store
                    .load_conversations()
                    .await
                    .expect("conversation load failed")
                    .expect("concurrent conversation update was lost")[0]
                    .id,
                "C123"
            );
            assert_eq!(
                store
                    .load_history("C123")
                    .await
                    .expect("history load failed")
                    .expect("concurrent history update was lost")[0]
                    .body_text(),
                "cached"
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_cache_key_does_not_expose_workspace_identity() {
        let key = cache_key("T123:U123");

        assert_eq!(key.len(), 64);
        assert!(!key.contains("T123"));
        assert!(!key.contains("U123"));
    }

    #[test]
    fn workspace_store_replaces_invalid_cache_on_next_write() {
        let directory = temp_cache_dir("workspace-store-invalid");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        std::fs::create_dir_all(&directory).expect("failed to create cache dir");
        std::fs::write(store.path(), "not json").expect("failed to write invalid cache");

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C123".to_string(),
                    ..Default::default()
                }])
                .await
                .expect("conversation store failed");

            assert_eq!(
                store
                    .load_conversations()
                    .await
                    .expect("conversation load failed")
                    .expect("missing cached conversations")[0]
                    .id,
                "C123"
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_merges_paged_history_newest_first() {
        let directory = temp_cache_dir("workspace-store-merged-history");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_history(
                    "C123",
                    &[
                        SlackMessage {
                            ts: "1710000300.000000".to_string(),
                            text: Some("new".to_string()),
                            ..Default::default()
                        },
                        SlackMessage {
                            ts: "1710000200.000000".to_string(),
                            text: Some("middle".to_string()),
                            ..Default::default()
                        },
                    ],
                )
                .await
                .expect("history store failed");

            store
                .store_merged_history(
                    "C123",
                    &[
                        SlackMessage {
                            ts: "1710000200.000000".to_string(),
                            text: Some("duplicate".to_string()),
                            ..Default::default()
                        },
                        SlackMessage {
                            ts: "1710000100.000000".to_string(),
                            text: Some("old".to_string()),
                            ..Default::default()
                        },
                    ],
                )
                .await
                .expect("merged history store failed");

            let messages = store
                .load_history("C123")
                .await
                .expect("history load failed")
                .expect("missing cached history");
            let timestamps = messages
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>();

            assert_eq!(
                timestamps,
                vec![
                    "1710000300.000000",
                    "1710000200.000000",
                    "1710000100.000000"
                ]
            );
            assert_eq!(
                messages
                    .iter()
                    .find(|message| message.ts == "1710000200.000000")
                    .and_then(|message| message.text.as_deref()),
                Some("duplicate")
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn stale_history_page_does_not_remove_newer_realtime_message() {
        let directory = temp_cache_dir("workspace-store-realtime-history-race");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_merged_history(
                    "D1",
                    &[SlackMessage {
                        ts: "5.0".to_string(),
                        text: Some("realtime".to_string()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            store
                .store_history(
                    "D1",
                    &[SlackMessage {
                        ts: "4.0".to_string(),
                        text: Some("stale page".to_string()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();

            let messages = store.load_history("D1").await.unwrap().unwrap();
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["5.0", "4.0"]
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn channel_history_filters_thread_replies_but_keeps_broadcasts() {
        let directory = temp_cache_dir("workspace-store-thread-routing");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let root = SlackMessage {
                ts: "1.0".into(),
                thread_ts: Some("1.0".into()),
                ..Default::default()
            };
            let reply = SlackMessage {
                ts: "2.0".into(),
                thread_ts: Some("1.0".into()),
                ..Default::default()
            };
            let mut broadcast = reply.clone();
            broadcast.ts = "3.0".into();
            broadcast.subtype = Some("thread_broadcast".into());

            store
                .store_merged_history("C1", &[root.clone(), reply.clone(), broadcast.clone()])
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
                vec!["3.0", "1.0"]
            );

            // Loading also sanitizes caches written by older Conduit versions.
            store
                .store_kind_map(
                    "channel_history",
                    HashMap::from([("C2".into(), vec![root, reply, broadcast])]),
                    false,
                )
                .await
                .unwrap();
            assert_eq!(
                store
                    .load_history("C2")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["3.0", "1.0"]
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn stale_thread_snapshot_keeps_newer_realtime_reply() {
        let directory = temp_cache_dir("workspace-store-realtime-thread-race");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_merged_thread(
                    "C1",
                    "1.0",
                    &[SlackMessage {
                        ts: "2.0".into(),
                        thread_ts: Some("1.0".into()),
                        text: Some("realtime reply".into()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            store
                .store_thread(
                    "C1",
                    "1.0",
                    &[SlackMessage {
                        ts: "1.0".into(),
                        text: Some("stale parent".into()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();

            assert_eq!(
                store
                    .load_thread("C1", "1.0")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["2.0", "1.0"]
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_prunes_cached_history_to_recent_bound() {
        let directory = temp_cache_dir("workspace-store-pruned-history");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let messages = (0..=MAX_CACHED_CHANNEL_MESSAGES)
                .map(|index| SlackMessage {
                    ts: format!("1710000{:03}.000000", MAX_CACHED_CHANNEL_MESSAGES - index),
                    text: Some(format!("message {index}")),
                    ..Default::default()
                })
                .collect::<Vec<_>>();

            store
                .store_history("C123", &messages)
                .await
                .expect("history store failed");

            let cached = store
                .load_history("C123")
                .await
                .expect("history load failed")
                .expect("missing cached history");

            assert_eq!(cached.len(), MAX_CACHED_CHANNEL_MESSAGES);
            assert_eq!(cached[0].ts, "1710000200.000000");
            assert_eq!(
                cached.last().map(|message| message.ts.as_str()),
                Some("1710000001.000000")
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_round_trips_thread_catalog() {
        use crate::thread_catalog::ThreadCatalog;

        let directory = temp_cache_dir("workspace-store-thread-catalog");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let mut catalog = ThreadCatalog::default();
            let root = SlackMessage {
                ts: "1710000000.000100".into(),
                reply_count: Some(3),
                subscribed: Some(true),
                unread_count: Some(2),
                last_read: Some("1710000100.000100".into()),
                latest_reply: Some("1710000300.000100".into()),
                ..Default::default()
            };
            catalog.observe_thread("C123", &root.ts.clone(), &[root], false);
            let records = catalog.into_records();
            store
                .store_thread_catalog(&records)
                .await
                .expect("thread catalog store failed");

            assert_eq!(
                store
                    .load_thread_catalog()
                    .await
                    .expect("thread catalog load failed"),
                records
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn marking_thread_read_can_persist_only_its_parent_attention_count() {
        use crate::thread_catalog::ThreadCatalog;

        let directory = temp_cache_dir("workspace-store-thread-read-attention");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let mut conversation = SlackConversation {
                id: "C123".into(),
                ..Default::default()
            };
            conversation.observe_attention_message_at("2.0", true);
            conversation.observe_attention_message_at("3.0", false);
            conversation.observe_attention_message_at("10.0", true);
            store.store_conversations(&[conversation]).await.unwrap();

            let mut catalog = ThreadCatalog::default();
            let root = SlackMessage {
                ts: "1.0".into(),
                subscribed: Some(true),
                unread_count: Some(2),
                latest_reply: Some("3.0".into()),
                last_read: Some("1.0".into()),
                ..Default::default()
            };
            let relevant_reply = SlackMessage {
                ts: "2.0".into(),
                thread_ts: Some("1.0".into()),
                user: Some("U2".into()),
                ..Default::default()
            };
            let filtered_reply = SlackMessage {
                ts: "3.0".into(),
                thread_ts: Some("1.0".into()),
                user: Some("U3".into()),
                ..Default::default()
            };
            catalog.observe_thread(
                "C123",
                "1.0",
                &[root, relevant_reply, filtered_reply],
                false,
            );
            store
                .store_thread_catalog(&catalog.into_records())
                .await
                .unwrap();

            let cleared_reply_ts = store.mark_thread_read("C123", "1.0", "3.0").await.unwrap();
            assert_eq!(cleared_reply_ts, vec!["2.0".to_string(), "3.0".to_string()]);

            let conversation = store
                .load_conversations()
                .await
                .unwrap()
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(conversation.unread_activity_count(), 1);
        });

        let _ = std::fs::remove_dir_all(directory);
    }
}
