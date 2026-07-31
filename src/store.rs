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
#[cfg(test)]
use crate::models::SlackUnreadState;
use crate::models::{
    slack_timestamp_is_after, SlackConversation, SlackConversationUnreadSnapshot, SlackMessage,
    SlackUser, SlackUserStatus,
};
use crate::slack_message_wire::normalize_cached_messages;
use crate::thread_catalog::ThreadRecord;
use crate::workspace_pipeline::{
    apply_reaction_projection_mutation, same_message_identity, AttentionDeliveryIdentity,
    ConversationAttentionObservation, MessageMutationKind, ReactionMutation,
    ReactionProjectionMutation, StoreBatch, StoreChange, WorkspaceRevision,
    WorkspaceStoreProjection,
};

pub(crate) const CACHE_VERSION: u32 = 1;
const DATABASE_SCHEMA_VERSION: u32 = 2;
const DATABASE_FILENAME: &str = "state.sqlite3";
const MAX_CACHED_CHANNEL_MESSAGES: usize = 200;
const ATTENTION_DELIVERY_KIND: &str = "attention_delivery";
const ATTENTION_DELIVERY_LEDGER_KEY: &str = "__ledger__";
const WORKSPACE_REPAIR_KIND: &str = "workspace_repair";
const WORKSPACE_REPAIR_USER_BASELINE_KEY: &str = "user_projection_baseline";
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
    pub(crate) fn rejected_update(message: impl Into<String>) -> Self {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotificationClaimOutcome {
    pub(crate) identity: AttentionDeliveryIdentity,
    pub(crate) notification_claimed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoreBatchOutcome {
    pub(crate) execution: StoreBatchExecution,
    pub(crate) notification_claims: Vec<NotificationClaimOutcome>,
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

struct StoreHubOpening {
    writer: Option<tokio::sync::mpsc::Sender<StoreWorkerRequest>>,
    readers: Vec<tokio::sync::mpsc::Sender<StoreWorkerRequest>>,
    workers: Vec<std::thread::JoinHandle<Result<()>>>,
}

impl StoreHubOpening {
    fn new(
        writer: tokio::sync::mpsc::Sender<StoreWorkerRequest>,
        worker: std::thread::JoinHandle<Result<()>>,
    ) -> Self {
        Self {
            writer: Some(writer),
            readers: Vec::with_capacity(STORE_READER_COUNT),
            workers: vec![worker],
        }
    }

    fn add_reader(
        &mut self,
        reader: tokio::sync::mpsc::Sender<StoreWorkerRequest>,
        worker: std::thread::JoinHandle<Result<()>>,
    ) {
        self.readers.push(reader);
        self.workers.push(worker);
    }

    fn finish(mut self, metrics: Arc<StoreMetrics>) -> StoreHub {
        StoreHub {
            inner: Arc::new(StoreHubInner {
                writer: self
                    .writer
                    .take()
                    .expect("store opening guard must own its writer"),
                readers: std::mem::take(&mut self.readers),
                next_reader: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                admission: tokio::sync::Mutex::new(()),
                workers: std::sync::Mutex::new(std::mem::take(&mut self.workers)),
                metrics,
            }),
        }
    }
}

impl Drop for StoreHubOpening {
    fn drop(&mut self) {
        // Closing every request channel lets workers that outlive a cancelled
        // startup finish cleanly before another hub retries schema creation.
        drop(self.writer.take());
        self.readers.clear();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
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
        let mut opening = StoreHubOpening::new(writer, writer_worker);
        writer_startup.await.map_err(|_| StoreError::HubClosed)??;

        for _ in 0..STORE_READER_COUNT {
            let (reader, startup, worker) = spawn_store_worker(
                directory.clone(),
                StoreConnectionKind::QueryOnly,
                STORE_READER_QUEUE_CAPACITY,
                Arc::clone(&metrics),
            );
            opening.add_reader(reader, worker);
            startup.await.map_err(|_| StoreError::HubClosed)??;
        }

        Ok(opening.finish(metrics))
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
    // Phase 6 owns retirement of the whole-state migration seam.
    #[allow(dead_code)]
    update_lock: Arc<Mutex<()>>,
    hub: Arc<tokio::sync::OnceCell<StoreHub>>,
    hub_migration: Arc<tokio::sync::OnceCell<()>>,
    hub_initialization_started: Arc<AtomicBool>,
    store_batch_revision: Arc<std::sync::Mutex<WorkspaceRevision>>,
    recovery_generation: Arc<AtomicU64>,
    workspace_reset_generation: Arc<AtomicU64>,
    workspace_repair_generation: Arc<AtomicU64>,
    recovery_linearization: Arc<tokio::sync::RwLock<()>>,
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
    pub(crate) reaction_actor_states: Vec<ReactionMutation>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SyncFreshness {
    pub(crate) refreshed_at_ms: Option<i64>,
    pub(crate) retry_count: u32,
    pub(crate) retry_after_ms: Option<i64>,
}

fn advance_recovery_generation_if_observed(
    recovery_generation: &AtomicU64,
    observed_generation: u64,
) -> Option<u64> {
    let next_generation = observed_generation.checked_add(1)?;
    recovery_generation
        .compare_exchange(
            observed_generation,
            next_generation,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .ok()
        .map(|_| next_generation)
}

impl WorkspaceStore {
    pub fn new(directory: PathBuf, workspace_id: &str) -> Self {
        Self {
            directory,
            workspace_id: workspace_id.to_string(),
            workspace_key: cache_key(workspace_id),
            update_lock: Arc::new(Mutex::new(())),
            hub: Arc::new(tokio::sync::OnceCell::new()),
            hub_migration: Arc::new(tokio::sync::OnceCell::new()),
            hub_initialization_started: Arc::new(AtomicBool::new(false)),
            store_batch_revision: Arc::new(std::sync::Mutex::new(WorkspaceRevision::INITIAL)),
            recovery_generation: Arc::new(AtomicU64::new(0)),
            workspace_reset_generation: Arc::new(AtomicU64::new(0)),
            workspace_repair_generation: Arc::new(AtomicU64::new(0)),
            recovery_linearization: Arc::new(tokio::sync::RwLock::new(())),
        }
    }

    async fn hub(&self) -> Result<&StoreHub> {
        // This remains true after cancellation so a later handoff barrier can
        // finish or retry a partially started first initialization.
        self.hub_initialization_started
            .store(true, Ordering::Release);
        let directory = self.directory.clone();
        let hub = self
            .hub
            .get_or_try_init(|| StoreHub::open(directory))
            .await?;

        // Publish the hub before migration work is queued. If this future is
        // cancelled, the owned worker pool remains available and the separate
        // cell retries the idempotent migration on the next access.
        let directory = self.directory.clone();
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        self.hub_migration
            .get_or_try_init(|| async {
                hub.write(move |connection| {
                    migrate_legacy_workspace(connection, &directory, &workspace_key, &workspace_id)
                })
                .await?;
                Ok::<(), StoreError>(())
            })
            .await?;
        Ok(hub)
    }

    pub(crate) async fn barrier(&self) -> Result<()> {
        if !self.hub_initialization_started.load(Ordering::Acquire) {
            return Ok(());
        }
        self.hub().await?.barrier().await
    }

    #[cfg(test)]
    pub(crate) async fn committed_transaction_count(&self) -> Result<u64> {
        Ok(self.hub().await?.metrics().transactions)
    }

    /// Executes one coordinator batch on the existing writer queue.
    ///
    /// The gate is strictly increasing rather than contiguous while compatibility
    /// surfaces still produce unsubmitted revisions. Migrated runtime paths must
    /// serialize reducer assignment and this submission.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn execute_store_batch(
        &self,
        batch: StoreBatch,
    ) -> Result<StoreBatchExecution> {
        Ok(self
            .execute_store_batch_inner(batch, false)
            .await?
            .execution)
    }

    pub(crate) async fn execute_store_batch_with_claims(
        &self,
        batch: StoreBatch,
    ) -> Result<StoreBatchOutcome> {
        self.execute_store_batch_inner(batch, false).await
    }

    /// Rebuilds a reset cache from the coordinator's complete current
    /// projection. An equal revision is accepted because an intervening delta
    /// may already have reached the newly empty cache.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn execute_store_repair_batch(
        &self,
        batch: StoreBatch,
    ) -> Result<StoreBatchExecution> {
        Ok(self.execute_store_batch_inner(batch, true).await?.execution)
    }

    pub(crate) async fn execute_store_repair_batch_with_claims(
        &self,
        batch: StoreBatch,
    ) -> Result<StoreBatchOutcome> {
        self.execute_store_batch_inner(batch, true).await
    }

    async fn execute_store_batch_inner(
        &self,
        batch: StoreBatch,
        accept_equal_revision: bool,
    ) -> Result<StoreBatchOutcome> {
        let revision = batch.revision();
        let notification_claims = batch.notification_claims();
        let changes = batch.changes().to_vec();
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let store_batch_revision = Arc::clone(&self.store_batch_revision);
        let recovery_generation = Arc::clone(&self.recovery_generation);
        self.hub()
            .await?
            .write(move |connection| {
                let mut persisted_revision = store_batch_revision.lock().map_err(|_| {
                    StoreError::Other(anyhow::anyhow!("store batch revision lock poisoned"))
                })?;
                if revision < *persisted_revision
                    || (!accept_equal_revision && revision == *persisted_revision)
                {
                    let notification_claims = known_notification_claim_outcomes(
                        connection,
                        &workspace_key,
                        notification_claims,
                    )?;
                    return Ok(StoreBatchOutcome {
                        execution: StoreBatchExecution::SkippedStale,
                        notification_claims,
                    });
                }

                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut changed =
                    ensure_sqlite_workspace(&transaction, &workspace_key, &workspace_id, false)?;
                let mut notification_claims = Vec::new();
                let repair_generation =
                    accept_equal_revision.then(|| recovery_generation.load(Ordering::Acquire));
                for change in changes {
                    match change {
                        StoreChange::AttentionNotificationClaim { identity } => {
                            match apply_attention_notification_claim(
                                &transaction,
                                &workspace_key,
                                &identity,
                            ) {
                                Ok((change_applied, notification_claimed)) => {
                                    changed |= change_applied;
                                    notification_claims.push(NotificationClaimOutcome {
                                        identity,
                                        notification_claimed,
                                    });
                                }
                                Err(error) => {
                                    let _ = transaction.rollback();
                                    return Err(error);
                                }
                            }
                        }
                        change => {
                            match apply_store_change(
                                &transaction,
                                &workspace_key,
                                &workspace_id,
                                repair_generation,
                                change,
                            ) {
                                Ok(change_applied) => changed |= change_applied,
                                Err(error) => {
                                    let _ = transaction.rollback();
                                    return Err(error);
                                }
                            }
                        }
                    }
                }
                let outcome = if finish_sqlite_transaction(transaction, changed)? {
                    StoreBatchExecution::Committed
                } else {
                    StoreBatchExecution::Unchanged
                };
                *persisted_revision = revision;
                Ok(StoreBatchOutcome {
                    execution: outcome,
                    notification_claims,
                })
            })
            .await
    }

    async fn query_or_reset<T, F>(&self, empty: T, query: F) -> Result<T>
    where
        T: Send + 'static,
        F: Fn(&mut Connection) -> Result<T> + Send + Sync + 'static,
    {
        let hub = self.hub().await?.clone();
        // Readers share this gate, while publication and repair take it
        // exclusively. A patch therefore cannot drain between observing
        // corrupt cache data and completing its atomic recheck/reset.
        let recovery_read = Arc::clone(&self.recovery_linearization).read_owned().await;
        let observed_generation = self.recovery_generation();
        let workspace_key = self.workspace_key.clone();
        let store_batch_revision = Arc::clone(&self.store_batch_revision);
        let recovery_generation = Arc::clone(&self.recovery_generation);
        let workspace_reset_generation = Arc::clone(&self.workspace_reset_generation);
        let query = Arc::new(query);
        // Transfer the complete query and recovery sequence to the runtime
        // before its first cancellation point. A cancelled caller therefore
        // cannot abandon known corruption during either worker admission.
        let operation = tokio::spawn(async move {
            let initial_query = Arc::clone(&query);
            let result = match hub.query(move |connection| initial_query(connection)).await {
                Err(error) if error.category() == StoreErrorCategory::CorruptData => {
                    let retry_query = Arc::clone(&query);
                    let writer_recovery_generation = Arc::clone(&recovery_generation);
                    let writer_reset_generation = Arc::clone(&workspace_reset_generation);
                    let recovered = hub
                        .write(move |connection| {
                            if writer_recovery_generation.load(Ordering::Acquire)
                                != observed_generation
                            {
                                return Ok(None);
                            }
                            match retry_query(connection) {
                                Ok(value) => Ok(Some(value)),
                                Err(error)
                                    if error.category() == StoreErrorCategory::CorruptData =>
                                {
                                    let mut persisted_revision =
                                        store_batch_revision.lock().map_err(|_| {
                                            StoreError::Other(anyhow::anyhow!(
                                                "store batch revision lock poisoned"
                                            ))
                                        })?;
                                    reset_sqlite_workspace(connection, &workspace_key)?;
                                    *persisted_revision = WorkspaceRevision::INITIAL;
                                    if let Some(reset_generation) =
                                        advance_recovery_generation_if_observed(
                                            &writer_recovery_generation,
                                            observed_generation,
                                        )
                                    {
                                        writer_reset_generation
                                            .fetch_max(reset_generation, Ordering::Release);
                                    }
                                    Ok(None)
                                }
                                Err(error) => Err(error),
                            }
                        })
                        .await;
                    match recovered {
                        Ok(Some(value)) => Ok(value),
                        Ok(None) => Ok(empty),
                        Err(error) => {
                            // The reader established corruption, but writer
                            // dispatch, revalidation, or reset could not prove
                            // the current cache valid. Preserve repair intent
                            // while the shared recovery guard is still held.
                            advance_recovery_generation_if_observed(
                                &recovery_generation,
                                observed_generation,
                            );
                            Err(error)
                        }
                    }
                }
                result => result,
            };
            drop(recovery_read);
            result
        });
        operation.await.map_err(|error| {
            StoreError::Other(anyhow::anyhow!(
                "workspace cache query task failed: {error}"
            ))
        })?
    }

    pub(crate) fn recovery_generation(&self) -> u64 {
        self.recovery_generation.load(Ordering::Acquire)
    }

    pub(crate) fn workspace_cache_needs_repair(&self) -> bool {
        self.workspace_repair_generation.load(Ordering::Acquire) < self.recovery_generation()
    }

    pub(crate) fn workspace_cache_needs_reset(&self) -> bool {
        self.workspace_reset_generation.load(Ordering::Acquire) < self.recovery_generation()
    }

    pub(crate) fn mark_workspace_cache_repaired(&self, recovery_generation: u64) {
        if self.workspace_reset_generation.load(Ordering::Acquire) >= recovery_generation {
            self.workspace_repair_generation
                .fetch_max(recovery_generation, Ordering::AcqRel);
        }
    }

    pub(crate) async fn lock_recovery_linearization(
        &self,
    ) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        Arc::clone(&self.recovery_linearization).write_owned().await
    }

    pub(crate) async fn ensure_workspace_cache_reset_for_repair(
        &self,
        expected_recovery_generation: u64,
    ) -> Result<bool> {
        if self.recovery_generation() != expected_recovery_generation {
            return Ok(false);
        }
        if self.workspace_reset_generation.load(Ordering::Acquire) >= expected_recovery_generation {
            return Ok(true);
        }

        let _recovery = self.lock_recovery_linearization().await;
        if self.recovery_generation() != expected_recovery_generation {
            return Ok(false);
        }
        if self.workspace_reset_generation.load(Ordering::Acquire) >= expected_recovery_generation {
            return Ok(true);
        }

        let workspace_key = self.workspace_key.clone();
        let store_batch_revision = Arc::clone(&self.store_batch_revision);
        let workspace_reset_generation = Arc::clone(&self.workspace_reset_generation);
        self.hub()
            .await?
            .write(move |connection| {
                let mut persisted_revision = store_batch_revision.lock().map_err(|_| {
                    StoreError::Other(anyhow::anyhow!("store batch revision lock poisoned"))
                })?;
                reset_sqlite_workspace(connection, &workspace_key)?;
                *persisted_revision = WorkspaceRevision::INITIAL;
                workspace_reset_generation
                    .fetch_max(expected_recovery_generation, Ordering::Release);
                Ok(())
            })
            .await?;
        Ok(true)
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

    pub(crate) async fn load_bootstrap(&self) -> Result<Option<WorkspaceBootstrap>> {
        Ok(self.load_state().await?.map(WorkspaceBootstrap::from))
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
                .query_map([workspace_key.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(StoreError::from)?;
            let mut queue = Vec::new();
            let mut legacy = Vec::new();
            for (item_key, payload) in rows {
                if item_key == PENDING_UNREAD_QUEUE_KEY {
                    queue.extend(serde_json::from_str::<Vec<String>>(&payload)?);
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

    pub async fn store_user_statuses(
        &self,
        statuses: &HashMap<String, SlackUserStatus>,
    ) -> Result<()> {
        self.store_kind_map("user_status", statuses.clone(), true)
            .await
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

    #[allow(dead_code)]
    async fn update_state(&self, update: impl FnOnce(&mut CachedWorkspaceState)) -> Result<()> {
        let _guard = self.update_lock.lock().await;
        let mut state = self.load_state_for_update().await?;
        state.workspace_id = self.workspace_id.clone();
        update(&mut state);
        self.store_state(&state).await
    }

    async fn load_state(&self) -> Result<Option<CachedWorkspaceState>> {
        let workspace_key = self.workspace_key.clone();
        let result = self
            .query_or_reset(None, move |connection| {
                load_sqlite_state(connection, &workspace_key)
            })
            .await;
        if let Err(error) = &result {
            crate::debug::log(
                "store",
                &format!("WorkspaceCacheReadFailed category={:?}", error.category()),
            );
        }
        result
    }

    #[allow(dead_code)]
    async fn load_state_for_update(&self) -> Result<CachedWorkspaceState> {
        let mut state = self
            .load_state()
            .await?
            .unwrap_or_else(CachedWorkspaceState::new);
        state.workspace_id = self.workspace_id.clone();
        Ok(state)
    }

    #[allow(dead_code)]
    async fn store_state(&self, state: &CachedWorkspaceState) -> Result<()> {
        self.store_state_with_activation(state, false).await
    }

    #[allow(dead_code)]
    async fn store_state_with_activation(
        &self,
        state: &CachedWorkspaceState,
        activate: bool,
    ) -> Result<()> {
        let workspace_key = self.workspace_key.clone();
        let state = state.clone();
        self.hub()
            .await?
            .write(move |connection| {
                store_sqlite_state(connection, &workspace_key, &state, activate)
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

    #[cfg(test)]
    pub(crate) async fn corrupt_cached_item_payload(
        &self,
        kind: &str,
        item_key: &str,
    ) -> Result<()> {
        let workspace_key = self.workspace_key.clone();
        let kind = kind.to_string();
        let item_key = item_key.to_string();
        self.hub()
            .await?
            .write(move |connection| {
                let changed = connection.execute(
                    "UPDATE workspace_items SET payload_json = '{'
                     WHERE workspace_key = ?1 AND kind = ?2 AND item_key = ?3",
                    params![workspace_key, kind, item_key],
                )?;
                if changed != 1 {
                    return Err(StoreError::rejected_update(
                        "test cache item payload was not found",
                    ));
                }
                Ok(())
            })
            .await
    }

    #[cfg(test)]
    pub(crate) async fn install_workspace_reset_failure_trigger(&self) -> Result<()> {
        self.hub()
            .await?
            .write(|connection| {
                connection.execute_batch(
                    "CREATE TEMP TRIGGER conduit_test_fail_workspace_reset
                     BEFORE DELETE ON workspaces
                     BEGIN
                         SELECT RAISE(ABORT, 'injected workspace reset failure');
                     END;",
                )?;
                Ok(())
            })
            .await
    }

    #[cfg(test)]
    pub(crate) async fn clear_workspace_reset_failure_trigger(&self) -> Result<()> {
        self.hub()
            .await?
            .write(|connection| {
                connection
                    .execute_batch("DROP TRIGGER IF EXISTS conduit_test_fail_workspace_reset;")?;
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
struct CachedWorkspaceState {
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
    #[serde(default)]
    reaction_actor_states: Vec<ReactionMutation>,
}

impl CachedWorkspaceState {
    fn new() -> Self {
        Self {
            version: CACHE_VERSION,
            workspace_id: String::new(),
            conversations: Vec::new(),
            user_names: HashMap::new(),
            user_full_names: HashMap::new(),
            user_avatar_urls: HashMap::new(),
            user_search_aliases: HashMap::new(),
            user_statuses: HashMap::new(),
            channel_histories: HashMap::new(),
            thread_replies: HashMap::new(),
            thread_catalog: Vec::new(),
            pending_unread_refresh: Vec::new(),
            custom_emojis: HashMap::new(),
            attention_deliveries: Vec::new(),
            reaction_actor_states: Vec::new(),
        }
    }
}

impl From<CachedWorkspaceState> for WorkspaceBootstrap {
    fn from(state: CachedWorkspaceState) -> Self {
        Self {
            workspace_id: state.workspace_id,
            conversations: state.conversations,
            user_names: state.user_names,
            user_full_names: state.user_full_names,
            user_avatar_urls: state.user_avatar_urls,
            user_search_aliases: state.user_search_aliases,
            user_statuses: state.user_statuses,
            thread_catalog: state.thread_catalog,
            custom_emojis: state.custom_emojis,
            reaction_actor_states: state.reaction_actor_states,
        }
    }
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

fn load_sqlite_state(
    connection: &Connection,
    workspace_key: &str,
) -> Result<Option<CachedWorkspaceState>> {
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

    let mut state = CachedWorkspaceState::new();
    state.workspace_id = workspace_id;
    let mut statement = connection.prepare(
        "SELECT kind, item_key, payload_json
         FROM workspace_items WHERE workspace_key = ?1 ORDER BY kind, item_key",
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
            "user_avatar_url" => {
                state.user_avatar_urls.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user avatar URL")?,
                );
            }
            "user_aliases" => {
                state.user_search_aliases.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user aliases")?,
                );
            }
            "user_status" => {
                state.user_statuses.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user status")?,
                );
            }
            "channel_history" => {
                state.channel_histories.insert(
                    item_key,
                    normalize_cached_messages(
                        serde_json::from_str(&payload).context("invalid cached channel history")?,
                    ),
                );
            }
            "thread_replies" => {
                state.thread_replies.insert(
                    item_key,
                    normalize_cached_messages(
                        serde_json::from_str(&payload).context("invalid cached thread replies")?,
                    ),
                );
            }
            "thread_record" => state
                .thread_catalog
                .push(serde_json::from_str(&payload).context("invalid cached thread record")?),
            "reaction_actor_state" => state.reaction_actor_states.push(
                serde_json::from_str(&payload).context("invalid cached reaction actor state")?,
            ),
            "pending_unread" if item_key == PENDING_UNREAD_QUEUE_KEY => {
                state.pending_unread_refresh.extend(
                    serde_json::from_str::<Vec<String>>(&payload)
                        .context("invalid cached pending unread queue")?,
                );
            }
            "pending_unread" => state.pending_unread_refresh.push(item_key),
            "custom_emoji" => {
                state.custom_emojis.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached custom emoji")?,
                );
            }
            ATTENTION_DELIVERY_KIND if item_key == ATTENTION_DELIVERY_LEDGER_KEY => {
                state.attention_deliveries.extend(
                    serde_json::from_str::<Vec<String>>(&payload)
                        .context("invalid cached attention delivery ledger")?,
                );
            }
            _ => {}
        }
    }
    state.pending_unread_refresh = normalized_pending_unread_queue(state.pending_unread_refresh);
    Ok(Some(state))
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

fn store_sqlite_state(
    connection: &mut Connection,
    workspace_key: &str,
    state: &CachedWorkspaceState,
    activate: bool,
) -> Result<()> {
    let desired = state_items(state)?;
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
    repair_generation: Option<u64>,
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
            changed |=
                sync_reaction_actor_states(transaction, workspace_key, data.reaction_actor_states)?;
            Ok(changed)
        }
        StoreChange::WorkspaceRepaired(WorkspaceStoreProjection {
            conversations,
            users,
            histories,
            thread_timelines,
            thread_catalog,
            reaction_actor_states,
        }) => {
            let repair_generation = repair_generation.ok_or_else(|| {
                StoreError::rejected_update(
                    "workspace repair changes require the repair execution path",
                )
            })?;
            let mut changed = sync_conversations(transaction, workspace_key, conversations)?;
            let mut user_projections = CachedUserProjections::default();
            for user in users {
                user_projections.insert(user)?;
            }
            let prior_user_baseline = load_sqlite_item::<WorkspaceRepairUserBaseline>(
                transaction,
                workspace_key,
                WORKSPACE_REPAIR_KIND,
                WORKSPACE_REPAIR_USER_BASELINE_KEY,
            )?
            .filter(|baseline| baseline.recovery_generation == repair_generation);
            changed |= merge_repaired_user_projections(
                transaction,
                workspace_key,
                &user_projections,
                prior_user_baseline
                    .as_ref()
                    .map(|baseline| &baseline.projections),
            )?;
            changed |= sync_sqlite_kind(
                transaction,
                workspace_key,
                "channel_history",
                histories
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
            changed |= sync_sqlite_kind(
                transaction,
                workspace_key,
                "thread_replies",
                thread_timelines
                    .into_iter()
                    .map(|((channel_id, thread_ts), messages)| {
                        require_store_key("thread channel", &channel_id)?;
                        require_store_key("thread timestamp", &thread_ts)?;
                        Ok((
                            thread_key(&channel_id, &thread_ts),
                            pruned_history(normalize_cached_messages(messages)),
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
            )?;
            changed |= sync_thread_catalog(transaction, workspace_key, thread_catalog)?;
            changed |=
                sync_reaction_actor_states(transaction, workspace_key, reaction_actor_states)?;
            changed |= upsert_sqlite_item(
                transaction,
                workspace_key,
                WORKSPACE_REPAIR_KIND,
                WORKSPACE_REPAIR_USER_BASELINE_KEY,
                &WorkspaceRepairUserBaseline {
                    recovery_generation: repair_generation,
                    projections: user_projections,
                },
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
        StoreChange::AttentionNotificationClaim { .. } => {
            unreachable!("notification claims are applied with keyed batch outcomes")
        }
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
        StoreChange::ReactionChanged(projection) => {
            require_store_key("reaction channel", &projection.change.channel_id)?;
            require_store_key("reaction message timestamp", &projection.change.message_ts)?;
            require_store_key("reaction name", &projection.change.name)?;
            require_store_key("reaction user", &projection.change.user_id)?;
            apply_reaction_delta(transaction, workspace_key, projection)
        }
        StoreChange::ReactionActorStatesReplaced(states)
        | StoreChange::ReactionActorStatesRepaired(states) => {
            sync_reaction_actor_states(transaction, workspace_key, states)
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
            replace_thread_and_channel_root(
                transaction,
                workspace_key,
                &channel_id,
                &thread_ts,
                messages,
            )
        }
        StoreChange::ThreadCatalogReplaced(records) => {
            sync_thread_catalog(transaction, workspace_key, records)
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

fn apply_reaction_delta(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    projection: ReactionProjectionMutation,
) -> Result<bool> {
    let existing_history = load_sqlite_item::<Vec<SlackMessage>>(
        transaction,
        workspace_key,
        "channel_history",
        &projection.change.channel_id,
    )?;
    let mut changed = false;
    if let Some(existing_history) = existing_history {
        let mut history = channel_timeline_messages(normalize_cached_messages(existing_history));
        let history_changed = history
            .iter_mut()
            .filter(|message| message.ts == projection.change.message_ts)
            .fold(false, |changed, message| {
                apply_reaction_projection_mutation(message, &projection) || changed
            });
        if history_changed {
            changed |= replace_timeline_item(
                transaction,
                workspace_key,
                "channel_history",
                &projection.change.channel_id,
                history,
            )?;
        }
    }

    for mut thread in
        load_sqlite_channel_threads(transaction, workspace_key, &projection.change.channel_id)?
    {
        let thread_changed = thread
            .messages
            .iter_mut()
            .filter(|message| message.ts == projection.change.message_ts)
            .fold(false, |changed, message| {
                apply_reaction_projection_mutation(message, &projection) || changed
            });
        if thread_changed {
            changed |= replace_timeline_item(
                transaction,
                workspace_key,
                "thread_replies",
                &thread.item_key,
                thread.messages,
            )?;
        }
    }

    let mut records =
        load_sqlite_kind_values::<ThreadRecord>(transaction, workspace_key, "thread_record")?;
    let mut records_changed = false;
    for root in records
        .iter_mut()
        .filter(|record| record.key.channel_id == projection.change.channel_id)
        .filter_map(|record| record.root.as_mut())
        .filter(|root| root.ts == projection.change.message_ts)
    {
        records_changed |= apply_reaction_projection_mutation(root, &projection);
    }
    if records_changed {
        changed |= sync_thread_records(transaction, workspace_key, records)?;
    }
    Ok(changed)
}

fn replace_thread_and_channel_root(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    channel_id: &str,
    thread_ts: &str,
    messages: Vec<SlackMessage>,
) -> Result<bool> {
    let messages = pruned_history(normalize_cached_messages(messages));
    let mut changed = upsert_sqlite_item(
        transaction,
        workspace_key,
        "thread_replies",
        &thread_key(channel_id, thread_ts),
        &messages,
    )?;
    let Some(root) = messages.iter().find(|message| message.ts == thread_ts) else {
        return Ok(changed);
    };
    let Some(history) = load_sqlite_item::<Vec<SlackMessage>>(
        transaction,
        workspace_key,
        "channel_history",
        channel_id,
    )?
    else {
        return Ok(changed);
    };
    let mut history = channel_timeline_messages(normalize_cached_messages(history));
    let original = history.clone();
    for channel_root in history.iter_mut().filter(|message| message.ts == thread_ts) {
        channel_root.reply_count = root.reply_count;
        channel_root.latest_reply.clone_from(&root.latest_reply);
        channel_root.reply_users.clone_from(&root.reply_users);
        channel_root.subscribed = root.subscribed;
        channel_root.unread_count = root.unread_count;
        channel_root.last_read.clone_from(&root.last_read);
    }
    if history != original {
        changed |= upsert_sqlite_item(
            transaction,
            workspace_key,
            "channel_history",
            channel_id,
            &history,
        )?;
    }
    Ok(changed)
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CachedUserProjections {
    display_names: BTreeMap<String, String>,
    full_names: BTreeMap<String, String>,
    avatar_urls: BTreeMap<String, String>,
    aliases: BTreeMap<String, Vec<String>>,
    statuses: BTreeMap<String, SlackUserStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkspaceRepairUserBaseline {
    recovery_generation: u64,
    projections: CachedUserProjections,
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

fn merge_repaired_user_projections(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    desired: &CachedUserProjections,
    previous: Option<&CachedUserProjections>,
) -> Result<bool> {
    let mut changed = merge_repaired_user_kind(
        transaction,
        workspace_key,
        "user_name",
        &desired.display_names,
        previous.map(|projections| &projections.display_names),
    )?;
    changed |= merge_repaired_user_kind(
        transaction,
        workspace_key,
        "user_full_name",
        &desired.full_names,
        previous.map(|projections| &projections.full_names),
    )?;
    changed |= merge_repaired_user_kind(
        transaction,
        workspace_key,
        "user_avatar_url",
        &desired.avatar_urls,
        previous.map(|projections| &projections.avatar_urls),
    )?;
    changed |= merge_repaired_user_kind(
        transaction,
        workspace_key,
        "user_aliases",
        &desired.aliases,
        previous.map(|projections| &projections.aliases),
    )?;
    changed |= merge_repaired_user_kind(
        transaction,
        workspace_key,
        "user_status",
        &desired.statuses,
        previous.map(|projections| &projections.statuses),
    )?;
    Ok(changed)
}

fn merge_repaired_user_kind<T: Serialize>(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    kind: &str,
    desired: &BTreeMap<String, T>,
    previous: Option<&BTreeMap<String, T>>,
) -> Result<bool> {
    let desired = serialize_repaired_user_kind(desired)?;
    let previous = previous.map(serialize_repaired_user_kind).transpose()?;
    let keys = desired
        .keys()
        .chain(previous.iter().flat_map(|values| values.keys()))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut changed = false;
    for item_key in keys {
        let current = transaction
            .query_row(
                "SELECT payload_json FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = ?2 AND item_key = ?3",
                params![workspace_key, kind, item_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let desired_payload = desired.get(&item_key);
        let Some(previous) = previous.as_ref() else {
            if let (None, Some(desired_payload)) = (current.as_ref(), desired_payload) {
                changed |= insert_sqlite_payload_if_absent(
                    transaction,
                    workspace_key,
                    kind,
                    &item_key,
                    desired_payload,
                )?;
            }
            continue;
        };
        let Some(previous_payload) = previous.get(&item_key) else {
            if let (None, Some(desired_payload)) = (current.as_ref(), desired_payload) {
                changed |= insert_sqlite_payload_if_absent(
                    transaction,
                    workspace_key,
                    kind,
                    &item_key,
                    desired_payload,
                )?;
            }
            continue;
        };
        if current.as_ref() != Some(previous_payload) {
            continue;
        }
        changed |= match desired_payload {
            Some(desired_payload) if desired_payload != previous_payload => {
                transaction.execute(
                    "UPDATE workspace_items
                     SET payload_json = ?4
                     WHERE workspace_key = ?1
                       AND kind = ?2
                       AND item_key = ?3
                       AND payload_json = ?5",
                    params![
                        workspace_key,
                        kind,
                        item_key,
                        desired_payload,
                        previous_payload
                    ],
                )? > 0
            }
            Some(_) => false,
            None => {
                transaction.execute(
                    "DELETE FROM workspace_items
                     WHERE workspace_key = ?1
                       AND kind = ?2
                       AND item_key = ?3
                       AND payload_json = ?4",
                    params![workspace_key, kind, item_key, previous_payload],
                )? > 0
            }
        };
    }
    Ok(changed)
}

fn serialize_repaired_user_kind<T: Serialize>(
    values: &BTreeMap<String, T>,
) -> Result<BTreeMap<String, String>> {
    values
        .iter()
        .map(|(item_key, value)| {
            Ok((
                item_key.clone(),
                serde_json::to_string(value).context("failed to serialize cached user item")?,
            ))
        })
        .collect()
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

fn sync_reaction_actor_states(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    states: Vec<ReactionMutation>,
) -> Result<bool> {
    let states = states
        .into_iter()
        .map(|state| {
            require_store_key("reaction channel", &state.channel_id)?;
            require_store_key("reaction message timestamp", &state.message_ts)?;
            require_store_key("reaction name", &state.name)?;
            require_store_key("reaction user", &state.user_id)?;
            Ok((reaction_actor_state_key(&state), state))
        })
        .collect::<Result<Vec<_>>>()?;
    sync_sqlite_kind(transaction, workspace_key, "reaction_actor_state", states)
}

fn sync_thread_catalog(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    records: Vec<ThreadRecord>,
) -> Result<bool> {
    let root_projections_changed =
        sync_thread_catalog_root_projections(transaction, workspace_key, &records)?;
    let records_changed = sync_thread_records(transaction, workspace_key, records)?;
    Ok(root_projections_changed || records_changed)
}

fn sync_thread_catalog_root_projections(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    records: &[ThreadRecord],
) -> Result<bool> {
    let mut records_by_channel = BTreeMap::<&str, Vec<&ThreadRecord>>::new();
    for record in records {
        records_by_channel
            .entry(record.key.channel_id.as_str())
            .or_default()
            .push(record);
    }

    let mut changed = false;
    for (channel_id, channel_records) in records_by_channel {
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
        let mut threads = load_sqlite_channel_threads(transaction, workspace_key, channel_id)?;

        for record in channel_records {
            let Some(root) = record.root.as_ref() else {
                continue;
            };
            replace_root_catalog_metadata(&mut history, &mut threads, &record.key.root_ts, root);
        }

        if history != original_history {
            changed |= replace_timeline_item(
                transaction,
                workspace_key,
                "channel_history",
                channel_id,
                history,
            )?;
        }
        for thread in threads {
            if thread.messages != thread.original {
                changed |= replace_timeline_item(
                    transaction,
                    workspace_key,
                    "thread_replies",
                    &thread.item_key,
                    thread.messages,
                )?;
            }
        }
    }
    Ok(changed)
}

fn replace_root_catalog_metadata(
    history: &mut [SlackMessage],
    threads: &mut [StoredThreadTimeline],
    root_ts: &str,
    root: &SlackMessage,
) {
    let replace = |message: &mut SlackMessage| {
        message.reply_count = root.reply_count;
        message.latest_reply.clone_from(&root.latest_reply);
        message.reply_users.clone_from(&root.reply_users);
        message.subscribed = root.subscribed;
        message.unread_count = root.unread_count;
        message.last_read.clone_from(&root.last_read);
    };
    for message in history.iter_mut().filter(|message| message.ts == root_ts) {
        replace(message);
    }
    for message in threads
        .iter_mut()
        .filter(|thread| thread.root_ts == root_ts)
        .flat_map(|thread| thread.messages.iter_mut())
        .filter(|message| message.ts == root_ts)
    {
        replace(message);
    }
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

fn insert_sqlite_payload_if_absent(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    kind: &str,
    item_key: &str,
    payload: &str,
) -> Result<bool> {
    Ok(transaction.execute(
        "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(workspace_key, kind, item_key) DO NOTHING",
        params![workspace_key, kind, item_key, payload],
    )? > 0)
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

fn state_items(state: &CachedWorkspaceState) -> Result<HashMap<(String, String), String>> {
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
    for state in &state.reaction_actor_states {
        insert_state_item(
            &mut items,
            "reaction_actor_state",
            reaction_actor_state_key(state),
            state,
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

fn apply_attention_notification_claim(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    identity: &AttentionDeliveryIdentity,
) -> Result<(bool, bool)> {
    let identity_key = attention_delivery_identity(&identity.channel_id, &identity.message_ts)
        .ok_or_else(|| StoreError::rejected_update("invalid attention delivery identity"))?;
    let mut ledger = load_sqlite_item::<Vec<String>>(
        transaction,
        workspace_key,
        ATTENTION_DELIVERY_KIND,
        ATTENTION_DELIVERY_LEDGER_KEY,
    )?
    .unwrap_or_default();
    if ledger.iter().any(|known| known == &identity_key) {
        return Ok((false, false));
    }
    ledger.push(identity_key);
    if ledger.len() > MAX_ATTENTION_DELIVERIES {
        ledger.drain(..ledger.len() - MAX_ATTENTION_DELIVERIES);
    }
    let changed = upsert_sqlite_item(
        transaction,
        workspace_key,
        ATTENTION_DELIVERY_KIND,
        ATTENTION_DELIVERY_LEDGER_KEY,
        &ledger,
    )?;
    Ok((changed, true))
}

fn known_notification_claim_outcomes(
    connection: &Connection,
    workspace_key: &str,
    identities: Vec<AttentionDeliveryIdentity>,
) -> Result<Vec<NotificationClaimOutcome>> {
    if identities.is_empty() {
        return Ok(Vec::new());
    }
    let ledger = load_sqlite_item::<Vec<String>>(
        connection,
        workspace_key,
        ATTENTION_DELIVERY_KIND,
        ATTENTION_DELIVERY_LEDGER_KEY,
    )?
    .unwrap_or_default();
    Ok(identities
        .into_iter()
        .filter(|identity| {
            attention_delivery_identity(&identity.channel_id, &identity.message_ts)
                .is_some_and(|identity| ledger.iter().any(|known| known == &identity))
        })
        .map(|identity| NotificationClaimOutcome {
            identity,
            notification_claimed: false,
        })
        .collect())
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
    store_sqlite_state(connection, workspace_key, &state, false)?;
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
        store_sqlite_state(connection, &workspace_key, &state, true)?;
        remove_legacy_workspace_files(directory, &workspace_key);
        let _ = std::fs::remove_file(directory.join("active-workspace"));
    }
    Ok(())
}

fn remove_legacy_workspace_files(directory: &Path, workspace_key: &str) {
    let _ = std::fs::remove_file(directory.join(format!("{workspace_key}.json")));
    let _ = std::fs::remove_file(directory.join(format!("{workspace_key}.search.json")));
}

fn legacy_states(directory: &Path) -> Result<Vec<(String, CachedWorkspaceState)>> {
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
) -> Result<Option<CachedWorkspaceState>> {
    let path = directory.join(format!("{workspace_key}.json"));
    let data = match std::fs::read_to_string(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let state = match serde_json::from_str::<CachedWorkspaceState>(&data) {
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

fn reaction_actor_state_key(state: &ReactionMutation) -> String {
    cache_key(&format!(
        "{}\u{0}{}\u{0}{}\u{0}{}",
        state.channel_id, state.message_ts, state.name, state.user_id
    ))
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
    use crate::thread_catalog::ThreadCatalog;
    use crate::workspace_pipeline::{
        MessageMutationKind, MutationOrigin, ReactionMutation, ReactionProjectionCount,
        ReactionProjectionMutation, StoreBatch, StoreChange, WorkspaceBootstrapData,
        WorkspaceCoordinator, WorkspaceMutation, WorkspaceRevision, WorkspaceStoreProjection,
    };

    async fn apply_test_store_changes(store: &WorkspaceStore, changes: Vec<StoreChange>) {
        // Persistent fixture setup must use the coordinator-native executor without
        // consuming the revision that the behavior under test will submit.
        let previous_revision = *store
            .store_batch_revision
            .lock()
            .expect("store revision lock poisoned");
        let revision = previous_revision.successor();
        let batch = StoreBatch::new(revision, changes).expect("test store batch has changes");
        let outcome = store.execute_store_batch(batch).await;
        *store
            .store_batch_revision
            .lock()
            .expect("store revision lock poisoned") = previous_revision;
        outcome.unwrap();
    }

    async fn seed_test_conversations(
        store: &WorkspaceStore,
        conversations: Vec<SlackConversation>,
    ) {
        apply_test_store_changes(
            store,
            vec![StoreChange::ConversationsReplaced(conversations)],
        )
        .await;
    }

    async fn seed_test_history(
        store: &WorkspaceStore,
        channel_id: &str,
        messages: Vec<SlackMessage>,
    ) {
        apply_test_store_changes(
            store,
            vec![StoreChange::HistoryReplaced {
                channel_id: channel_id.to_string(),
                messages,
            }],
        )
        .await;
    }

    async fn seed_test_thread(
        store: &WorkspaceStore,
        channel_id: &str,
        thread_ts: &str,
        messages: Vec<SlackMessage>,
    ) {
        apply_test_store_changes(
            store,
            vec![StoreChange::ThreadReplaced {
                channel_id: channel_id.to_string(),
                thread_ts: thread_ts.to_string(),
                messages,
            }],
        )
        .await;
    }

    async fn seed_test_thread_catalog(store: &WorkspaceStore, records: Vec<ThreadRecord>) {
        apply_test_store_changes(store, vec![StoreChange::ThreadCatalogReplaced(records)]).await;
    }

    async fn test_conversations(store: &WorkspaceStore) -> Result<Option<Vec<SlackConversation>>> {
        Ok(store
            .load_bootstrap()
            .await?
            .map(|bootstrap| bootstrap.conversations)
            .filter(|conversations| !conversations.is_empty()))
    }

    async fn test_thread_catalog(store: &WorkspaceStore) -> Result<Vec<ThreadRecord>> {
        Ok(store
            .load_bootstrap()
            .await?
            .map(|bootstrap| bootstrap.thread_catalog)
            .unwrap_or_default())
    }

    trait WorkspaceStoreTestExt {
        async fn stored_conversations(&self) -> Result<Option<Vec<SlackConversation>>>;
        async fn seed_conversations(&self, conversations: &[SlackConversation]) -> Result<()>;
        async fn seed_history(&self, channel_id: &str, messages: &[SlackMessage]) -> Result<()>;
        async fn seed_thread(
            &self,
            channel_id: &str,
            thread_ts: &str,
            messages: &[SlackMessage],
        ) -> Result<()>;
        async fn stored_thread_catalog(&self) -> Result<Vec<ThreadRecord>>;
        async fn seed_thread_catalog(&self, records: &[ThreadRecord]) -> Result<()>;
        async fn seed_read_cursor(&self, channel_id: &str, ts: &str) -> Result<bool>;
    }

    impl WorkspaceStoreTestExt for WorkspaceStore {
        async fn stored_conversations(&self) -> Result<Option<Vec<SlackConversation>>> {
            test_conversations(self).await
        }

        async fn seed_conversations(&self, conversations: &[SlackConversation]) -> Result<()> {
            seed_test_conversations(self, conversations.to_vec()).await;
            Ok(())
        }

        async fn seed_history(&self, channel_id: &str, messages: &[SlackMessage]) -> Result<()> {
            seed_test_history(self, channel_id, messages.to_vec()).await;
            Ok(())
        }

        async fn seed_thread(
            &self,
            channel_id: &str,
            thread_ts: &str,
            messages: &[SlackMessage],
        ) -> Result<()> {
            seed_test_thread(self, channel_id, thread_ts, messages.to_vec()).await;
            Ok(())
        }

        async fn stored_thread_catalog(&self) -> Result<Vec<ThreadRecord>> {
            test_thread_catalog(self).await
        }

        async fn seed_thread_catalog(&self, records: &[ThreadRecord]) -> Result<()> {
            seed_test_thread_catalog(self, records.to_vec()).await;
            Ok(())
        }

        async fn seed_read_cursor(&self, channel_id: &str, ts: &str) -> Result<bool> {
            let Some(mut conversation) = self
                .stored_conversations()
                .await?
                .unwrap_or_default()
                .into_iter()
                .find(|conversation| conversation.id == channel_id)
            else {
                return Ok(false);
            };
            let before = conversation.clone();
            conversation.advance_read_cursor(ts, 0);
            conversation.set_local_read_ts(ts);
            if conversation == before {
                return Ok(false);
            }
            apply_test_store_changes(self, vec![StoreChange::ConversationUpsert(conversation)])
                .await;
            Ok(true)
        }
    }

    #[test]
    fn legacy_direct_workspace_mutation_apis_are_removed() {
        let production = include_str!("store.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap()
            .0;
        for legacy_api in [
            "pub async fn load_conversations(",
            "pub async fn store_conversations(",
            "pub async fn reconcile_conversations(",
            "pub async fn store_conversation(",
            "pub async fn merge_conversation(",
            "pub async fn apply_conversation_unread_state(",
            "pub async fn apply_conversation_unread_snapshot(",
            "pub async fn advance_conversation_read_cursor(",
            "pub async fn clear_conversation_unread_state(",
            "pub async fn mark_conversation_unread_from_event(",
            "pub async fn observe_conversation_attention_from_event(",
            "pub async fn observe_conversation_attention_batch(",
            "pub async fn accept_attention_delivery(",
            "pub async fn claim_attention_delivery(",
            "pub async fn remove_conversation(",
            "pub async fn store_history(",
            "pub async fn store_merged_history(",
            "pub async fn store_thread(",
            "pub async fn store_merged_thread(",
            "pub async fn load_thread_catalog(",
            "pub async fn store_thread_catalog(",
            "pub async fn observe_thread_history(",
            "pub async fn observe_thread_page(",
            "pub async fn observe_thread_realtime(",
            "pub async fn mark_thread_read(",
        ] {
            assert!(
                !production.contains(legacy_api),
                "legacy direct persistence API remains: {legacy_api}"
            );
        }
    }

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
            let conversations = store.stored_conversations().await.unwrap().unwrap();
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
                    .stored_conversations()
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
    fn coordinator_workspace_repair_exactly_replaces_timelines_and_merges_users() {
        let directory = temp_cache_dir("coordinator-complete-store-repair");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let stale_root = SlackMessage {
                ts: "10.0".into(),
                ..Default::default()
            };
            let stale_reply = SlackMessage {
                ts: "11.0".into(),
                thread_ts: Some("10.0".into()),
                ..Default::default()
            };
            let mut stale_catalog = ThreadCatalog::default();
            stale_catalog.observe_history("C_STALE", &[stale_root.clone(), stale_reply.clone()]);
            store
                .seed_conversations(&[SlackConversation {
                    id: "C_STALE".into(),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store
                .store_user_names(&HashMap::from([(
                    "U_COMPAT".to_string(),
                    "Compatibility User".to_string(),
                )]))
                .await
                .unwrap();
            store
                .store_user_name("U1", "Newer Compatibility Name")
                .await
                .unwrap();
            let newer_status = SlackUserStatus {
                text: "Newer compatibility status".into(),
                emoji: ":wave:".into(),
                expiration: 0,
            };
            store
                .store_user_status("U1", Some(newer_status.clone()))
                .await
                .unwrap();
            store
                .seed_history("C_STALE", std::slice::from_ref(&stale_root))
                .await
                .unwrap();
            store
                .seed_thread("C_STALE", "10.0", &[stale_root.clone(), stale_reply])
                .await
                .unwrap();
            store
                .seed_thread_catalog(&stale_catalog.into_records())
                .await
                .unwrap();

            let conversation = SlackConversation {
                id: "C1".into(),
                unread_count: Some(3),
                extra: HashMap::from([
                    ("has_unreads".into(), serde_json::json!(true)),
                    ("last_read".into(), serde_json::json!("1.0")),
                ]),
                ..Default::default()
            };
            let user = SlackUser {
                id: Some("U1".into()),
                real_name: Some("Ada Lovelace".into()),
                profile: Some(crate::models::SlackUserProfile {
                    display_name: Some("Stale Coordinator Name".into()),
                    real_name: Some("Ada Lovelace".into()),
                    image_72: Some("https://example.test/coordinator.png".into()),
                    status_text: Some(String::new()),
                    status_emoji: Some(String::new()),
                    status_expiration: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let root = SlackMessage {
                ts: "2.0".into(),
                reply_count: Some(1),
                latest_reply: Some("3.0".into()),
                ..Default::default()
            };
            let reply = SlackMessage {
                ts: "3.0".into(),
                thread_ts: Some("2.0".into()),
                ..Default::default()
            };
            let mut catalog = ThreadCatalog::default();
            catalog.observe_history("C1", &[root.clone(), reply.clone()]);
            let thread_catalog = catalog.into_records();
            let repair = StoreBatch::new(
                WorkspaceRevision::INITIAL.successor(),
                vec![StoreChange::WorkspaceRepaired(WorkspaceStoreProjection {
                    conversations: vec![conversation.clone()],
                    users: vec![user],
                    histories: HashMap::from([("C1".into(), vec![root.clone()])]),
                    thread_timelines: HashMap::from([(
                        ("C1".into(), "2.0".into()),
                        vec![root, reply],
                    )]),
                    thread_catalog: thread_catalog.clone(),
                    reaction_actor_states: Vec::new(),
                })],
            )
            .unwrap();

            assert_eq!(
                store.execute_store_repair_batch(repair).await.unwrap(),
                StoreBatchExecution::Committed
            );
            assert_eq!(
                store.stored_conversations().await.unwrap().unwrap(),
                vec![conversation]
            );
            assert!(store.load_history("C_STALE").await.unwrap().is_none());
            assert!(store
                .load_thread("C_STALE", "10.0")
                .await
                .unwrap()
                .is_none());
            assert_eq!(
                store
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["2.0"]
            );
            assert_eq!(
                store
                    .load_thread("C1", "2.0")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["3.0", "2.0"]
            );
            let bootstrap = store.load_bootstrap().await.unwrap().unwrap();
            assert_eq!(
                bootstrap.user_names,
                HashMap::from([
                    ("U1".to_string(), "Newer Compatibility Name".to_string()),
                    ("U_COMPAT".to_string(), "Compatibility User".to_string(),),
                ])
            );
            assert_eq!(
                bootstrap.user_full_names.get("U1").map(String::as_str),
                Some("Ada Lovelace"),
                "repair must still add coordinator fields missing from the compatibility cache"
            );
            assert_eq!(
                bootstrap.user_avatar_urls.get("U1").map(String::as_str),
                Some("https://example.test/coordinator.png")
            );
            assert_eq!(
                bootstrap.user_search_aliases.get("U1"),
                Some(&vec![
                    "Stale Coordinator Name".to_string(),
                    "Ada Lovelace".to_string(),
                ])
            );
            assert_eq!(
                bootstrap.user_statuses.get("U1"),
                Some(&newer_status),
                "an explicit stale coordinator clear must not erase a newer compatibility status"
            );
            assert_eq!(bootstrap.thread_catalog, thread_catalog);
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
            assert!(store.stored_conversations().await.unwrap().is_none());

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
            let conversations = store.stored_conversations().await.unwrap().unwrap();
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

            let rolled_back = store.stored_conversations().await.unwrap().unwrap();
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
            let recovered = store.stored_conversations().await.unwrap().unwrap();
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
            let after_duplicate = store.stored_conversations().await.unwrap().unwrap();
            let after_duplicate = &after_duplicate[0];
            assert_eq!(after_duplicate.unread_activity_count(), 1);
            assert_eq!(after_duplicate.raw_unread_activity_count(), 5);
            assert!(after_duplicate.is_starred());
            assert_eq!(after_duplicate.name.as_deref(), Some("general"));
            assert_eq!(
                after_duplicate.extra.get("topic"),
                Some(&serde_json::json!("Keep me"))
            );

            revision = revision.successor();
            store.seed_read_cursor("C1", "20.0").await.unwrap();
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
            let after_stale = store.stored_conversations().await.unwrap().unwrap();
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
            let after_new = store.stored_conversations().await.unwrap().unwrap();
            let after_new = &after_new[0];
            assert_eq!(after_new.unread_activity_count(), 1);
            assert_eq!(after_new.raw_unread_activity_count(), 0);
            assert_eq!(after_new.last_read_ts(), Some("20.0"));
            assert_eq!(after_new.local_read_ts(), Some("20.0"));
            assert!(after_new.is_starred());
            assert_eq!(after_new.name.as_deref(), Some("general"));

            revision = revision.successor();
            store.seed_read_cursor("C1", "20.0").await.unwrap();
            let after_partial_read = store.stored_conversations().await.unwrap().unwrap();
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
                .seed_history(
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
                .seed_thread(
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
                .seed_history("C1", std::slice::from_ref(&authoritative))
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
                .seed_history("C1", std::slice::from_ref(&authoritative))
                .await
                .unwrap();
            store
                .seed_thread("C1", "1.0", std::slice::from_ref(&authoritative))
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
                .seed_history("C1", &[root_one.clone(), root_two.clone()])
                .await
                .unwrap();
            store
                .seed_thread(
                    "C1",
                    "10.0",
                    &[root_one.clone(), reply_11.clone(), reply_12],
                )
                .await
                .unwrap();
            store
                .seed_thread(
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
                .seed_history("C1", std::slice::from_ref(&root))
                .await
                .unwrap();
            store
                .seed_thread(
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
                .seed_history("C1", &[first_root.clone(), second_root.clone()])
                .await
                .unwrap();
            store
                .seed_thread("C1", "10.0", &[first_root.clone(), existing.clone()])
                .await
                .unwrap();
            store
                .seed_thread("C1", "20.0", std::slice::from_ref(&second_root))
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
                .seed_history("C1", std::slice::from_ref(&root))
                .await
                .unwrap();
            store
                .seed_thread("C1", "10.0", std::slice::from_ref(&root))
                .await
                .unwrap();
            store
                .seed_thread("C10", "10.0", std::slice::from_ref(&other_root))
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
                .seed_history("C1", std::slice::from_ref(&history_root))
                .await
                .unwrap();
            store
                .seed_thread("C1", "10.0", std::slice::from_ref(&thread_root))
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
                .seed_thread(
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

            let stored = store.stored_conversations().await.unwrap().unwrap();
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
                    reaction_actor_states: Vec::new(),
                })],
            )
            .unwrap();
            assert_eq!(
                store.execute_store_batch(bootstrap).await.unwrap(),
                StoreBatchExecution::Committed
            );

            let second_revision = first_revision.successor();
            let authoritative_reaction = ReactionMutation {
                channel_id: "C3".into(),
                message_ts: "2.100".into(),
                name: "wave".into(),
                user_id: "U4".into(),
                added: true,
            };
            let delta_reaction = ReactionMutation {
                channel_id: "C3".into(),
                message_ts: "2.100".into(),
                name: "heart".into(),
                user_id: "U5".into(),
                added: true,
            };
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
                    StoreChange::ReactionActorStatesReplaced(vec![authoritative_reaction.clone()]),
                    StoreChange::ReactionChanged(ReactionProjectionMutation {
                        change: authoritative_reaction.clone(),
                        count: ReactionProjectionCount::Authoritative(1),
                    }),
                    StoreChange::ReactionActorStatesRepaired(vec![
                        authoritative_reaction,
                        delta_reaction.clone(),
                    ]),
                    StoreChange::ReactionChanged(ReactionProjectionMutation {
                        change: delta_reaction,
                        count: ReactionProjectionCount::Delta(1),
                    }),
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

            let conversations = store.stored_conversations().await.unwrap().unwrap();
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
                store.load_history("C3").await.unwrap().unwrap()[0]
                    .reactions
                    .as_ref()
                    .map(Vec::len),
                Some(2)
            );
            assert_eq!(
                store
                    .load_bootstrap()
                    .await
                    .unwrap()
                    .unwrap()
                    .reaction_actor_states
                    .len(),
                2
            );
            assert_eq!(
                store.load_thread("C3", "2.000").await.unwrap().unwrap()[0].body_text(),
                "replacement-thread"
            );
            assert!(store.stored_thread_catalog().await.unwrap().is_empty());
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
                .stored_conversations()
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
                .seed_conversations(&conversations)
                .await
                .expect("conversation store failed");
            assert_eq!(
                store
                    .load_state()
                    .await
                    .expect("workspace state load failed")
                    .expect("missing cached workspace state")
                    .workspace_id,
                "T123:U123"
            );
            assert_eq!(
                store
                    .stored_conversations()
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
                .seed_history("C123", &messages)
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
                .seed_thread("C123", "1710000000.000100", &messages)
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
            .block_on(store.stored_conversations())
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
            .block_on(store.stored_conversations())
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
            .block_on(store.stored_conversations())
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
                .seed_conversations(&[SlackConversation {
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
            .block_on(store.stored_conversations())
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
    fn concurrent_corrupt_reads_reset_one_observed_generation_once() {
        let directory = temp_cache_dir("workspace-concurrent-corrupt-read-reset");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .seed_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();

            let first_store = store.clone();
            let second_store = store.clone();
            let (first, second) = futures_util::future::join(
                async move { first_store.stored_conversations().await },
                async move { second_store.stored_conversations().await },
            )
            .await;
            assert!(first.unwrap().is_none());
            assert!(second.unwrap().is_none());
            assert_eq!(
                store.recovery_generation(),
                1,
                "readers that observed the same generation must coalesce their reset"
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn delayed_corrupt_read_does_not_reset_a_successor_store_batch() {
        let directory = temp_cache_dir("workspace-delayed-corrupt-read-reset");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let first_revision = WorkspaceRevision::INITIAL.successor();
            assert_eq!(
                store
                    .execute_store_batch(
                        StoreBatch::new(
                            first_revision,
                            vec![StoreChange::ConversationUpsert(SlackConversation {
                                id: "C1".into(),
                                name: Some("before delayed read".into()),
                                ..Default::default()
                            })],
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap(),
                StoreBatchExecution::Committed
            );

            let (corruption_observed, observed) = tokio::sync::oneshot::channel();
            let corruption_observed = Arc::new(std::sync::Mutex::new(Some(corruption_observed)));
            let release_read = Arc::new(std::sync::Barrier::new(2));
            let query_attempt = Arc::new(AtomicUsize::new(0));
            let reader_store = store.clone();
            let reader_workspace_key = store.workspace_key.clone();
            let reader_observed = Arc::clone(&corruption_observed);
            let reader_release = Arc::clone(&release_read);
            let reader_attempt = Arc::clone(&query_attempt);
            let delayed_read = tokio::spawn(async move {
                reader_store
                    .query_or_reset((), move |connection| {
                        if reader_attempt.fetch_add(1, Ordering::SeqCst) == 0 {
                            if let Some(observed) = reader_observed
                                .lock()
                                .expect("corruption observation lock poisoned")
                                .take()
                            {
                                let _ = observed.send(());
                            }
                            reader_release.wait();
                            return Err(StoreError::invalid_derived_cache(
                                "delayed corrupt read test",
                            ));
                        }
                        let _ = load_sqlite_kind_values::<SlackConversation>(
                            connection,
                            &reader_workspace_key,
                            "conversation",
                        )?;
                        Ok(())
                    })
                    .await
            });
            observed
                .await
                .expect("corrupt reader did not reach reset admission");

            let successor_revision = first_revision.successor();
            assert_eq!(
                store
                    .execute_store_batch(
                        StoreBatch::new(
                            successor_revision,
                            vec![StoreChange::ConversationUpsert(SlackConversation {
                                id: "C2".into(),
                                name: Some("committed after corrupt read".into()),
                                ..Default::default()
                            })],
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap(),
                StoreBatchExecution::Committed
            );

            let publication_store = store.clone();
            let publication = publication_store.lock_recovery_linearization();
            tokio::pin!(publication);
            assert!(
                tokio::time::timeout(Duration::from_millis(10), publication.as_mut())
                    .await
                    .is_err(),
                "publication must wait until an in-flight recoverable read is settled"
            );
            release_read.wait();
            delayed_read.await.unwrap().unwrap();
            let publication_guard = publication.await;
            drop(publication_guard);

            let persisted_ids = store
                .stored_conversations()
                .await
                .unwrap()
                .unwrap_or_default()
                .into_iter()
                .map(|conversation| conversation.id)
                .collect::<Vec<_>>();
            let persisted_revision = *store
                .store_batch_revision
                .lock()
                .expect("store revision lock poisoned");
            assert_eq!(
                (
                    persisted_ids,
                    persisted_revision,
                    store.recovery_generation(),
                    store.workspace_cache_needs_repair(),
                ),
                (
                    vec!["C1".to_string(), "C2".to_string()],
                    successor_revision,
                    0,
                    false,
                ),
                "a corrupt read observed before the successor batch must not reset that batch"
            );

            let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
            let reopened_ids = reopened
                .stored_conversations()
                .await
                .unwrap()
                .unwrap_or_default()
                .into_iter()
                .map(|conversation| conversation.id)
                .collect::<Vec<_>>();
            assert_eq!(
                reopened_ids,
                vec!["C1".to_string(), "C2".to_string()],
                "the successor batch must remain durable across reopen"
            );
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn persistent_corruption_resets_before_successor_publication_continues() {
        let directory = temp_cache_dir("workspace-corrupt-read-before-publication");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let first_revision = WorkspaceRevision::INITIAL.successor();
            store
                .execute_store_batch(
                    StoreBatch::new(
                        first_revision,
                        vec![StoreChange::ConversationUpsert(SlackConversation {
                            id: "C1".into(),
                            name: Some("corrupt conversation".into()),
                            ..Default::default()
                        })],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();

            let (corruption_observed, observed) = tokio::sync::oneshot::channel();
            let corruption_observed = Arc::new(std::sync::Mutex::new(Some(corruption_observed)));
            let release_read = Arc::new(std::sync::Barrier::new(2));
            let query_attempt = Arc::new(AtomicUsize::new(0));
            let reader_store = store.clone();
            let reader_workspace_key = store.workspace_key.clone();
            let reader_observed = Arc::clone(&corruption_observed);
            let reader_release = Arc::clone(&release_read);
            let reader_attempt = Arc::clone(&query_attempt);
            let corrupt_read = tokio::spawn(async move {
                reader_store
                    .query_or_reset((), move |connection| {
                        let result = load_sqlite_kind_values::<SlackConversation>(
                            connection,
                            &reader_workspace_key,
                            "conversation",
                        )
                        .map(|_| ());
                        if reader_attempt.fetch_add(1, Ordering::SeqCst) == 0 {
                            if let Some(observed) = reader_observed
                                .lock()
                                .expect("corruption observation lock poisoned")
                                .take()
                            {
                                let _ = observed.send(());
                            }
                            reader_release.wait();
                        }
                        result
                    })
                    .await
            });
            observed
                .await
                .expect("corrupt reader did not finish its first read");

            let successor_revision = first_revision.successor();
            store
                .execute_store_batch(
                    StoreBatch::new(
                        successor_revision,
                        vec![StoreChange::ConversationUpsert(SlackConversation {
                            id: "C2".into(),
                            name: Some("successor conversation".into()),
                            ..Default::default()
                        })],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();

            let publication_store = store.clone();
            let publication = publication_store.lock_recovery_linearization();
            tokio::pin!(publication);
            assert!(
                tokio::time::timeout(Duration::from_millis(10), publication.as_mut())
                    .await
                    .is_err(),
                "publication must not pass a still-unresolved corrupt read"
            );
            release_read.wait();
            corrupt_read.await.unwrap().unwrap();

            let publication_guard = publication.await;
            assert_eq!(store.recovery_generation(), 1);
            assert!(
                store.workspace_cache_needs_repair(),
                "publication must observe the reset and repair before draining"
            );
            assert_eq!(
                *store
                    .store_batch_revision
                    .lock()
                    .expect("store revision lock poisoned"),
                WorkspaceRevision::INITIAL
            );
            drop(publication_guard);
            assert!(store.stored_conversations().await.unwrap().is_none());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn failed_corrupt_cache_reset_marks_workspace_for_repair() {
        let directory = temp_cache_dir("workspace-failed-corrupt-cache-reset");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let conversation = SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                ..Default::default()
            };
            let revision = WorkspaceRevision::INITIAL.successor();
            store
                .execute_store_batch(
                    StoreBatch::new(
                        revision,
                        vec![StoreChange::ConversationUpsert(conversation.clone())],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();
            store
                .install_workspace_reset_failure_trigger()
                .await
                .unwrap();

            assert!(store.validate_conversation_cache().await.is_err());
            assert_eq!(
                store.recovery_generation(),
                1,
                "confirmed corruption must advance recovery even when reset fails"
            );
            assert!(
                store.workspace_cache_needs_repair(),
                "publication must remain behind repair after a failed reset"
            );
            assert!(store.workspace_cache_needs_reset());

            store.clear_workspace_reset_failure_trigger().await.unwrap();
            let recovery_generation = store.recovery_generation();
            assert!(store
                .ensure_workspace_cache_reset_for_repair(recovery_generation)
                .await
                .unwrap());
            assert!(!store.workspace_cache_needs_reset());
            assert_eq!(
                store
                    .execute_store_repair_batch(
                        StoreBatch::new(
                            revision,
                            vec![StoreChange::WorkspaceRepaired(WorkspaceStoreProjection {
                                conversations: vec![conversation],
                                ..Default::default()
                            },)],
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap(),
                StoreBatchExecution::Committed
            );
            store.mark_workspace_cache_repaired(recovery_generation);
            store.validate_conversation_cache().await.unwrap();
            assert!(!store.workspace_cache_needs_repair());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn failed_corrupt_cache_recheck_marks_workspace_for_repair() {
        let directory = temp_cache_dir("workspace-failed-corrupt-cache-recheck");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            let attempts = Arc::new(AtomicUsize::new(0));
            let query_attempts = Arc::clone(&attempts);
            assert!(store
                .query_or_reset((), move |_| {
                    if query_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(StoreError::invalid_derived_cache(
                            "injected initial corruption",
                        ))
                    } else {
                        Err(StoreError::HubClosed)
                    }
                })
                .await
                .is_err());
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
            assert_eq!(store.recovery_generation(), 1);
            assert!(store.workspace_cache_needs_repair());
            assert!(store.workspace_cache_needs_reset());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn failed_corrupt_cache_writer_dispatch_marks_workspace_for_repair() {
        let directory = temp_cache_dir("workspace-failed-corrupt-cache-dispatch");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .seed_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();

            let hub = store.hub().await.unwrap().clone();
            let (corruption_observed, observed) = tokio::sync::oneshot::channel();
            let corruption_observed = Arc::new(std::sync::Mutex::new(Some(corruption_observed)));
            let release_read = Arc::new(std::sync::Barrier::new(2));
            let reader_store = store.clone();
            let reader_workspace_key = store.workspace_key.clone();
            let reader_observed = Arc::clone(&corruption_observed);
            let reader_release = Arc::clone(&release_read);
            let corrupt_read = tokio::spawn(async move {
                reader_store
                    .query_or_reset((), move |connection| {
                        let result = load_sqlite_kind_values::<SlackConversation>(
                            connection,
                            &reader_workspace_key,
                            "conversation",
                        )
                        .map(|_| ());
                        if let Some(observed) = reader_observed
                            .lock()
                            .expect("corruption observation lock poisoned")
                            .take()
                        {
                            let _ = observed.send(());
                            reader_release.wait();
                        }
                        result
                    })
                    .await
            });
            observed
                .await
                .expect("corrupt reader did not finish its first read");

            let admission = hub.inner.admission.lock().await;
            hub.inner.closed.store(true, Ordering::Release);
            release_read.wait();
            drop(admission);
            assert!(corrupt_read.await.unwrap().is_err());
            assert_eq!(
                store.recovery_generation(),
                1,
                "writer dispatch failure must preserve repair intent"
            );
            assert!(store.workspace_cache_needs_repair());
            assert!(store.workspace_cache_needs_reset());
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn cancelled_corrupt_read_keeps_publication_blocked_until_queued_reset_finishes() {
        let directory = temp_cache_dir("workspace-cancelled-corrupt-read-reset");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .seed_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();

            let hub = store.hub().await.unwrap().clone();
            let release_writer = Arc::new(std::sync::Barrier::new(2));
            let writer_release = Arc::clone(&release_writer);
            let (writer_started, started) = tokio::sync::oneshot::channel();
            let blocking_hub = hub.clone();
            let blocking_write = tokio::spawn(async move {
                blocking_hub
                    .write(move |_| {
                        let _ = writer_started.send(());
                        writer_release.wait();
                        Ok(())
                    })
                    .await
            });
            started.await.expect("blocking writer did not start");
            let idle_writer_capacity = hub.inner.writer.capacity();

            let (corruption_observed, observed) = tokio::sync::oneshot::channel();
            let corruption_observed = Arc::new(std::sync::Mutex::new(Some(corruption_observed)));
            let reader_store = store.clone();
            let reader_workspace_key = store.workspace_key.clone();
            let reader_observed = Arc::clone(&corruption_observed);
            let corrupt_read = tokio::spawn(async move {
                reader_store
                    .query_or_reset((), move |connection| {
                        let result = load_sqlite_kind_values::<SlackConversation>(
                            connection,
                            &reader_workspace_key,
                            "conversation",
                        )
                        .map(|_| ());
                        if let Some(observed) = reader_observed
                            .lock()
                            .expect("corruption observation lock poisoned")
                            .take()
                        {
                            let _ = observed.send(());
                        }
                        result
                    })
                    .await
            });
            observed
                .await
                .expect("corrupt reader did not finish its first read");

            let mut retry_was_queued = false;
            for _ in 0..1_000 {
                if hub.inner.writer.capacity() < idle_writer_capacity {
                    retry_was_queued = true;
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(
                retry_was_queued,
                "corrupt retry did not enter the writer queue"
            );
            corrupt_read.abort();
            assert!(corrupt_read.await.unwrap_err().is_cancelled());

            let publication_store = store.clone();
            let publication = publication_store.lock_recovery_linearization();
            tokio::pin!(publication);
            assert!(
                tokio::time::timeout(Duration::from_millis(10), publication.as_mut())
                    .await
                    .is_err(),
                "cancelling the reader must not release its queued reset guard"
            );

            release_writer.wait();
            blocking_write.await.unwrap().unwrap();
            let publication_guard = publication.await;
            assert_eq!(store.recovery_generation(), 1);
            assert!(store.workspace_cache_needs_repair());
            drop(publication_guard);
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn cancelled_corrupt_read_keeps_recovery_alive_during_initial_query() {
        let directory = temp_cache_dir("workspace-cancelled-corrupt-initial-query");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .seed_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();

            let (corruption_observed, observed) = tokio::sync::oneshot::channel();
            let corruption_observed = Arc::new(std::sync::Mutex::new(Some(corruption_observed)));
            let release_read = Arc::new(std::sync::Barrier::new(2));
            let reader_store = store.clone();
            let reader_workspace_key = store.workspace_key.clone();
            let reader_observed = Arc::clone(&corruption_observed);
            let reader_release = Arc::clone(&release_read);
            let corrupt_read = tokio::spawn(async move {
                reader_store
                    .query_or_reset((), move |connection| {
                        let result = load_sqlite_kind_values::<SlackConversation>(
                            connection,
                            &reader_workspace_key,
                            "conversation",
                        )
                        .map(|_| ());
                        if let Some(observed) = reader_observed
                            .lock()
                            .expect("corruption observation lock poisoned")
                            .take()
                        {
                            let _ = observed.send(());
                            reader_release.wait();
                        }
                        result
                    })
                    .await
            });
            observed
                .await
                .expect("corrupt reader did not observe its first result");
            corrupt_read.abort();
            assert!(corrupt_read.await.unwrap_err().is_cancelled());

            let publication_store = store.clone();
            let publication = publication_store.lock_recovery_linearization();
            tokio::pin!(publication);
            assert!(
                tokio::time::timeout(Duration::from_millis(10), publication.as_mut())
                    .await
                    .is_err(),
                "cancelling the initial query must not release the recovery guard"
            );

            release_read.wait();
            let publication_guard = publication.await;
            assert_eq!(store.recovery_generation(), 1);
            assert!(store.workspace_cache_needs_repair());
            drop(publication_guard);
        });
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn cancelled_corrupt_read_keeps_recovery_alive_before_writer_enqueue() {
        let directory = temp_cache_dir("workspace-cancelled-corrupt-read-before-enqueue");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .seed_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store.corrupt_conversation_payload("C1").await.unwrap();

            let hub = store.hub().await.unwrap().clone();
            let (corruption_observed, observed) = tokio::sync::oneshot::channel();
            let corruption_observed = Arc::new(std::sync::Mutex::new(Some(corruption_observed)));
            let release_read = Arc::new(std::sync::Barrier::new(2));
            let reader_store = store.clone();
            let reader_workspace_key = store.workspace_key.clone();
            let reader_observed = Arc::clone(&corruption_observed);
            let reader_release = Arc::clone(&release_read);
            let mut corrupt_read = tokio::spawn(async move {
                reader_store
                    .query_or_reset((), move |connection| {
                        let result = load_sqlite_kind_values::<SlackConversation>(
                            connection,
                            &reader_workspace_key,
                            "conversation",
                        )
                        .map(|_| ());
                        if let Some(observed) = reader_observed
                            .lock()
                            .expect("corruption observation lock poisoned")
                            .take()
                        {
                            let _ = observed.send(());
                            reader_release.wait();
                        }
                        result
                    })
                    .await
            });
            observed
                .await
                .expect("corrupt reader did not finish its first read");

            let admission = hub.inner.admission.lock().await;
            release_read.wait();
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut corrupt_read)
                    .await
                    .is_err(),
                "corrupt recovery must wait for writer admission"
            );
            corrupt_read.abort();
            assert!(corrupt_read.await.unwrap_err().is_cancelled());

            let publication_store = store.clone();
            let publication = publication_store.lock_recovery_linearization();
            tokio::pin!(publication);
            assert!(
                tokio::time::timeout(Duration::from_millis(10), publication.as_mut())
                    .await
                    .is_err(),
                "cancelling before enqueue must not release the recovery guard"
            );

            drop(admission);
            let publication_guard = publication.await;
            assert_eq!(store.recovery_generation(), 1);
            assert!(store.workspace_cache_needs_repair());
            drop(publication_guard);
        });
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
    fn workspace_store_barrier_is_a_noop_before_the_hub_is_initialized() {
        let directory = temp_cache_dir("workspace-store-unopened-barrier");
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");

        runtime().block_on(async {
            assert!(store.hub.get().is_none());
            store.barrier().await.unwrap();
            assert!(store.hub.get().is_none());
        });
        assert!(!directory.exists());
    }

    #[test]
    fn workspace_store_barrier_retries_an_incomplete_first_initialization() {
        let directory = temp_cache_dir("workspace-store-incomplete-barrier");
        std::fs::create_dir_all(&directory).unwrap();
        Connection::open(database_path(&directory)).unwrap();
        let store = WorkspaceStore::new(directory.clone(), "T1:U1");
        store
            .hub_initialization_started
            .store(true, Ordering::Release);

        runtime().block_on(async {
            assert!(store.hub.get().is_none());
            store.barrier().await.unwrap();
            let hub = store
                .hub
                .get()
                .expect("barrier must finish an attempted initialization");
            assert!(
                store.hub_migration.get().is_some(),
                "barrier must finish or retry the workspace migration"
            );
            let workspace_rows: i64 = hub
                .query(|connection| {
                    Ok(connection
                        .query_row("SELECT count(*) FROM workspaces", [], |row| row.get(0))?)
                })
                .await
                .unwrap();
            assert_eq!(workspace_rows, 0);
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
                .seed_conversations(&[SlackConversation {
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
                .seed_history("C123", std::slice::from_ref(&message))
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
                .seed_history(
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
                .seed_history(
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
                .seed_history(
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
                .seed_history(
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
                .seed_conversations(std::slice::from_ref(&conversation))
                .await
                .expect("conversation snapshot store failed");

            let cached = store
                .stored_conversations()
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
            .block_on(store.load_state())
            .unwrap()
            .expect("missing upgraded state");
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
                .seed_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    is_channel: Some(true),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store
                .seed_history(
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
                .seed_conversations(&[SlackConversation {
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
                .seed_history(
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
        let cached = runtime().block_on(store.load_state()).unwrap().unwrap();
        assert_eq!(cached.conversations[0].id, "C1");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn notification_claim_outcome_is_keyed_independently_of_batch_commit() {
        let directory = temp_cache_dir("workspace-store-keyed-attention-claim");
        let runtime = runtime();

        runtime.block_on(async {
            let store = WorkspaceStore::new(directory.clone(), "T123:U123");
            let duplicate_identity = AttentionDeliveryIdentity::new("D1", "1.0").unwrap();
            let initial_claim = store
                .execute_store_batch_with_claims(
                    StoreBatch::new(
                        WorkspaceRevision::INITIAL.successor(),
                        vec![StoreChange::AttentionNotificationClaim {
                            identity: duplicate_identity.clone(),
                        }],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                initial_claim.notification_claims,
                [NotificationClaimOutcome {
                    identity: duplicate_identity.clone(),
                    notification_claimed: true,
                }]
            );
            let transaction_baseline = store.committed_transaction_count().await.unwrap();
            let duplicate_batch = StoreBatch::new(
                WorkspaceRevision::INITIAL.successor().successor(),
                vec![
                    StoreChange::MessageDelta {
                        channel_id: "D1".into(),
                        message: SlackMessage {
                            ts: "2.0".into(),
                            text: Some("the delta still changes".into()),
                            ..Default::default()
                        },
                        kind: MessageMutationKind::Posted,
                    },
                    StoreChange::AttentionNotificationClaim {
                        identity: duplicate_identity.clone(),
                    },
                ],
            )
            .unwrap();
            assert!(matches!(
                duplicate_batch.workspace_repair_replay_changes().as_slice(),
                [StoreChange::AttentionNotificationClaim { identity }]
                    if identity == &duplicate_identity
            ));
            let duplicate = store
                .execute_store_batch_with_claims(duplicate_batch)
                .await
                .unwrap();
            assert_eq!(duplicate.execution, StoreBatchExecution::Committed);
            assert_eq!(
                duplicate.notification_claims,
                [NotificationClaimOutcome {
                    identity: duplicate_identity,
                    notification_claimed: false,
                }]
            );
            assert_eq!(
                store.committed_transaction_count().await.unwrap() - transaction_baseline,
                1
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
                ["2.0"]
            );

            let fresh_identity = AttentionDeliveryIdentity::new("D1", "3.0").unwrap();
            let fresh = store
                .execute_store_batch_with_claims(
                    StoreBatch::new(
                        WorkspaceRevision::INITIAL
                            .successor()
                            .successor()
                            .successor(),
                        vec![
                            StoreChange::MessageDelta {
                                channel_id: "D1".into(),
                                message: SlackMessage {
                                    ts: "3.0".into(),
                                    text: Some("fresh claim".into()),
                                    ..Default::default()
                                },
                                kind: MessageMutationKind::Posted,
                            },
                            StoreChange::AttentionNotificationClaim {
                                identity: fresh_identity.clone(),
                            },
                        ],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(fresh.execution, StoreBatchExecution::Committed);
            assert_eq!(
                fresh.notification_claims,
                [NotificationClaimOutcome {
                    identity: fresh_identity,
                    notification_claimed: true,
                }]
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn malformed_pending_unread_queue_resets_workspace_cache() {
        let directory = temp_cache_dir("workspace-malformed-pending-unread");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_pending_unread_refresh(&["C1".to_string()])
                .await
                .unwrap();
            store
                .corrupt_cached_item_payload("pending_unread", PENDING_UNREAD_QUEUE_KEY)
                .await
                .unwrap();

            assert!(store
                .load_pending_unread_refresh()
                .await
                .unwrap()
                .is_empty());
            assert_eq!(store.recovery_generation(), 1);
            assert!(store.workspace_cache_needs_repair());
            assert!(!store.workspace_cache_needs_reset());
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
            assert_eq!(
                store
                    .load_state()
                    .await
                    .unwrap()
                    .unwrap()
                    .pending_unread_refresh,
                pending
            );
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
            assert_eq!(
                store
                    .load_state()
                    .await
                    .unwrap()
                    .unwrap()
                    .pending_unread_refresh,
                expected
            );
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
                .seed_conversations(&[SlackConversation {
                    id: "C123".to_string(),
                    ..Default::default()
                }])
                .await
                .expect("conversation store failed");

            assert_eq!(
                store
                    .stored_conversations()
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
    fn coordinator_reaction_batch_persists_all_known_projections_across_reopen() {
        let directory = temp_cache_dir("coordinator-reaction-projections");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let mut broadcast = SlackMessage {
                ts: "11.0".into(),
                thread_ts: Some("10.0".into()),
                subtype: Some("thread_broadcast".into()),
                text: Some("broadcast".into()),
                ..Default::default()
            };
            broadcast.refresh_canonical_content();
            store
                .seed_history("C1", std::slice::from_ref(&broadcast))
                .await
                .unwrap();
            store
                .seed_thread("C1", "10.0", std::slice::from_ref(&broadcast))
                .await
                .unwrap();

            let change = ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "11.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            };
            let first_revision = WorkspaceRevision::INITIAL.successor();
            assert_eq!(
                store
                    .execute_store_batch(
                        StoreBatch::new(
                            first_revision,
                            vec![StoreChange::ReactionChanged(ReactionProjectionMutation {
                                change: change.clone(),
                                count: ReactionProjectionCount::Authoritative(1),
                            })],
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap(),
                StoreBatchExecution::Committed
            );
            assert_eq!(
                store
                    .execute_store_batch(
                        StoreBatch::new(
                            first_revision.successor(),
                            vec![StoreChange::ReactionChanged(ReactionProjectionMutation {
                                change,
                                count: ReactionProjectionCount::Authoritative(1),
                            })],
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap(),
                StoreBatchExecution::Unchanged
            );

            let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
            for message in [
                reopened
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .into_iter()
                    .find(|message| message.ts == "11.0")
                    .unwrap(),
                reopened
                    .load_thread("C1", "10.0")
                    .await
                    .unwrap()
                    .unwrap()
                    .into_iter()
                    .find(|message| message.ts == "11.0")
                    .unwrap(),
            ] {
                assert!(matches!(
                    message.reactions.as_deref(),
                    Some([reaction])
                        if reaction.name.as_deref() == Some("wave")
                            && reaction.count == Some(1)
                            && reaction.users.as_deref() == Some(&["U1".to_string()][..])
                ));
            }
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn coordinator_reaction_batch_persists_thread_catalog_root_across_reopen() {
        let directory = temp_cache_dir("coordinator-reaction-thread-root");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let mut root = SlackMessage {
                ts: "10.0".into(),
                reply_count: Some(1),
                text: Some("root".into()),
                ..Default::default()
            };
            root.refresh_canonical_content();
            let mut catalog = ThreadCatalog::default();
            catalog.observe_thread("C1", "10.0", std::slice::from_ref(&root), false);
            store
                .seed_thread_catalog(&catalog.into_records())
                .await
                .unwrap();

            store
                .execute_store_batch(
                    StoreBatch::new(
                        WorkspaceRevision::INITIAL.successor(),
                        vec![StoreChange::ReactionChanged(ReactionProjectionMutation {
                            change: ReactionMutation {
                                channel_id: "C1".into(),
                                message_ts: "10.0".into(),
                                name: "wave".into(),
                                user_id: "U1".into(),
                                added: true,
                            },
                            count: ReactionProjectionCount::Authoritative(1),
                        })],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();

            let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
            let records = reopened.stored_thread_catalog().await.unwrap();
            assert!(matches!(
                records[0].root.as_ref().unwrap().reactions.as_deref(),
                Some([reaction])
                    if reaction.name.as_deref() == Some("wave")
                        && reaction.count == Some(1)
                        && reaction.users.as_deref() == Some(&["U1".to_string()][..])
            ));
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn unknown_coordinator_reaction_applies_a_delta_to_loaded_store_projections() {
        let directory = temp_cache_dir("coordinator-unknown-reaction-delta");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let mut cached = SlackMessage {
                ts: "10.0".into(),
                text: Some("cached but not coordinator-loaded".into()),
                ..Default::default()
            };
            cached.refresh_canonical_content();
            store
                .seed_history("C1", std::slice::from_ref(&cached))
                .await
                .unwrap();

            let mut coordinator = WorkspaceCoordinator::default();
            let reduction = coordinator
                .apply(WorkspaceMutation::ReactionChanged(ReactionMutation {
                    channel_id: "C1".into(),
                    message_ts: "10.0".into(),
                    name: "wave".into(),
                    user_id: "U1".into(),
                    added: true,
                }))
                .expect("the unknown effective event must produce a durable reduction");
            assert!(
                reduction
                    .store_batch()
                    .unwrap()
                    .changes()
                    .iter()
                    .any(|change| matches!(change, StoreChange::ReactionChanged(_))),
                "the store batch must carry a projection delta even without a loaded projection"
            );
            store
                .execute_store_batch(reduction.store_batch().unwrap().clone())
                .await
                .unwrap();

            let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
            let history = reopened.load_history("C1").await.unwrap().unwrap();
            assert!(matches!(
                history[0].reactions.as_deref(),
                Some([reaction])
                    if reaction.name.as_deref() == Some("wave")
                        && reaction.count == Some(1)
                        && reaction.users.as_deref() == Some(&["U1".to_string()][..])
            ));
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn cold_coordinator_add_replay_does_not_double_an_explicit_cached_actor() {
        let directory = temp_cache_dir("coordinator-explicit-reaction-replay");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let mut cached = SlackMessage {
                ts: "10.0".into(),
                text: Some("cached but not coordinator-loaded".into()),
                reactions: Some(vec![crate::models::SlackReaction {
                    name: Some("wave".into()),
                    count: Some(1),
                    users: Some(vec!["U1".into()]),
                }]),
                ..Default::default()
            };
            cached.refresh_canonical_content();
            store
                .seed_history("C1", std::slice::from_ref(&cached))
                .await
                .unwrap();

            let added = ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "10.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            };
            let mut coordinator = WorkspaceCoordinator::default();
            let reduction = coordinator
                .apply(WorkspaceMutation::ReactionChanged(added.clone()))
                .expect("the cold coordinator must persist actor idempotency");
            store
                .execute_store_batch(reduction.store_batch().unwrap().clone())
                .await
                .unwrap();

            let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
            let history = reopened.load_history("C1").await.unwrap().unwrap();
            let reaction = &history[0].reactions.as_ref().unwrap()[0];
            assert_eq!(reaction.count, Some(1));
            assert_eq!(
                reaction.users.as_deref(),
                Some(&["U1".to_string()][..]),
                "the explicit cached actor must remain unique"
            );
            let bootstrap = reopened.load_bootstrap().await.unwrap().unwrap();
            assert_eq!(bootstrap.reaction_actor_states, vec![added.clone()]);
            let mut restored = WorkspaceCoordinator::default();
            restored.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                histories: HashMap::from([("C1".into(), history)]),
                reaction_actor_states: bootstrap.reaction_actor_states,
                ..Default::default()
            }));
            assert!(
                restored
                    .apply(WorkspaceMutation::ReactionChanged(added))
                    .is_none(),
                "the persisted actor fact must suppress another replay"
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn snapshot_actor_reconciliation_survives_reopen_and_deletes_retired_rows() {
        let directory = temp_cache_dir("reaction-snapshot-actor-reconciliation");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let mut reacted = SlackMessage {
                ts: "10.0".into(),
                text: Some("reacted".into()),
                reactions: Some(vec![crate::models::SlackReaction {
                    name: Some("wave".into()),
                    count: Some(1),
                    users: None,
                }]),
                ..Default::default()
            };
            reacted.refresh_canonical_content();
            store
                .seed_history("C1", std::slice::from_ref(&reacted))
                .await
                .unwrap();

            let mut coordinator = WorkspaceCoordinator::default();
            coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                histories: HashMap::from([("C1".into(), vec![reacted])]),
                ..Default::default()
            }));
            let removal = ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "10.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: false,
            };
            let removed = coordinator
                .apply(WorkspaceMutation::ReactionChanged(removal.clone()))
                .unwrap();
            store
                .execute_store_batch(removed.store_batch().unwrap().clone())
                .await
                .unwrap();

            let fresh_base = coordinator.revision();
            let mut explicit = SlackMessage {
                ts: "10.0".into(),
                text: Some("reacted".into()),
                reactions: Some(vec![crate::models::SlackReaction {
                    name: Some("wave".into()),
                    count: Some(1),
                    users: Some(vec!["U1".into()]),
                }]),
                ..Default::default()
            };
            explicit.refresh_canonical_content();
            let reconciled = coordinator
                .apply_from(
                    MutationOrigin::WebApi,
                    WorkspaceMutation::HistorySnapshot {
                        channel_id: "C1".into(),
                        snapshot: crate::workspace_pipeline::SnapshotEnvelope::new(
                            fresh_base,
                            crate::workspace_pipeline::MessagePage {
                                messages: vec![explicit],
                                complete: true,
                                ..Default::default()
                            },
                        ),
                    },
                )
                .unwrap();
            store
                .execute_store_batch(reconciled.store_batch().unwrap().clone())
                .await
                .unwrap();

            let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
            let bootstrap = reopened.load_bootstrap().await.unwrap().unwrap();
            let added = ReactionMutation {
                added: true,
                ..removal.clone()
            };
            assert_eq!(bootstrap.reaction_actor_states, vec![added.clone()]);
            let history = reopened.load_history("C1").await.unwrap().unwrap();
            let mut restored = WorkspaceCoordinator::default();
            restored.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                histories: HashMap::from([("C1".into(), history)]),
                reaction_actor_states: bootstrap.reaction_actor_states,
                ..Default::default()
            }));
            assert!(
                restored
                    .apply(WorkspaceMutation::ReactionChanged(added))
                    .is_none(),
                "the reconciled true fact must suppress an add replay after reopen"
            );

            let zero_base = restored.revision();
            let retired = restored
                .apply_from(
                    MutationOrigin::WebApi,
                    WorkspaceMutation::HistorySnapshot {
                        channel_id: "C1".into(),
                        snapshot: crate::workspace_pipeline::SnapshotEnvelope::new(
                            zero_base,
                            crate::workspace_pipeline::MessagePage {
                                messages: vec![SlackMessage {
                                    ts: "10.0".into(),
                                    text: Some("reacted".into()),
                                    ..Default::default()
                                }],
                                complete: true,
                                ..Default::default()
                            },
                        ),
                    },
                )
                .unwrap();
            reopened
                .execute_store_batch(retired.store_batch().unwrap().clone())
                .await
                .unwrap();

            let final_store = WorkspaceStore::new(directory.clone(), "T123:U123");
            let final_bootstrap = final_store.load_bootstrap().await.unwrap().unwrap();
            assert!(final_bootstrap.reaction_actor_states.is_empty());
            let final_history = final_store.load_history("C1").await.unwrap().unwrap();
            let mut after_retirement = WorkspaceCoordinator::default();
            after_retirement.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                histories: HashMap::from([("C1".into(), final_history)]),
                reaction_actor_states: final_bootstrap.reaction_actor_states,
                ..Default::default()
            }));
            assert!(
                after_retirement
                    .apply(WorkspaceMutation::ReactionChanged(removal))
                    .is_some(),
                "retiring the durable row must let one removal replay re-establish authority"
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn reaction_actor_tombstone_survives_reopen_and_suppresses_duplicate_removal() {
        let directory = temp_cache_dir("reaction-actor-tombstone");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let mut reacted = SlackMessage {
                ts: "10.0".into(),
                text: Some("reacted".into()),
                reactions: Some(vec![crate::models::SlackReaction {
                    name: Some("wave".into()),
                    count: Some(3),
                    users: Some(vec!["U_OTHER".into()]),
                }]),
                ..Default::default()
            };
            reacted.refresh_canonical_content();
            store
                .seed_history("C1", std::slice::from_ref(&reacted))
                .await
                .unwrap();

            let mut coordinator = WorkspaceCoordinator::default();
            coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                histories: HashMap::from([("C1".into(), vec![reacted])]),
                ..Default::default()
            }));
            let removal = ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "10.0".into(),
                name: "wave".into(),
                user_id: "U_OMITTED".into(),
                added: false,
            };
            let reduction = coordinator
                .apply(WorkspaceMutation::ReactionChanged(removal.clone()))
                .expect("the first removal must produce a durable reduction");
            store
                .execute_store_batch(reduction.store_batch().unwrap().clone())
                .await
                .unwrap();

            let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
            let bootstrap = reopened.load_bootstrap().await.unwrap().unwrap();
            assert_eq!(bootstrap.reaction_actor_states, vec![removal.clone()]);
            let history = reopened.load_history("C1").await.unwrap().unwrap();
            assert_eq!(history[0].reactions.as_ref().unwrap()[0].count, Some(2));

            let mut restored = WorkspaceCoordinator::default();
            restored.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                histories: HashMap::from([("C1".into(), history)]),
                reaction_actor_states: bootstrap.reaction_actor_states,
                ..Default::default()
            }));
            let revision = restored.revision();
            assert!(
                restored
                    .apply(WorkspaceMutation::ReactionChanged(removal))
                    .is_none(),
                "the persisted actor tombstone must suppress a replayed removal"
            );
            assert_eq!(restored.revision(), revision);
            assert_eq!(
                restored.history("C1")[0].reactions.as_ref().unwrap()[0].count,
                Some(2)
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn absent_reaction_authority_persists_with_later_stale_history() {
        let directory = temp_cache_dir("coordinator-reaction-late-projection");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let mut coordinator = WorkspaceCoordinator::default();
            let request_base = coordinator.revision();
            let reaction_reduction = coordinator
                .apply_from(
                    MutationOrigin::Realtime,
                    WorkspaceMutation::ReactionChanged(ReactionMutation {
                        channel_id: "C1".into(),
                        message_ts: "10.0".into(),
                        name: "wave".into(),
                        user_id: "U1".into(),
                        added: true,
                    }),
                )
                .expect("the unknown reaction must persist its actor authority");
            assert!(reaction_reduction.patch().changes().is_empty());
            store
                .execute_store_batch(reaction_reduction.store_batch().unwrap().clone())
                .await
                .unwrap();
            let reduction = coordinator
                .apply_from(
                    MutationOrigin::WebApi,
                    WorkspaceMutation::HistorySnapshot {
                        channel_id: "C1".into(),
                        snapshot: crate::workspace_pipeline::SnapshotEnvelope::new(
                            request_base,
                            crate::workspace_pipeline::MessagePage {
                                messages: vec![SlackMessage {
                                    ts: "10.0".into(),
                                    text: Some("late".into()),
                                    ..Default::default()
                                }],
                                complete: true,
                                ..Default::default()
                            },
                        ),
                    },
                )
                .expect("the stale history must materialize retained reaction authority");
            store
                .execute_store_batch(reduction.store_batch().unwrap().clone())
                .await
                .unwrap();

            let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
            let history = reopened.load_history("C1").await.unwrap().unwrap();
            assert!(matches!(
                history[0].reactions.as_deref(),
                Some([reaction])
                    if reaction.name.as_deref() == Some("wave")
                        && reaction.count == Some(1)
                        && reaction.users.as_deref() == Some(&["U1".to_string()][..])
            ));
        });

        let _ = std::fs::remove_dir_all(directory);
    }
}
