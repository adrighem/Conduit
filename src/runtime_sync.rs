/* runtime_sync.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Tokio execution boundary for the pure bounded synchronization scheduler.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::future::{AbortHandle, AbortRegistration, Abortable};
use futures_util::FutureExt;
use tokio::sync::{oneshot, watch};

use crate::sync_scheduler::{
    AdmissionOutcome, AdmissionRejection, AdmissionRejectionReason, AdmissionToken, CancellationId,
    CancellationOutcome, CompletionOutcome, DispatchedJob, JobOutcome, JobRun, SchedulerConfig,
    SchedulerCounters, ShutdownPhase, SyncJob, SyncJobId, SyncScheduler,
};

type RuntimeSyncFuture = Pin<Box<dyn Future<Output = JobOutcome> + Send + 'static>>;
type RuntimeSyncFactory = dyn Fn(u32) -> RuntimeSyncFuture + Send + Sync + 'static;

/// Retry-safe executable work stored outside the pure scheduler.
///
/// The scheduler carries only opaque identity and scheduling metadata. Runtime
/// payloads stay here and are released as soon as their admission is terminal.
#[derive(Clone)]
pub(crate) struct RuntimeSyncWork {
    factory: Arc<RuntimeSyncFactory>,
}

// Task 1 stages the scheduler boundary. Task 2 routes concrete runtime work
// through this API, so production-only dead-code allowances are temporary.
#[cfg_attr(not(test), allow(dead_code))]
impl RuntimeSyncWork {
    pub(crate) fn new<F, Fut>(factory: F) -> Self
    where
        F: Fn(u32) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = JobOutcome> + Send + 'static,
    {
        Self {
            factory: Arc::new(move |attempt| Box::pin(factory(attempt))),
        }
    }

    fn start(&self, attempt: u32) -> RuntimeSyncFuture {
        (self.factory)(attempt)
    }
}

/// Rejected scheduler admission together with the executable payload that was
/// never started.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct RuntimeSyncAdmissionError {
    rejection: AdmissionRejection,
    work: RuntimeSyncWork,
}

impl std::fmt::Debug for RuntimeSyncAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeSyncAdmissionError")
            .field("rejection", &self.rejection)
            .field("work", &"<runtime sync work>")
            .finish()
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl RuntimeSyncAdmissionError {
    pub(crate) fn reason(&self) -> crate::sync_scheduler::AdmissionRejectionReason {
        self.rejection.reason()
    }

    pub(crate) fn into_parts(self) -> (AdmissionRejection, RuntimeSyncWork) {
        (self.rejection, self.work)
    }
}

#[must_use]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum RuntimeSyncAdmissionOutcome {
    Accepted(RuntimeSyncReceipt),
    SkippedFresh(SyncJob),
}

impl std::fmt::Debug for RuntimeSyncAdmissionOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Accepted(receipt) => formatter
                .debug_tuple("Accepted")
                .field(&receipt.admission)
                .finish(),
            Self::SkippedFresh(job) => formatter.debug_tuple("SkippedFresh").field(job).finish(),
        }
    }
}

#[must_use]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct RuntimeSyncReceipt {
    admission: AdmissionToken,
    receiver: oneshot::Receiver<RuntimeSyncTerminal>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RuntimeSyncReceipt {
    pub(crate) fn admission(&self) -> AdmissionToken {
        self.admission
    }

    pub(crate) async fn wait(self) -> Result<RuntimeSyncTerminal, RuntimeSyncCompletionLost> {
        self.receiver.await.map_err(|_| RuntimeSyncCompletionLost {
            admission: self.admission,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct RuntimeSyncCompletionLost {
    admission: AdmissionToken,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RuntimeSyncCompletionLost {
    pub(crate) fn admission(self) -> AdmissionToken {
        self.admission
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeSyncTerminal {
    admission: AdmissionToken,
    attempt: u32,
    result: RuntimeSyncTerminalResult,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RuntimeSyncTerminal {
    pub(crate) fn admission(self) -> AdmissionToken {
        self.admission
    }

    pub(crate) fn attempt(self) -> u32 {
        self.attempt
    }

    pub(crate) fn result(self) -> RuntimeSyncTerminalResult {
        self.result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeSyncTerminalResult {
    Succeeded,
    Failed(RuntimeSyncFailureKind),
    Cancelled,
    Superseded { superseding: AdmissionToken },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeSyncFailureKind {
    Permanent,
    RetriesExhausted,
    Panicked,
}

struct RuntimeSyncPayload {
    admission: AdmissionToken,
    work: RuntimeSyncWork,
    terminal: oneshot::Sender<RuntimeSyncTerminal>,
}

struct RuntimeTerminalNotification {
    terminal: RuntimeSyncTerminal,
    sender: oneshot::Sender<RuntimeSyncTerminal>,
}

impl RuntimeTerminalNotification {
    fn send(self) {
        let _ = self.sender.send(self.terminal);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeAttemptExit {
    Returned(JobOutcome),
    Aborted,
    Panicked,
}

impl RuntimeAttemptExit {
    fn scheduler_outcome(self, cancellation_requested: bool) -> JobOutcome {
        let outcome = match self {
            Self::Returned(outcome) => outcome,
            Self::Aborted => JobOutcome::Cancelled,
            Self::Panicked => JobOutcome::PermanentFailure,
        };
        RuntimeSyncScheduler::authorized_worker_outcome(cancellation_requested, outcome)
    }

    fn failure_kind(self) -> RuntimeSyncFailureKind {
        match self {
            Self::Panicked => RuntimeSyncFailureKind::Panicked,
            Self::Returned(JobOutcome::RetryableFailure) => {
                RuntimeSyncFailureKind::RetriesExhausted
            }
            Self::Returned(
                JobOutcome::Succeeded | JobOutcome::PermanentFailure | JobOutcome::Cancelled,
            )
            | Self::Aborted => RuntimeSyncFailureKind::Permanent,
        }
    }
}

struct RetryWakeup {
    generation: u64,
    abort: AbortHandle,
}

struct RuntimeSyncState {
    scheduler: SyncScheduler,
    payloads: HashMap<SyncJobId, RuntimeSyncPayload>,
    running: HashMap<JobRun, AbortHandle>,
    cancellation_requested: HashSet<JobRun>,
    retry_wakeups: HashMap<SyncJobId, RetryWakeup>,
    next_job_identity: u64,
    next_retry_wakeup_generation: u64,
}

struct RuntimeSyncInner {
    started_at: Instant,
    started_at_unix_ms: u64,
    state: Mutex<RuntimeSyncState>,
    shutdown_complete: watch::Sender<bool>,
}

impl RuntimeSyncInner {
    fn state(&self) -> MutexGuard<'_, RuntimeSyncState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now_ms(&self) -> u64 {
        let elapsed_ms = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.started_at_unix_ms.saturating_add(elapsed_ms)
    }

    fn publish_drained(&self, state: &RuntimeSyncState) {
        if state.scheduler.is_drained() && !*self.shutdown_complete.borrow() {
            self.shutdown_complete.send_replace(true);
        }
    }
}

struct RuntimeDispatch {
    job_id: SyncJobId,
    run: JobRun,
    attempt: u32,
    work: RuntimeSyncWork,
    abort_registration: AbortRegistration,
}

struct RuntimeRetryWakeup {
    job_id: SyncJobId,
    generation: u64,
    ready_at_ms: u64,
    abort_registration: AbortRegistration,
}

fn shutdown_phase_code(phase: ShutdownPhase) -> &'static str {
    match phase {
        ShutdownPhase::Open => "open",
        ShutdownPhase::Draining => "draining",
        ShutdownPhase::Drained => "drained",
    }
}

fn cancellation_transition(outcome: &CancellationOutcome) -> &'static str {
    match outcome {
        CancellationOutcome::Cancelled(_) => "cancellation_cancelled",
        CancellationOutcome::Requested(_) => "cancellation_requested",
        CancellationOutcome::AlreadyRequested(_) => "cancellation_already_requested",
        CancellationOutcome::Protected { .. } => "cancellation_protected",
        CancellationOutcome::NotFound => "cancellation_not_found",
    }
}

fn completion_transition(outcome: &CompletionOutcome) -> &'static str {
    match outcome {
        CompletionOutcome::Completed => "completion_completed",
        CompletionOutcome::Retried { .. } => "completion_retried",
        CompletionOutcome::Failed(_) => "completion_failed",
        CompletionOutcome::Cancelled(_) => "completion_cancelled",
        CompletionOutcome::Superseded { .. } => "completion_superseded",
    }
}

fn trace_scheduler_snapshot(transition: &'static str, counters: SchedulerCounters) {
    tracing::trace!(
        target: "conduit::sync",
        parent: None,
        event = "sync_scheduler_snapshot",
        transition,
        admitted = counters.admitted(),
        queued_depth = counters.queued_depth(),
        running_depth = counters.running_depth(),
        coalesced = counters.coalesced(),
        cancellation_requested = counters.cancellation_requested(),
        cancellation_completed = counters.cancellation_completed(),
        completed = counters.completed(),
        failed = counters.failed(),
        retried = counters.retried(),
        skipped_fresh = counters.skipped_fresh(),
        rejected_at_capacity = counters.rejected(AdmissionRejectionReason::AtCapacity),
        rejected_duplicate_identity = counters.rejected(AdmissionRejectionReason::DuplicateIdentity),
        rejected_shutting_down = counters.rejected(AdmissionRejectionReason::ShuttingDown),
        queue_high_water = counters.queue_high_water(),
        running_high_water = counters.running_high_water(),
        shutdown_phase = shutdown_phase_code(counters.shutdown_phase()),
    );
}

/// Cloneable Tokio adapter around the deterministic synchronization scheduler.
#[derive(Clone)]
pub(crate) struct RuntimeSyncScheduler {
    inner: Arc<RuntimeSyncInner>,
}

/// Session-local identity pair for one scheduler admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct RuntimeSyncJobIdentity {
    job_id: SyncJobId,
    cancellation_id: CancellationId,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RuntimeSyncJobIdentity {
    pub(crate) fn job_id(self) -> SyncJobId {
        self.job_id
    }

    pub(crate) fn cancellation_id(self) -> CancellationId {
        self.cancellation_id
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl RuntimeSyncScheduler {
    pub(crate) fn new(config: SchedulerConfig) -> Self {
        let started_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        Self::new_at(config, started_at_unix_ms)
    }

    fn new_at(config: SchedulerConfig, started_at_unix_ms: u64) -> Self {
        let (shutdown_complete, _) = watch::channel(false);
        Self {
            inner: Arc::new(RuntimeSyncInner {
                started_at: Instant::now(),
                started_at_unix_ms,
                state: Mutex::new(RuntimeSyncState {
                    scheduler: SyncScheduler::new(config),
                    payloads: HashMap::new(),
                    running: HashMap::new(),
                    cancellation_requested: HashSet::new(),
                    retry_wakeups: HashMap::new(),
                    next_job_identity: 0,
                    next_retry_wakeup_generation: 0,
                }),
                shutdown_complete,
            }),
        }
    }

    /// Allocates one unique job and cancellation identity pair for this
    /// session-owned scheduler.
    pub(crate) fn allocate_job_identity(&self) -> RuntimeSyncJobIdentity {
        let mut state = self.inner.state();
        let value = state
            .next_job_identity
            .checked_add(1)
            .expect("runtime sync job identity space exhausted");
        state.next_job_identity = value;
        RuntimeSyncJobIdentity {
            job_id: SyncJobId::new(value),
            cancellation_id: CancellationId::new(value),
        }
    }

    /// Returns epoch-compatible milliseconds that remain monotonic for this
    /// scheduler session.
    pub(crate) fn now_epoch_ms(&self) -> u64 {
        self.inner.now_ms()
    }

    pub(crate) fn admit(
        &self,
        job: SyncJob,
        last_success_at_ms: Option<u64>,
        work: RuntimeSyncWork,
    ) -> Result<RuntimeSyncAdmissionOutcome, RuntimeSyncAdmissionError> {
        let dispatches;
        let runtime_outcome;
        let notification;
        let trace_transition;
        {
            let mut state = self.inner.state();
            let now_ms = self.inner.now_ms();
            let outcome = match state.scheduler.admit(job, now_ms, last_success_at_ms) {
                Ok(outcome) => outcome,
                Err(rejection) => {
                    trace_scheduler_snapshot("admission_rejected", state.scheduler.counters());
                    return Err(RuntimeSyncAdmissionError { rejection, work });
                }
            };

            match outcome {
                AdmissionOutcome::Accepted { token, coalesced } => {
                    trace_transition = "admission_accepted";
                    notification = coalesced.and_then(|released| {
                        Self::release_terminal_locked(
                            &mut state,
                            released.job().id(),
                            released.attempt(),
                            RuntimeSyncTerminalResult::Superseded { superseding: token },
                        )
                    });
                    let (terminal, receiver) = oneshot::channel();
                    state.payloads.insert(
                        token.job_id(),
                        RuntimeSyncPayload {
                            admission: token,
                            work,
                            terminal,
                        },
                    );
                    runtime_outcome = RuntimeSyncAdmissionOutcome::Accepted(RuntimeSyncReceipt {
                        admission: token,
                        receiver,
                    });
                }
                AdmissionOutcome::SkippedFresh(job) => {
                    trace_transition = "admission_skipped_fresh";
                    notification = None;
                    runtime_outcome = RuntimeSyncAdmissionOutcome::SkippedFresh(job);
                }
            }

            dispatches = Self::pump_locked(&mut state, now_ms);
            self.inner.publish_drained(&state);
            trace_scheduler_snapshot(trace_transition, state.scheduler.counters());
        }
        if let Some(notification) = notification {
            notification.send();
        }
        self.spawn_dispatches(dispatches);
        Ok(runtime_outcome)
    }

    pub(crate) fn cancel(&self, cancellation_id: CancellationId) -> CancellationOutcome {
        let abort;
        let dispatches;
        let notification;
        let outcome;
        {
            let mut state = self.inner.state();
            let now_ms = self.inner.now_ms();
            outcome = state.scheduler.cancel(cancellation_id, now_ms);
            abort = match &outcome {
                CancellationOutcome::Cancelled(released) => {
                    notification = Self::release_terminal_locked(
                        &mut state,
                        released.job().id(),
                        released.attempt(),
                        RuntimeSyncTerminalResult::Cancelled,
                    );
                    None
                }
                CancellationOutcome::Requested(directive)
                | CancellationOutcome::AlreadyRequested(directive) => {
                    notification = None;
                    let run = directive.run();
                    state.cancellation_requested.insert(run);
                    state.running.get(&run).cloned()
                }
                CancellationOutcome::Protected { .. } | CancellationOutcome::NotFound => {
                    notification = None;
                    None
                }
            };
            dispatches = Self::pump_locked(&mut state, now_ms);
            self.inner.publish_drained(&state);
            trace_scheduler_snapshot(
                cancellation_transition(&outcome),
                state.scheduler.counters(),
            );
        }

        if let Some(abort) = abort {
            abort.abort();
        }
        if let Some(notification) = notification {
            notification.send();
        }
        self.spawn_dispatches(dispatches);
        outcome
    }

    /// Starts shutdown and returns a completion receiver.
    ///
    /// Queued ephemeral work is released immediately. Running ephemeral work
    /// is aborted, while accepted durable work continues until the scheduler
    /// reaches its drained state.
    pub(crate) fn begin_shutdown(&self) -> watch::Receiver<bool> {
        let aborts;
        let dispatches;
        let completion;
        let notifications;
        {
            let mut state = self.inner.state();
            let now_ms = self.inner.now_ms();
            let report = state.scheduler.begin_shutdown(now_ms);

            notifications = report
                .cancelled()
                .iter()
                .filter_map(|released| {
                    Self::release_terminal_locked(
                        &mut state,
                        released.job().id(),
                        released.attempt(),
                        RuntimeSyncTerminalResult::Cancelled,
                    )
                })
                .collect::<Vec<_>>();

            aborts = report
                .cancellation_requested()
                .iter()
                .filter_map(|directive| {
                    let run = directive.run();
                    state.cancellation_requested.insert(run);
                    state.running.get(&run).cloned()
                })
                .collect::<Vec<_>>();

            dispatches = Self::pump_locked(&mut state, now_ms);
            self.inner.publish_drained(&state);
            trace_scheduler_snapshot("shutdown_started", state.scheduler.counters());
            completion = self.inner.shutdown_complete.subscribe();
        }

        for abort in aborts {
            abort.abort();
        }
        for notification in notifications {
            notification.send();
        }
        self.spawn_dispatches(dispatches);
        completion
    }

    pub(crate) async fn shutdown(&self) {
        let mut completion = self.begin_shutdown();
        while !*completion.borrow() {
            if completion.changed().await.is_err() {
                break;
            }
        }
    }

    pub(crate) fn counters(&self) -> SchedulerCounters {
        self.inner.state().scheduler.counters()
    }

    pub(crate) fn shutdown_phase(&self) -> ShutdownPhase {
        self.inner.state().scheduler.shutdown_phase()
    }

    #[cfg(test)]
    pub(crate) fn payload_count(&self) -> usize {
        self.inner.state().payloads.len()
    }

    #[cfg(test)]
    pub(crate) fn retry_wakeup_count(&self) -> usize {
        self.inner.state().retry_wakeups.len()
    }

    fn release_payload_locked(
        state: &mut RuntimeSyncState,
        job_id: SyncJobId,
    ) -> Option<RuntimeSyncPayload> {
        let payload = state.payloads.remove(&job_id);
        if let Some(wakeup) = state.retry_wakeups.remove(&job_id) {
            wakeup.abort.abort();
        }
        payload
    }

    fn release_terminal_locked(
        state: &mut RuntimeSyncState,
        job_id: SyncJobId,
        attempt: u32,
        result: RuntimeSyncTerminalResult,
    ) -> Option<RuntimeTerminalNotification> {
        Self::release_payload_locked(state, job_id).map(|payload| RuntimeTerminalNotification {
            terminal: RuntimeSyncTerminal {
                admission: payload.admission,
                attempt,
                result,
            },
            sender: payload.terminal,
        })
    }

    fn pump_locked(state: &mut RuntimeSyncState, now_ms: u64) -> Vec<RuntimeDispatch> {
        let mut dispatches = Vec::new();
        while let Some(dispatched) = state.scheduler.dispatch_next(now_ms) {
            let is_retry = dispatched.attempt() > 1;
            let job_id = dispatched.job().id();
            let run = dispatched.run();
            let work = state
                .payloads
                .get(&job_id)
                .expect("dispatched runtime sync work has an admitted payload")
                .work
                .clone();
            let (abort, abort_registration) = AbortHandle::new_pair();
            state.running.insert(run, abort);
            dispatches.push(Self::runtime_dispatch(dispatched, work, abort_registration));
            if is_retry {
                // Running capacity is reserved at this point, so this event
                // always represents an actual retry dispatch.
                trace_scheduler_snapshot("retry_dispatched", state.scheduler.counters());
            }
        }
        dispatches
    }

    fn runtime_dispatch(
        dispatched: DispatchedJob,
        work: RuntimeSyncWork,
        abort_registration: AbortRegistration,
    ) -> RuntimeDispatch {
        RuntimeDispatch {
            job_id: dispatched.job().id(),
            run: dispatched.run(),
            attempt: dispatched.attempt(),
            work,
            abort_registration,
        }
    }

    fn spawn_dispatches(&self, dispatches: Vec<RuntimeDispatch>) {
        for dispatch in dispatches {
            let scheduler = self.clone();
            tokio::spawn(async move {
                let outcome =
                    Self::run_work(dispatch.work, dispatch.attempt, dispatch.abort_registration)
                        .await;
                scheduler.complete(dispatch.job_id, dispatch.run, dispatch.attempt, outcome);
            });
        }
    }

    async fn run_work(
        work: RuntimeSyncWork,
        attempt: u32,
        abort_registration: AbortRegistration,
    ) -> RuntimeAttemptExit {
        let execution = async move {
            let future = match catch_unwind(AssertUnwindSafe(|| work.start(attempt))) {
                Ok(future) => future,
                Err(_) => return RuntimeAttemptExit::Panicked,
            };
            match AssertUnwindSafe(future).catch_unwind().await {
                Ok(outcome) => RuntimeAttemptExit::Returned(outcome),
                Err(_) => RuntimeAttemptExit::Panicked,
            }
        };

        match Abortable::new(execution, abort_registration).await {
            Ok(exit) => exit,
            Err(_) => RuntimeAttemptExit::Aborted,
        }
    }

    fn authorized_worker_outcome(cancellation_requested: bool, outcome: JobOutcome) -> JobOutcome {
        if outcome == JobOutcome::Cancelled && !cancellation_requested {
            // Cancellation belongs to the scheduler contract. A work item
            // cannot release itself as cancelled without a directive.
            JobOutcome::PermanentFailure
        } else {
            outcome
        }
    }

    fn complete(&self, job_id: SyncJobId, run: JobRun, attempt: u32, exit: RuntimeAttemptExit) {
        let dispatches;
        let retry_wakeup;
        let notification;
        {
            let mut state = self.inner.state();
            let now_ms = self.inner.now_ms();
            state.running.remove(&run);
            let cancellation_requested = state.cancellation_requested.remove(&run);
            let outcome = exit.scheduler_outcome(cancellation_requested);

            let completion = state
                .scheduler
                .complete(run, outcome, now_ms)
                .expect("runtime completion must match its dispatched scheduler run");
            let trace_transition = completion_transition(&completion);
            retry_wakeup = match completion {
                CompletionOutcome::Retried {
                    job_id,
                    ready_at_ms,
                    ..
                } if ready_at_ms > now_ms => {
                    notification = None;
                    let generation = state
                        .next_retry_wakeup_generation
                        .checked_add(1)
                        .expect("runtime retry wakeup generation space exhausted");
                    state.next_retry_wakeup_generation = generation;
                    let (abort, abort_registration) = AbortHandle::new_pair();
                    if let Some(previous) = state
                        .retry_wakeups
                        .insert(job_id, RetryWakeup { generation, abort })
                    {
                        previous.abort.abort();
                    }
                    Some(RuntimeRetryWakeup {
                        job_id,
                        generation,
                        ready_at_ms,
                        abort_registration,
                    })
                }
                CompletionOutcome::Retried { .. } => {
                    notification = None;
                    None
                }
                CompletionOutcome::Completed => {
                    notification = Self::release_terminal_locked(
                        &mut state,
                        job_id,
                        attempt,
                        RuntimeSyncTerminalResult::Succeeded,
                    );
                    None
                }
                CompletionOutcome::Failed(released) => {
                    notification = Self::release_terminal_locked(
                        &mut state,
                        job_id,
                        released.attempt(),
                        RuntimeSyncTerminalResult::Failed(exit.failure_kind()),
                    );
                    None
                }
                CompletionOutcome::Cancelled(released) => {
                    notification = Self::release_terminal_locked(
                        &mut state,
                        job_id,
                        released.attempt(),
                        RuntimeSyncTerminalResult::Cancelled,
                    );
                    None
                }
                CompletionOutcome::Superseded {
                    released,
                    superseding,
                } => {
                    notification = Self::release_terminal_locked(
                        &mut state,
                        job_id,
                        released.attempt(),
                        RuntimeSyncTerminalResult::Superseded { superseding },
                    );
                    None
                }
            };

            dispatches = Self::pump_locked(&mut state, now_ms);
            // Terminal delivery is part of the drain contract. A shutdown
            // waiter must never observe Drained before the final receipt.
            if let Some(notification) = notification {
                notification.send();
            }
            self.inner.publish_drained(&state);
            trace_scheduler_snapshot(trace_transition, state.scheduler.counters());
        }

        self.spawn_dispatches(dispatches);
        if let Some(retry_wakeup) = retry_wakeup {
            self.spawn_retry_wakeup(retry_wakeup);
        }
    }

    fn spawn_retry_wakeup(&self, retry_wakeup: RuntimeRetryWakeup) {
        let scheduler = self.clone();
        tokio::spawn(async move {
            let timer = async {
                loop {
                    let now_ms = scheduler.inner.now_ms();
                    if now_ms >= retry_wakeup.ready_at_ms {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(retry_wakeup.ready_at_ms - now_ms))
                        .await;
                }
            };
            if Abortable::new(timer, retry_wakeup.abort_registration)
                .await
                .is_ok()
            {
                scheduler.retry_wakeup_ready(retry_wakeup.job_id, retry_wakeup.generation);
            }
        });
    }

    fn retry_wakeup_ready(&self, job_id: SyncJobId, generation: u64) {
        let dispatches;
        {
            let mut state = self.inner.state();
            let is_current = state
                .retry_wakeups
                .get(&job_id)
                .is_some_and(|wakeup| wakeup.generation == generation);
            if !is_current {
                return;
            }
            state.retry_wakeups.remove(&job_id);
            let now_ms = self.inner.now_ms();
            trace_scheduler_snapshot("retry_wakeup_ready", state.scheduler.counters());
            dispatches = Self::pump_locked(&mut state, now_ms);
            self.inner.publish_drained(&state);
        }
        self.spawn_dispatches(dispatches);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::*;
    use crate::sync_scheduler::{
        AdmissionRejectionReason, FreshnessPolicy, JobOutcome, RefreshClass, ReplacementClass,
        RetryPolicy, SchedulerConfig, ShutdownPhase, SyncDurability, SyncJob, SyncJobId,
        SyncPriority, SyncTargetKey, SyncTargetKind,
    };

    fn scheduler(admission_capacity: usize, running_capacity: usize) -> RuntimeSyncScheduler {
        RuntimeSyncScheduler::new(
            SchedulerConfig::new(admission_capacity, running_capacity, 3).unwrap(),
        )
    }

    fn job(id: u64, durability: SyncDurability) -> SyncJob {
        SyncJob::new(
            SyncJobId::new(id),
            crate::sync_scheduler::CancellationId::new(id),
            SyncTargetKey::new(SyncTargetKind::Conversation, id),
            SyncPriority::Foreground,
            durability,
            FreshnessPolicy::Always,
            ReplacementClass::Never,
            RetryPolicy::Never,
        )
        .unwrap()
    }

    fn replaceable_job(id: u64, target: u64) -> SyncJob {
        SyncJob::new(
            SyncJobId::new(id),
            crate::sync_scheduler::CancellationId::new(id),
            SyncTargetKey::new(SyncTargetKind::Conversation, target),
            SyncPriority::Foreground,
            SyncDurability::Ephemeral,
            FreshnessPolicy::Always,
            ReplacementClass::Refresh(RefreshClass::ConversationHistory),
            RetryPolicy::Never,
        )
        .unwrap()
    }

    fn retrying_job_with_delay(id: u64, delay_ms: u64) -> SyncJob {
        SyncJob::new(
            SyncJobId::new(id),
            crate::sync_scheduler::CancellationId::new(id),
            SyncTargetKey::new(SyncTargetKind::Conversation, id),
            SyncPriority::Foreground,
            SyncDurability::Ephemeral,
            FreshnessPolicy::Always,
            ReplacementClass::Never,
            RetryPolicy::fixed(2, delay_ms).unwrap(),
        )
        .unwrap()
    }

    fn retrying_job(id: u64) -> SyncJob {
        retrying_job_with_delay(id, 1)
    }

    fn freshness_job(id: u64, max_age_ms: u64) -> SyncJob {
        SyncJob::new(
            SyncJobId::new(id),
            crate::sync_scheduler::CancellationId::new(id),
            SyncTargetKey::new(SyncTargetKind::Conversation, id),
            SyncPriority::Foreground,
            SyncDurability::Ephemeral,
            FreshnessPolicy::IfOlderThan { max_age_ms },
            ReplacementClass::Never,
            RetryPolicy::Never,
        )
        .unwrap()
    }

    fn counted_work(starts: Arc<AtomicUsize>, gate: Option<Arc<Notify>>) -> RuntimeSyncWork {
        RuntimeSyncWork::new(move |_| {
            let starts = Arc::clone(&starts);
            let gate = gate.clone();
            async move {
                starts.fetch_add(1, Ordering::SeqCst);
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                JobOutcome::Succeeded
            }
        })
    }

    fn accepted_receipt(outcome: RuntimeSyncAdmissionOutcome) -> RuntimeSyncReceipt {
        match outcome {
            RuntimeSyncAdmissionOutcome::Accepted(receipt) => receipt,
            RuntimeSyncAdmissionOutcome::SkippedFresh(_) => {
                panic!("test work was unexpectedly skipped as fresh")
            }
        }
    }

    async fn wait_for_count(counter: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while counter.load(Ordering::SeqCst) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scheduled work did not reach the expected count");
    }

    async fn wait_for_scheduler(
        scheduler: &RuntimeSyncScheduler,
        predicate: impl Fn(SchedulerCounters) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !predicate(scheduler.counters()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("scheduler did not reach the expected state");
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build sync scheduler test runtime")
    }

    #[derive(Clone)]
    struct TraceWriter(Arc<Mutex<Vec<u8>>>);

    static TRACE_SUBSCRIBER_TEST_LOCK: Mutex<()> = Mutex::new(());
    const TRACE_TEST_CHILD_ENV: &str = "CONDUIT_SYNC_TRACE_TEST_CHILD";

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
    fn scheduler_trace_snapshots_are_structured_and_redacted() {
        if run_trace_test_in_isolated_process(
            "runtime_sync::tests::scheduler_trace_snapshots_are_structured_and_redacted",
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
        tracing::subscriber::set_global_default(subscriber)
            .expect("isolated scheduler trace subscriber should install");

        const PRIVATE_ID_CANARY: u64 = 9_876_543_210;
        const PRIVATE_PAYLOAD_CANARY: &str = "private-runtime-payload-canary";
        test_runtime().block_on(async {
            let cancellation_scheduler = scheduler(1, 1);
            let starts = Arc::new(AtomicUsize::new(0));
            let gate = Arc::new(Notify::new());
            let receipt = accepted_receipt(
                cancellation_scheduler
                    .admit(
                        job(PRIVATE_ID_CANARY, SyncDurability::Ephemeral),
                        None,
                        counted_work(Arc::clone(&starts), Some(Arc::clone(&gate))),
                    )
                    .unwrap(),
            );
            wait_for_count(&starts, 1).await;

            let rejection = cancellation_scheduler
                .admit(
                    job(PRIVATE_ID_CANARY + 1, SyncDurability::Ephemeral),
                    None,
                    counted_work(Arc::new(AtomicUsize::new(0)), None),
                )
                .unwrap_err();
            assert_eq!(rejection.reason(), AdmissionRejectionReason::AtCapacity);
            assert!(matches!(
                cancellation_scheduler.cancel(crate::sync_scheduler::CancellationId::new(
                    PRIVATE_ID_CANARY
                )),
                CancellationOutcome::Requested(_)
            ));
            assert_eq!(
                receipt.wait().await.unwrap().result(),
                RuntimeSyncTerminalResult::Cancelled
            );

            let fresh_scheduler = scheduler(1, 1);
            let fresh_outcome = fresh_scheduler
                .admit(
                    freshness_job(PRIVATE_ID_CANARY + 2, 60_000),
                    Some(fresh_scheduler.now_epoch_ms()),
                    counted_work(Arc::new(AtomicUsize::new(0)), None),
                )
                .unwrap();
            assert!(matches!(
                fresh_outcome,
                RuntimeSyncAdmissionOutcome::SkippedFresh(_)
            ));

            let retry_scheduler = scheduler(2, 1);
            let retry_attempts = Arc::new(AtomicUsize::new(0));
            let retry_first_attempt_gate = Arc::new(Notify::new());
            let payload_canary = Arc::new(String::from(PRIVATE_PAYLOAD_CANARY));
            let retry_attempts_for_work = Arc::clone(&retry_attempts);
            let retry_first_attempt_gate_for_work = Arc::clone(&retry_first_attempt_gate);
            let retry_receipt = accepted_receipt(
                retry_scheduler
                    .admit(
                        retrying_job_with_delay(PRIVATE_ID_CANARY + 3, 50),
                        None,
                        RuntimeSyncWork::new(move |attempt| {
                            let payload_canary = Arc::clone(&payload_canary);
                            let retry_attempts = Arc::clone(&retry_attempts_for_work);
                            let retry_first_attempt_gate =
                                Arc::clone(&retry_first_attempt_gate_for_work);
                            async move {
                                assert_eq!(payload_canary.as_str(), PRIVATE_PAYLOAD_CANARY);
                                retry_attempts.fetch_add(1, Ordering::SeqCst);
                                if attempt == 1 {
                                    retry_first_attempt_gate.notified().await;
                                    JobOutcome::RetryableFailure
                                } else {
                                    JobOutcome::Succeeded
                                }
                            }
                        }),
                    )
                    .unwrap(),
            );
            wait_for_count(&retry_attempts, 1).await;

            let blocker_starts = Arc::new(AtomicUsize::new(0));
            let blocker_gate = Arc::new(Notify::new());
            let blocker_receipt = accepted_receipt(
                retry_scheduler
                    .admit(
                        job(PRIVATE_ID_CANARY + 4, SyncDurability::Ephemeral),
                        None,
                        counted_work(Arc::clone(&blocker_starts), Some(Arc::clone(&blocker_gate))),
                    )
                    .unwrap(),
            );
            retry_first_attempt_gate.notify_one();
            wait_for_count(&blocker_starts, 1).await;
            wait_for_scheduler(&retry_scheduler, |counters| counters.retried() == 1).await;
            tokio::time::timeout(Duration::from_secs(1), async {
                while retry_scheduler.retry_wakeup_count() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("retry wakeup did not fire while capacity was occupied");
            assert_eq!(retry_attempts.load(Ordering::SeqCst), 1);

            blocker_gate.notify_one();
            assert_eq!(
                blocker_receipt.wait().await.unwrap().result(),
                RuntimeSyncTerminalResult::Succeeded
            );
            assert_eq!(
                retry_receipt.wait().await.unwrap().result(),
                RuntimeSyncTerminalResult::Succeeded
            );
            retry_scheduler.shutdown().await;
        });

        let output = String::from_utf8(output.lock().expect("trace output lock poisoned").clone())
            .expect("trace output should be UTF-8");
        let snapshots = output
            .lines()
            .filter(|line| line.contains("sync_scheduler_snapshot"))
            .collect::<Vec<_>>();
        assert!(!snapshots.is_empty(), "scheduler snapshots were not traced");
        for transition in [
            "admission_accepted",
            "admission_rejected",
            "admission_skipped_fresh",
            "cancellation_requested",
            "completion_cancelled",
            "completion_retried",
            "retry_wakeup_ready",
            "retry_dispatched",
            "completion_completed",
            "shutdown_started",
        ] {
            assert!(
                snapshots
                    .iter()
                    .any(|snapshot| snapshot.contains(&format!("transition=\"{transition}\""))),
                "scheduler trace omitted {transition}: {output}"
            );
        }
        for snapshot in &snapshots {
            assert!(
                snapshot.contains("conduit::sync"),
                "scheduler trace used the wrong target: {snapshot}"
            );
            for field in [
                "admitted=",
                "queued_depth=",
                "running_depth=",
                "coalesced=",
                "cancellation_requested=",
                "cancellation_completed=",
                "completed=",
                "failed=",
                "retried=",
                "skipped_fresh=",
                "rejected_at_capacity=",
                "rejected_duplicate_identity=",
                "rejected_shutting_down=",
                "queue_high_water=",
                "running_high_water=",
                "shutdown_phase=",
            ] {
                assert!(
                    snapshot.contains(field),
                    "scheduler snapshot omitted {field}: {snapshot}"
                );
            }
            for private in [
                PRIVATE_ID_CANARY.to_string(),
                (PRIVATE_ID_CANARY + 1).to_string(),
                (PRIVATE_ID_CANARY + 2).to_string(),
                (PRIVATE_ID_CANARY + 3).to_string(),
                (PRIVATE_ID_CANARY + 4).to_string(),
                PRIVATE_PAYLOAD_CANARY.to_string(),
            ] {
                assert!(
                    !snapshot.contains(&private),
                    "scheduler snapshot leaked private data: {snapshot}"
                );
            }
        }
        let retry_wakeup = snapshots
            .iter()
            .position(|snapshot| snapshot.contains("transition=\"retry_wakeup_ready\""))
            .expect("retry wakeup transition should be traced");
        let retry_dispatch = snapshots
            .iter()
            .position(|snapshot| snapshot.contains("transition=\"retry_dispatched\""))
            .expect("retry dispatch transition should be traced");
        assert!(
            retry_wakeup < retry_dispatch,
            "retry was reported as dispatched before capacity became available: {output}"
        );
        assert!(
            snapshots[retry_wakeup].contains("queued_depth=1")
                && snapshots[retry_wakeup].contains("running_depth=1"),
            "retry wakeup did not preserve the queued retry under contention: {}",
            snapshots[retry_wakeup]
        );
        assert!(
            snapshots[retry_dispatch].contains("queued_depth=0")
                && snapshots[retry_dispatch].contains("running_depth=1"),
            "retry dispatch snapshot did not describe the actual dispatch: {}",
            snapshots[retry_dispatch]
        );
    }

    #[test]
    fn scheduled_work_is_admitted_before_spawn() {
        test_runtime().block_on(async {
            let scheduler = scheduler(1, 1);
            let first_starts = Arc::new(AtomicUsize::new(0));
            let first_gate = Arc::new(Notify::new());
            let _ = scheduler
                .admit(
                    job(1, SyncDurability::Ephemeral),
                    None,
                    counted_work(Arc::clone(&first_starts), Some(Arc::clone(&first_gate))),
                )
                .unwrap();
            wait_for_count(&first_starts, 1).await;

            let rejected_starts = Arc::new(AtomicUsize::new(0));
            let rejection = scheduler
                .admit(
                    job(2, SyncDurability::Ephemeral),
                    None,
                    counted_work(Arc::clone(&rejected_starts), None),
                )
                .unwrap_err();

            assert_eq!(rejection.reason(), AdmissionRejectionReason::AtCapacity);
            assert_eq!(rejected_starts.load(Ordering::SeqCst), 0);
            first_gate.notify_one();
            scheduler.shutdown().await;
        });
    }

    #[test]
    fn rejected_admission_returns_runtime_payload_for_retry() {
        test_runtime().block_on(async {
            let scheduler = scheduler(1, 1);
            let blocker_starts = Arc::new(AtomicUsize::new(0));
            let blocker_gate = Arc::new(Notify::new());
            let _ = scheduler
                .admit(
                    job(1, SyncDurability::Ephemeral),
                    None,
                    counted_work(Arc::clone(&blocker_starts), Some(Arc::clone(&blocker_gate))),
                )
                .unwrap();
            wait_for_count(&blocker_starts, 1).await;

            let durable_starts = Arc::new(AtomicUsize::new(0));
            let error = scheduler
                .admit(
                    job(2, SyncDurability::DurableAction),
                    None,
                    counted_work(Arc::clone(&durable_starts), None),
                )
                .unwrap_err();
            assert_eq!(error.reason(), AdmissionRejectionReason::AtCapacity);
            let (rejection, work) = error.into_parts();
            let durable_job = rejection.into_job();

            blocker_gate.notify_one();
            wait_for_scheduler(&scheduler, |counters| counters.completed() == 1).await;
            let _ = scheduler.admit(durable_job, None, work).unwrap();
            wait_for_count(&durable_starts, 1).await;
            scheduler.shutdown().await;

            assert_eq!(durable_starts.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn persisted_freshness_uses_epoch_compatible_scheduler_time() {
        test_runtime().block_on(async {
            let scheduler =
                RuntimeSyncScheduler::new_at(SchedulerConfig::new(1, 1, 3).unwrap(), 1_000_000);
            let starts = Arc::new(AtomicUsize::new(0));
            let outcome = scheduler
                .admit(
                    freshness_job(1, 1_000),
                    Some(100),
                    counted_work(Arc::clone(&starts), None),
                )
                .unwrap();

            assert!(matches!(outcome, RuntimeSyncAdmissionOutcome::Accepted(_)));
            wait_for_count(&starts, 1).await;
            scheduler.shutdown().await;
        });
    }

    #[test]
    fn job_identities_are_session_local_monotonic_and_shared_by_clones() {
        let runtime_scheduler = scheduler(1, 1);
        let scheduler_clone = runtime_scheduler.clone();

        let first = runtime_scheduler.allocate_job_identity();
        let second = scheduler_clone.allocate_job_identity();

        assert_eq!(first.job_id(), SyncJobId::new(1));
        assert_eq!(
            first.cancellation_id(),
            crate::sync_scheduler::CancellationId::new(1)
        );
        assert_eq!(second.job_id(), SyncJobId::new(2));
        assert_eq!(
            second.cancellation_id(),
            crate::sync_scheduler::CancellationId::new(2)
        );
        assert_ne!(first, second);

        let next_session_scheduler = scheduler(1, 1);
        let next_session_first = next_session_scheduler.allocate_job_identity();
        assert_eq!(next_session_first.job_id(), SyncJobId::new(1));
        assert_eq!(
            next_session_first.cancellation_id(),
            crate::sync_scheduler::CancellationId::new(1)
        );
    }

    #[test]
    #[should_panic(expected = "runtime sync job identity space exhausted")]
    fn job_identity_allocation_panics_instead_of_wrapping() {
        let scheduler = scheduler(1, 1);
        scheduler.inner.state().next_job_identity = u64::MAX;

        let _ = scheduler.allocate_job_identity();
    }

    #[test]
    fn exposed_scheduler_clock_is_epoch_compatible_and_monotonic() {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        let scheduler = scheduler(1, 1);
        let first = scheduler.now_epoch_ms();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        let second = scheduler.now_epoch_ms();

        assert!(first >= before);
        assert!(first <= after);
        assert!(second >= first);
    }

    #[test]
    fn requested_cancellation_preserves_a_successful_worker_race() {
        assert_eq!(
            RuntimeSyncScheduler::authorized_worker_outcome(true, JobOutcome::Succeeded),
            JobOutcome::Succeeded
        );
        assert_eq!(
            RuntimeSyncScheduler::authorized_worker_outcome(true, JobOutcome::Cancelled),
            JobOutcome::Cancelled
        );
        assert_eq!(
            RuntimeSyncScheduler::authorized_worker_outcome(false, JobOutcome::Cancelled),
            JobOutcome::PermanentFailure
        );
    }

    #[test]
    fn completion_pumps_already_admitted_work() {
        test_runtime().block_on(async {
            let scheduler = scheduler(2, 1);
            let first_starts = Arc::new(AtomicUsize::new(0));
            let first_gate = Arc::new(Notify::new());
            let second_starts = Arc::new(AtomicUsize::new(0));

            let _ = scheduler
                .admit(
                    job(1, SyncDurability::Ephemeral),
                    None,
                    counted_work(Arc::clone(&first_starts), Some(Arc::clone(&first_gate))),
                )
                .unwrap();
            let _ = scheduler
                .admit(
                    job(2, SyncDurability::Ephemeral),
                    None,
                    counted_work(Arc::clone(&second_starts), None),
                )
                .unwrap();

            wait_for_count(&first_starts, 1).await;
            assert_eq!(second_starts.load(Ordering::SeqCst), 0);
            first_gate.notify_one();
            wait_for_count(&second_starts, 1).await;
            scheduler.shutdown().await;
            assert_eq!(scheduler.counters().completed(), 2);
        });
    }

    #[test]
    fn queued_replacement_releases_the_old_runtime_payload() {
        test_runtime().block_on(async {
            let scheduler = scheduler(3, 1);
            let blocker_starts = Arc::new(AtomicUsize::new(0));
            let blocker_gate = Arc::new(Notify::new());
            let old_starts = Arc::new(AtomicUsize::new(0));
            let new_starts = Arc::new(AtomicUsize::new(0));

            let _ = scheduler
                .admit(
                    job(1, SyncDurability::Ephemeral),
                    None,
                    counted_work(Arc::clone(&blocker_starts), Some(Arc::clone(&blocker_gate))),
                )
                .unwrap();
            let _ = scheduler
                .admit(
                    replaceable_job(2, 42),
                    None,
                    counted_work(Arc::clone(&old_starts), None),
                )
                .unwrap();
            let _ = scheduler
                .admit(
                    replaceable_job(3, 42),
                    None,
                    counted_work(Arc::clone(&new_starts), None),
                )
                .unwrap();

            wait_for_count(&blocker_starts, 1).await;
            blocker_gate.notify_one();
            wait_for_count(&new_starts, 1).await;
            scheduler.shutdown().await;

            assert_eq!(old_starts.load(Ordering::SeqCst), 0);
            assert_eq!(scheduler.counters().coalesced(), 1);
            assert_eq!(scheduler.payload_count(), 0);
        });
    }

    #[test]
    fn shutdown_cancels_ephemeral_work_and_drains_durable_work() {
        test_runtime().block_on(async {
            let scheduler = scheduler(4, 1);
            let running_ephemeral_starts = Arc::new(AtomicUsize::new(0));
            let durable_starts = Arc::new(AtomicUsize::new(0));
            let durable_gate = Arc::new(Notify::new());
            let read_marker_starts = Arc::new(AtomicUsize::new(0));
            let queued_ephemeral_starts = Arc::new(AtomicUsize::new(0));

            let running_ephemeral = accepted_receipt(
                scheduler
                    .admit(
                        job(1, SyncDurability::Ephemeral),
                        None,
                        RuntimeSyncWork::new({
                            let starts = Arc::clone(&running_ephemeral_starts);
                            move |_| {
                                let starts = Arc::clone(&starts);
                                async move {
                                    starts.fetch_add(1, Ordering::SeqCst);
                                    std::future::pending::<JobOutcome>().await
                                }
                            }
                        }),
                    )
                    .unwrap(),
            );
            let durable = accepted_receipt(
                scheduler
                    .admit(
                        job(2, SyncDurability::DurableAction),
                        None,
                        counted_work(Arc::clone(&durable_starts), Some(Arc::clone(&durable_gate))),
                    )
                    .unwrap(),
            );
            let read_marker = accepted_receipt(
                scheduler
                    .admit(
                        job(3, SyncDurability::ReadMarker),
                        None,
                        counted_work(Arc::clone(&read_marker_starts), None),
                    )
                    .unwrap(),
            );
            let queued_ephemeral = accepted_receipt(
                scheduler
                    .admit(
                        job(4, SyncDurability::Ephemeral),
                        None,
                        counted_work(Arc::clone(&queued_ephemeral_starts), None),
                    )
                    .unwrap(),
            );
            wait_for_count(&running_ephemeral_starts, 1).await;

            let shutdown_scheduler = scheduler.clone();
            let shutdown = tokio::spawn(async move {
                shutdown_scheduler.shutdown().await;
            });
            wait_for_count(&durable_starts, 1).await;
            assert!(!shutdown.is_finished());
            durable_gate.notify_one();
            wait_for_count(&read_marker_starts, 1).await;
            tokio::time::timeout(Duration::from_secs(1), shutdown)
                .await
                .expect("scheduler shutdown did not drain")
                .expect("scheduler shutdown task failed");

            assert_eq!(
                running_ephemeral.wait().await.unwrap().result(),
                RuntimeSyncTerminalResult::Cancelled
            );
            assert_eq!(
                queued_ephemeral.wait().await.unwrap().result(),
                RuntimeSyncTerminalResult::Cancelled
            );
            assert_eq!(
                durable.wait().await.unwrap().result(),
                RuntimeSyncTerminalResult::Succeeded
            );
            assert_eq!(
                read_marker.wait().await.unwrap().result(),
                RuntimeSyncTerminalResult::Succeeded
            );
            assert_eq!(queued_ephemeral_starts.load(Ordering::SeqCst), 0);
            assert_eq!(scheduler.counters().completed(), 2);
            assert_eq!(scheduler.counters().cancellation_completed(), 2);
            assert_eq!(
                scheduler.counters().shutdown_phase(),
                ShutdownPhase::Drained
            );
            assert_eq!(scheduler.payload_count(), 0);
        });
    }

    #[test]
    fn explicit_running_cancellation_is_acknowledged_before_pumping() {
        test_runtime().block_on(async {
            let scheduler = scheduler(2, 1);
            let first_starts = Arc::new(AtomicUsize::new(0));
            let second_starts = Arc::new(AtomicUsize::new(0));
            let _ = scheduler
                .admit(
                    job(1, SyncDurability::Ephemeral),
                    None,
                    RuntimeSyncWork::new({
                        let starts = Arc::clone(&first_starts);
                        move |_| {
                            let starts = Arc::clone(&starts);
                            async move {
                                starts.fetch_add(1, Ordering::SeqCst);
                                std::future::pending::<JobOutcome>().await
                            }
                        }
                    }),
                )
                .unwrap();
            let _ = scheduler
                .admit(
                    job(2, SyncDurability::Ephemeral),
                    None,
                    counted_work(Arc::clone(&second_starts), None),
                )
                .unwrap();
            wait_for_count(&first_starts, 1).await;

            assert!(matches!(
                scheduler.cancel(crate::sync_scheduler::CancellationId::new(1)),
                crate::sync_scheduler::CancellationOutcome::Requested(_)
            ));
            wait_for_count(&second_starts, 1).await;
            scheduler.shutdown().await;

            assert_eq!(scheduler.counters().cancellation_completed(), 1);
            assert_eq!(scheduler.counters().completed(), 1);
        });
    }

    #[test]
    fn retry_wakeup_reuses_the_bounded_payload_and_attempt_number() {
        test_runtime().block_on(async {
            let scheduler = scheduler(1, 1);
            let attempts = Arc::new(AtomicUsize::new(0));
            let _ = scheduler
                .admit(
                    retrying_job(1),
                    None,
                    RuntimeSyncWork::new({
                        let attempts = Arc::clone(&attempts);
                        move |attempt| {
                            let attempts = Arc::clone(&attempts);
                            async move {
                                attempts.fetch_add(1, Ordering::SeqCst);
                                if attempt == 1 {
                                    JobOutcome::RetryableFailure
                                } else {
                                    JobOutcome::Succeeded
                                }
                            }
                        }
                    }),
                )
                .unwrap();

            wait_for_count(&attempts, 2).await;
            scheduler.shutdown().await;

            assert_eq!(scheduler.counters().retried(), 1);
            assert_eq!(scheduler.counters().completed(), 1);
            assert_eq!(scheduler.payload_count(), 0);
        });
    }

    #[test]
    fn cancelling_a_delayed_retry_releases_its_owned_wakeup() {
        test_runtime().block_on(async {
            let scheduler = scheduler(1, 1);
            let _ = scheduler
                .admit(
                    retrying_job_with_delay(1, 60_000),
                    None,
                    RuntimeSyncWork::new(|_| async move { JobOutcome::RetryableFailure }),
                )
                .unwrap();
            wait_for_scheduler(&scheduler, |counters| counters.retried() == 1).await;
            assert_eq!(scheduler.retry_wakeup_count(), 1);

            assert!(matches!(
                scheduler.cancel(crate::sync_scheduler::CancellationId::new(1)),
                CancellationOutcome::Cancelled(_)
            ));
            assert_eq!(scheduler.retry_wakeup_count(), 0);
            scheduler.shutdown().await;
        });
    }

    #[test]
    fn terminal_receipts_distinguish_permanent_failure_and_panic() {
        test_runtime().block_on(async {
            let scheduler = scheduler(2, 1);
            let permanent = accepted_receipt(
                scheduler
                    .admit(
                        job(1, SyncDurability::Ephemeral),
                        None,
                        RuntimeSyncWork::new(|_| async move { JobOutcome::PermanentFailure }),
                    )
                    .unwrap(),
            );
            let panicked = accepted_receipt(
                scheduler
                    .admit(
                        job(2, SyncDurability::Ephemeral),
                        None,
                        RuntimeSyncWork::new(|_| async move {
                            panic!("synthetic receipt panic");
                        }),
                    )
                    .unwrap(),
            );

            let permanent_token = permanent.admission();
            let panicked_token = panicked.admission();
            let permanent = permanent.wait().await.unwrap();
            let panicked = panicked.wait().await.unwrap();

            assert_eq!(permanent.admission(), permanent_token);
            assert_eq!(permanent.attempt(), 1);
            assert_eq!(
                permanent.result(),
                RuntimeSyncTerminalResult::Failed(RuntimeSyncFailureKind::Permanent)
            );
            assert_eq!(panicked.admission(), panicked_token);
            assert_eq!(panicked.attempt(), 1);
            assert_eq!(
                panicked.result(),
                RuntimeSyncTerminalResult::Failed(RuntimeSyncFailureKind::Panicked)
            );
            scheduler.shutdown().await;
        });
    }

    #[test]
    fn terminal_receipt_stays_pending_across_a_retry() {
        test_runtime().block_on(async {
            let scheduler = scheduler(1, 1);
            let second_started = Arc::new(Notify::new());
            let finish_second = Arc::new(Notify::new());
            let receipt = accepted_receipt(
                scheduler
                    .admit(
                        retrying_job(1),
                        None,
                        RuntimeSyncWork::new({
                            let second_started = Arc::clone(&second_started);
                            let finish_second = Arc::clone(&finish_second);
                            move |attempt| {
                                let second_started = Arc::clone(&second_started);
                                let finish_second = Arc::clone(&finish_second);
                                async move {
                                    if attempt == 1 {
                                        JobOutcome::RetryableFailure
                                    } else {
                                        second_started.notify_one();
                                        finish_second.notified().await;
                                        JobOutcome::Succeeded
                                    }
                                }
                            }
                        }),
                    )
                    .unwrap(),
            );
            let completion = Box::pin(receipt.wait());
            let second_attempt = Box::pin(second_started.notified());
            let completion = match futures_util::future::select(completion, second_attempt).await {
                futures_util::future::Either::Left((terminal, _)) => {
                    panic!("receipt completed before the retry: {terminal:?}");
                }
                futures_util::future::Either::Right(((), completion)) => completion,
            };
            finish_second.notify_one();
            let terminal = completion.await.unwrap();

            assert_eq!(terminal.attempt(), 2);
            assert_eq!(terminal.result(), RuntimeSyncTerminalResult::Succeeded);
            scheduler.shutdown().await;
        });
    }

    #[test]
    fn replacement_and_queued_cancellation_resolve_their_receipts() {
        test_runtime().block_on(async {
            let scheduler = scheduler(3, 1);
            let blocker_starts = Arc::new(AtomicUsize::new(0));
            let blocker_gate = Arc::new(Notify::new());
            let _blocker = accepted_receipt(
                scheduler
                    .admit(
                        job(1, SyncDurability::Ephemeral),
                        None,
                        counted_work(Arc::clone(&blocker_starts), Some(Arc::clone(&blocker_gate))),
                    )
                    .unwrap(),
            );
            wait_for_count(&blocker_starts, 1).await;

            let old = accepted_receipt(
                scheduler
                    .admit(
                        replaceable_job(2, 42),
                        None,
                        counted_work(Arc::new(AtomicUsize::new(0)), None),
                    )
                    .unwrap(),
            );
            let new = accepted_receipt(
                scheduler
                    .admit(
                        replaceable_job(3, 42),
                        None,
                        counted_work(Arc::new(AtomicUsize::new(0)), None),
                    )
                    .unwrap(),
            );
            let old_token = old.admission();
            let new_token = new.admission();
            let old_terminal = old.wait().await.unwrap();

            assert_eq!(old_terminal.admission(), old_token);
            assert_eq!(
                old_terminal.result(),
                RuntimeSyncTerminalResult::Superseded {
                    superseding: new_token
                }
            );
            assert!(matches!(
                scheduler.cancel(crate::sync_scheduler::CancellationId::new(3)),
                CancellationOutcome::Cancelled(_)
            ));
            let new_terminal = new.wait().await.unwrap();
            assert_eq!(new_terminal.admission(), new_token);
            assert_eq!(new_terminal.result(), RuntimeSyncTerminalResult::Cancelled);

            blocker_gate.notify_one();
            scheduler.shutdown().await;
        });
    }

    #[test]
    fn reused_job_identity_gets_a_distinct_terminal_receipt() {
        test_runtime().block_on(async {
            let scheduler = scheduler(1, 1);
            let first = accepted_receipt(
                scheduler
                    .admit(
                        job(1, SyncDurability::Ephemeral),
                        None,
                        counted_work(Arc::new(AtomicUsize::new(0)), None),
                    )
                    .unwrap(),
            );
            let first_token = first.admission();
            let first_terminal = first.wait().await.unwrap();

            let second = accepted_receipt(
                scheduler
                    .admit(
                        job(1, SyncDurability::Ephemeral),
                        None,
                        counted_work(Arc::new(AtomicUsize::new(0)), None),
                    )
                    .unwrap(),
            );
            let second_token = second.admission();
            let second_terminal = second.wait().await.unwrap();

            assert_ne!(first_token.generation(), second_token.generation());
            assert_eq!(first_terminal.admission(), first_token);
            assert_eq!(second_terminal.admission(), second_token);
            scheduler.shutdown().await;
        });
    }

    #[test]
    fn lost_terminal_sender_preserves_the_receipt_identity() {
        test_runtime().block_on(async {
            let scheduler = scheduler(2, 1);
            let blocker_gate = Arc::new(Notify::new());
            let _blocker = accepted_receipt(
                scheduler
                    .admit(
                        job(1, SyncDurability::Ephemeral),
                        None,
                        counted_work(
                            Arc::new(AtomicUsize::new(0)),
                            Some(Arc::clone(&blocker_gate)),
                        ),
                    )
                    .unwrap(),
            );
            let receipt = accepted_receipt(
                scheduler
                    .admit(
                        job(2, SyncDurability::Ephemeral),
                        None,
                        counted_work(Arc::new(AtomicUsize::new(0)), None),
                    )
                    .unwrap(),
            );
            let admission = receipt.admission();

            let payload = scheduler
                .inner
                .state()
                .payloads
                .remove(&SyncJobId::new(2))
                .expect("queued test payload exists");
            drop(payload);
            let error = receipt.wait().await.unwrap_err();

            assert_eq!(error.admission(), admission);
            assert!(matches!(
                scheduler.cancel(crate::sync_scheduler::CancellationId::new(2)),
                CancellationOutcome::Cancelled(_)
            ));
            blocker_gate.notify_one();
            scheduler.shutdown().await;
        });
    }

    #[test]
    fn panicking_work_releases_capacity_and_pumps_the_next_job() {
        test_runtime().block_on(async {
            let scheduler = scheduler(2, 1);
            let second_starts = Arc::new(AtomicUsize::new(0));
            let _ = scheduler
                .admit(
                    job(1, SyncDurability::Ephemeral),
                    None,
                    RuntimeSyncWork::new(|_| async move {
                        panic!("synthetic scheduled work panic");
                    }),
                )
                .unwrap();
            let _ = scheduler
                .admit(
                    job(2, SyncDurability::Ephemeral),
                    None,
                    counted_work(Arc::clone(&second_starts), None),
                )
                .unwrap();

            wait_for_count(&second_starts, 1).await;
            scheduler.shutdown().await;
            assert_eq!(scheduler.counters().failed(), 1);
            assert_eq!(scheduler.counters().completed(), 1);
        });
    }
}
