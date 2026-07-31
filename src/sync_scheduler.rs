/* sync_scheduler.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Pure contracts and deterministic scheduling for bounded synchronization work.

// Runtime integration follows after the issue #11 authority slice.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SyncJobId(u64);

impl SyncJobId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CancellationId(u64);

impl CancellationId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SyncTargetKind {
    Workspace,
    Conversation,
    Thread,
    UserDirectory,
    User,
    Presence,
    SearchIndex,
    Asset,
}

/// An opaque, typed scheduler key. The numeric identity must be derived from
/// non-secret stable metadata by the caller and cannot carry message content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SyncTargetKey {
    kind: SyncTargetKind,
    opaque_id: u64,
}

impl SyncTargetKey {
    pub(crate) const fn new(kind: SyncTargetKind, opaque_id: u64) -> Self {
        Self { kind, opaque_id }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SyncPriority {
    Interactive,
    Foreground,
    Maintenance,
}

impl SyncPriority {
    fn can_supersede(self, existing: Self) -> bool {
        matches!(
            (self, existing),
            (Self::Interactive, _)
                | (Self::Foreground, Self::Foreground | Self::Maintenance)
                | (Self::Maintenance, Self::Maintenance)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SyncDurability {
    Ephemeral,
    DurableAction,
    ReadMarker,
}

impl SyncDurability {
    fn is_durable(self) -> bool {
        matches!(self, Self::DurableAction | Self::ReadMarker)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FreshnessPolicy {
    Always,
    IfOlderThan { max_age_ms: u64 },
}

impl FreshnessPolicy {
    pub(crate) fn decision(
        self,
        now_ms: u64,
        last_success_at_ms: Option<u64>,
    ) -> FreshnessDecision {
        match (self, last_success_at_ms) {
            (Self::Always, _) | (Self::IfOlderThan { .. }, None) => FreshnessDecision::Run,
            (Self::IfOlderThan { max_age_ms: 0 }, Some(_)) => FreshnessDecision::Run,
            (Self::IfOlderThan { max_age_ms }, Some(last_success_at_ms))
                if now_ms.saturating_sub(last_success_at_ms) < max_age_ms =>
            {
                FreshnessDecision::SkipFresh
            }
            (Self::IfOlderThan { .. }, Some(_)) => FreshnessDecision::Run,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FreshnessDecision {
    Run,
    SkipFresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RefreshClass {
    Workspace,
    Membership,
    ConversationHistory,
    ThreadReplies,
    UserDirectory,
    Presence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ReplacementClass {
    Never,
    Refresh(RefreshClass),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RetryPolicy {
    Never,
    Fixed { max_attempts: u32, delay_ms: u64 },
}

impl RetryPolicy {
    pub(crate) fn fixed(max_attempts: u32, delay_ms: u64) -> Result<Self, RetryPolicyError> {
        if max_attempts == 0 {
            return Err(RetryPolicyError::ZeroAttempts);
        }
        Ok(Self::Fixed {
            max_attempts,
            delay_ms,
        })
    }

    pub(crate) fn decision(self, failed_attempt: u32, now_ms: u64) -> RetryDecision {
        match self {
            Self::Fixed {
                max_attempts,
                delay_ms,
            } if failed_attempt < max_attempts => RetryDecision::RetryAt {
                ready_at_ms: now_ms.saturating_add(delay_ms),
            },
            Self::Never | Self::Fixed { .. } => RetryDecision::Exhausted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryPolicyError {
    ZeroAttempts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    RetryAt { ready_at_ms: u64 },
    Exhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobContractError {
    DurableWorkCannotBeReplaceable,
    DurableWorkMustAlwaysRun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncJob {
    id: SyncJobId,
    cancellation_id: CancellationId,
    target: SyncTargetKey,
    priority: SyncPriority,
    durability: SyncDurability,
    freshness: FreshnessPolicy,
    replacement: ReplacementClass,
    retry: RetryPolicy,
}

impl SyncJob {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: SyncJobId,
        cancellation_id: CancellationId,
        target: SyncTargetKey,
        priority: SyncPriority,
        durability: SyncDurability,
        freshness: FreshnessPolicy,
        replacement: ReplacementClass,
        retry: RetryPolicy,
    ) -> Result<Self, JobContractError> {
        if durability.is_durable() && replacement != ReplacementClass::Never {
            return Err(JobContractError::DurableWorkCannotBeReplaceable);
        }
        if durability.is_durable() && freshness != FreshnessPolicy::Always {
            return Err(JobContractError::DurableWorkMustAlwaysRun);
        }
        Ok(Self {
            id,
            cancellation_id,
            target,
            priority,
            durability,
            freshness,
            replacement,
            retry,
        })
    }

    pub(crate) fn id(&self) -> SyncJobId {
        self.id
    }

    pub(crate) fn cancellation_id(&self) -> CancellationId {
        self.cancellation_id
    }

    pub(crate) fn target(&self) -> SyncTargetKey {
        self.target
    }

    pub(crate) fn priority(&self) -> SyncPriority {
        self.priority
    }

    pub(crate) fn durability(&self) -> SyncDurability {
        self.durability
    }

    pub(crate) fn freshness(&self) -> FreshnessPolicy {
        self.freshness
    }

    pub(crate) fn replacement(&self) -> ReplacementClass {
        self.replacement
    }

    pub(crate) fn retry(&self) -> RetryPolicy {
        self.retry
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SchedulerConfig {
    admission_capacity: usize,
    running_capacity: usize,
    /// Maximum dispatches from strictly higher-priority lanes while an
    /// eligible lower-priority lane waits.
    max_higher_priority_dispatches: u64,
}

impl SchedulerConfig {
    pub(crate) fn new(
        admission_capacity: usize,
        running_capacity: usize,
        max_higher_priority_dispatches: u64,
    ) -> Result<Self, SchedulerConfigError> {
        if admission_capacity == 0 {
            return Err(SchedulerConfigError::ZeroAdmissionCapacity);
        }
        if running_capacity == 0 {
            return Err(SchedulerConfigError::ZeroRunningCapacity);
        }
        if running_capacity > admission_capacity {
            return Err(SchedulerConfigError::RunningCapacityExceedsAdmission);
        }
        if max_higher_priority_dispatches == 0 {
            return Err(SchedulerConfigError::ZeroStarvationBound);
        }
        Ok(Self {
            admission_capacity,
            running_capacity,
            max_higher_priority_dispatches,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedulerConfigError {
    ZeroAdmissionCapacity,
    ZeroRunningCapacity,
    RunningCapacityExceedsAdmission,
    ZeroStarvationBound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AdmissionToken {
    job_id: SyncJobId,
    generation: u64,
}

impl AdmissionToken {
    fn new(job_id: SyncJobId, generation: u64) -> Self {
        Self { job_id, generation }
    }

    pub(crate) fn job_id(self) -> SyncJobId {
        self.job_id
    }

    pub(crate) fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdmissionOutcome {
    Accepted {
        token: AdmissionToken,
        coalesced: Option<ReleasedJob>,
    },
    SkippedFresh(SyncJob),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionRejectionReason {
    AtCapacity,
    DuplicateIdentity,
    ShuttingDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdmissionRejection {
    reason: AdmissionRejectionReason,
    job: SyncJob,
}

impl AdmissionRejection {
    pub(crate) fn reason(&self) -> AdmissionRejectionReason {
        self.reason
    }

    pub(crate) fn into_job(self) -> SyncJob {
        self.job
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleasedJob {
    job: SyncJob,
    attempt: u32,
}

impl ReleasedJob {
    pub(crate) fn new(job: SyncJob, attempt: u32) -> Self {
        Self { job, attempt }
    }

    pub(crate) fn job(&self) -> &SyncJob {
        &self.job
    }

    pub(crate) fn attempt(&self) -> u32 {
        self.attempt
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CancellationDirective {
    run: JobRun,
    cancellation_id: CancellationId,
}

impl CancellationDirective {
    fn from_running(running: &RunningJob) -> Self {
        Self {
            run: JobRun {
                job_id: running.job.id,
                attempt: running.attempt,
                generation: running.generation,
            },
            cancellation_id: running.job.cancellation_id,
        }
    }

    pub(crate) fn run(&self) -> JobRun {
        self.run
    }

    pub(crate) fn cancellation_id(&self) -> CancellationId {
        self.cancellation_id
    }
}

#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CancellationOutcome {
    Cancelled(ReleasedJob),
    Requested(CancellationDirective),
    AlreadyRequested(CancellationDirective),
    Protected { job_id: SyncJobId },
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct JobRun {
    job_id: SyncJobId,
    attempt: u32,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchedJob {
    job: SyncJob,
    admission: AdmissionToken,
    attempt: u32,
    generation: u64,
}

impl DispatchedJob {
    pub(crate) fn job(&self) -> &SyncJob {
        &self.job
    }

    pub(crate) fn attempt(&self) -> u32 {
        self.attempt
    }

    pub(crate) fn admission(&self) -> AdmissionToken {
        self.admission
    }

    pub(crate) fn run(&self) -> JobRun {
        JobRun {
            job_id: self.job.id,
            attempt: self.attempt,
            generation: self.generation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobOutcome {
    Succeeded,
    RetryableFailure,
    PermanentFailure,
    Cancelled,
}

#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompletionOutcome {
    Completed,
    Retried {
        job_id: SyncJobId,
        attempt: u32,
        ready_at_ms: u64,
    },
    Failed(ReleasedJob),
    Cancelled(ReleasedJob),
    Superseded {
        released: ReleasedJob,
        superseding: AdmissionToken,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionError {
    UnknownJob,
    StaleRun,
    StaleAttempt,
    DurableCancellationForbidden,
    CancellationNotRequested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownPhase {
    Open,
    Draining,
    Drained,
}

/// Aggregate scheduler observability without job, target, or cancellation
/// identities. Cumulative counters saturate instead of affecting scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SchedulerCounters {
    admitted: u64,
    queued_depth: usize,
    running_depth: usize,
    coalesced: u64,
    cancellation_requested: u64,
    cancellation_completed: u64,
    completed: u64,
    failed: u64,
    retried: u64,
    skipped_fresh: u64,
    rejected_at_capacity: u64,
    rejected_duplicate_identity: u64,
    rejected_shutting_down: u64,
    queue_high_water: usize,
    running_high_water: usize,
    shutdown_phase: ShutdownPhase,
}

impl SchedulerCounters {
    pub(crate) fn admitted(self) -> u64 {
        self.admitted
    }

    pub(crate) fn queued_depth(self) -> usize {
        self.queued_depth
    }

    pub(crate) fn running_depth(self) -> usize {
        self.running_depth
    }

    pub(crate) fn coalesced(self) -> u64 {
        self.coalesced
    }

    /// First-time cancellation directives sent to already running work.
    ///
    /// This and `cancellation_completed` describe different populations and
    /// must not be subtracted to derive pending cancellation work.
    pub(crate) fn cancellation_requested(self) -> u64 {
        self.cancellation_requested
    }

    /// Terminally cancelled admissions, including synchronous queued
    /// cancellation where no running directive was necessary.
    pub(crate) fn cancellation_completed(self) -> u64 {
        self.cancellation_completed
    }

    pub(crate) fn completed(self) -> u64 {
        self.completed
    }

    pub(crate) fn failed(self) -> u64 {
        self.failed
    }

    pub(crate) fn retried(self) -> u64 {
        self.retried
    }

    pub(crate) fn skipped_fresh(self) -> u64 {
        self.skipped_fresh
    }

    pub(crate) fn rejected(self, reason: AdmissionRejectionReason) -> u64 {
        match reason {
            AdmissionRejectionReason::AtCapacity => self.rejected_at_capacity,
            AdmissionRejectionReason::DuplicateIdentity => self.rejected_duplicate_identity,
            AdmissionRejectionReason::ShuttingDown => self.rejected_shutting_down,
        }
    }

    pub(crate) fn queue_high_water(self) -> usize {
        self.queue_high_water
    }

    pub(crate) fn running_high_water(self) -> usize {
        self.running_high_water
    }

    pub(crate) fn shutdown_phase(self) -> ShutdownPhase {
        self.shutdown_phase
    }
}

#[derive(Debug, Default)]
struct SchedulerCounterState {
    admitted: u64,
    coalesced: u64,
    cancellation_requested: u64,
    cancellation_completed: u64,
    completed: u64,
    failed: u64,
    retried: u64,
    skipped_fresh: u64,
    rejected_at_capacity: u64,
    rejected_duplicate_identity: u64,
    rejected_shutting_down: u64,
    queue_high_water: usize,
    running_high_water: usize,
}

#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShutdownReport {
    phase: ShutdownPhase,
    cancelled: Vec<ReleasedJob>,
    cancellation_requested: Vec<CancellationDirective>,
}

impl ShutdownReport {
    pub(crate) fn phase(&self) -> ShutdownPhase {
        self.phase
    }

    pub(crate) fn cancelled(&self) -> &[ReleasedJob] {
        &self.cancelled
    }

    pub(crate) fn cancellation_requested(&self) -> &[CancellationDirective] {
        &self.cancellation_requested
    }
}

#[derive(Debug, Clone)]
struct QueuedJob {
    job: SyncJob,
    admission: AdmissionToken,
    attempt: u32,
    ready_at_ms: u64,
}

#[derive(Debug, Clone)]
struct RunningJob {
    job: SyncJob,
    admission: AdmissionToken,
    attempt: u32,
    generation: u64,
    cancellation_requested: bool,
    retry_superseded_by: Option<AdmissionToken>,
}

/// A pure state machine. A caller may spawn work only after `dispatch_next`
/// returns it; at that point both admission and running capacity are reserved.
pub(crate) struct SyncScheduler {
    config: SchedulerConfig,
    shutdown_phase: ShutdownPhase,
    interactive: VecDeque<QueuedJob>,
    foreground: VecDeque<QueuedJob>,
    maintenance: VecDeque<QueuedJob>,
    running: HashMap<SyncJobId, RunningJob>,
    counters: SchedulerCounterState,
    next_admission_generation: u64,
    next_run_generation: u64,
    foreground_wait_dispatches: u64,
    maintenance_wait_dispatches: u64,
}

impl SyncScheduler {
    pub(crate) fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            shutdown_phase: ShutdownPhase::Open,
            interactive: VecDeque::new(),
            foreground: VecDeque::new(),
            maintenance: VecDeque::new(),
            running: HashMap::new(),
            counters: SchedulerCounterState::default(),
            next_admission_generation: 1,
            next_run_generation: 1,
            foreground_wait_dispatches: 0,
            maintenance_wait_dispatches: 0,
        }
    }

    pub(crate) fn admit(
        &mut self,
        job: SyncJob,
        now_ms: u64,
        last_success_at_ms: Option<u64>,
    ) -> Result<AdmissionOutcome, AdmissionRejection> {
        if self.shutdown_phase != ShutdownPhase::Open {
            return self.reject_admission(job, AdmissionRejectionReason::ShuttingDown);
        }
        if self.has_active_identity(job.id, job.cancellation_id) {
            return self.reject_admission(job, AdmissionRejectionReason::DuplicateIdentity);
        }
        if job.freshness.decision(now_ms, last_success_at_ms) == FreshnessDecision::SkipFresh {
            self.counters.skipped_fresh = self.counters.skipped_fresh.saturating_add(1);
            return Ok(AdmissionOutcome::SkippedFresh(job));
        }
        let replacement = self.queued_replacement_for(&job);
        if replacement.is_none() && self.active_len() >= self.config.admission_capacity {
            return self.reject_admission(job, AdmissionRejectionReason::AtCapacity);
        }

        let token = AdmissionToken::new(job.id, self.next_admission_generation);
        self.next_admission_generation = self
            .next_admission_generation
            .checked_add(1)
            .expect("scheduler admission generation space exhausted");
        if matches!(job.replacement, ReplacementClass::Refresh(_)) {
            for running in self.running.values_mut() {
                if running.job.target == job.target
                    && running.job.replacement == job.replacement
                    && job.priority.can_supersede(running.job.priority)
                {
                    running.retry_superseded_by = Some(token);
                }
            }
        }
        let coalesced = replacement.map(|(priority, position)| {
            let replaced = self
                .queue_mut(priority)
                .remove(position)
                .expect("replacement queue position exists");
            self.reset_wait_if_ineligible(priority, now_ms);
            ReleasedJob::new(replaced.job, replaced.attempt)
        });
        self.queue_mut(job.priority).push_back(QueuedJob {
            job,
            admission: token,
            attempt: 1,
            ready_at_ms: now_ms,
        });
        self.counters.admitted = self.counters.admitted.saturating_add(1);
        self.counters.coalesced = self
            .counters
            .coalesced
            .saturating_add(u64::from(coalesced.is_some()));
        self.record_depth_high_water();
        Ok(AdmissionOutcome::Accepted { token, coalesced })
    }

    pub(crate) fn dispatch_next(&mut self, now_ms: u64) -> Option<DispatchedJob> {
        if self.running.len() >= self.config.running_capacity {
            return None;
        }

        let interactive = self.dispatchable_position(SyncPriority::Interactive, now_ms);
        let foreground = self.dispatchable_position(SyncPriority::Foreground, now_ms);
        let maintenance = self.dispatchable_position(SyncPriority::Maintenance, now_ms);
        let priority = if maintenance.is_some()
            && self.maintenance_wait_dispatches >= self.config.max_higher_priority_dispatches
        {
            SyncPriority::Maintenance
        } else if foreground.is_some()
            && self.foreground_wait_dispatches >= self.config.max_higher_priority_dispatches
        {
            SyncPriority::Foreground
        } else if interactive.is_some() {
            SyncPriority::Interactive
        } else if foreground.is_some() {
            SyncPriority::Foreground
        } else if maintenance.is_some() {
            SyncPriority::Maintenance
        } else {
            return None;
        };

        if maintenance.is_some() {
            if priority == SyncPriority::Maintenance {
                self.maintenance_wait_dispatches = 0;
            } else {
                self.maintenance_wait_dispatches =
                    self.maintenance_wait_dispatches.saturating_add(1);
            }
        } else {
            self.maintenance_wait_dispatches = 0;
        }
        if foreground.is_some() {
            if priority == SyncPriority::Foreground {
                self.foreground_wait_dispatches = 0;
            } else if priority == SyncPriority::Interactive {
                self.foreground_wait_dispatches = self.foreground_wait_dispatches.saturating_add(1);
            }
        } else {
            self.foreground_wait_dispatches = 0;
        }

        let position = match priority {
            SyncPriority::Interactive => interactive,
            SyncPriority::Foreground => foreground,
            SyncPriority::Maintenance => maintenance,
        }
        .expect("selected priority has eligible work");
        let queued = self
            .queue_mut(priority)
            .remove(position)
            .expect("eligible queue position exists");
        let generation = self.next_run_generation;
        self.next_run_generation = self
            .next_run_generation
            .checked_add(1)
            .expect("scheduler run generation space exhausted");
        let dispatched = DispatchedJob {
            job: queued.job.clone(),
            admission: queued.admission,
            attempt: queued.attempt,
            generation,
        };
        self.running.insert(
            queued.job.id,
            RunningJob {
                job: queued.job,
                admission: queued.admission,
                attempt: queued.attempt,
                generation,
                cancellation_requested: false,
                retry_superseded_by: None,
            },
        );
        self.record_depth_high_water();
        Some(dispatched)
    }

    pub(crate) fn complete(
        &mut self,
        run: JobRun,
        outcome: JobOutcome,
        now_ms: u64,
    ) -> Result<CompletionOutcome, CompletionError> {
        let Some(running) = self.running.get(&run.job_id) else {
            return Err(CompletionError::UnknownJob);
        };
        if running.generation != run.generation {
            return Err(CompletionError::StaleRun);
        }
        if running.attempt != run.attempt {
            return Err(CompletionError::StaleAttempt);
        }
        if running.job.durability.is_durable() && outcome == JobOutcome::Cancelled {
            return Err(CompletionError::DurableCancellationForbidden);
        }
        if outcome == JobOutcome::Cancelled && !running.cancellation_requested {
            return Err(CompletionError::CancellationNotRequested);
        }

        let running = self
            .running
            .remove(&run.job_id)
            .expect("validated running job exists");
        let completion = match outcome {
            JobOutcome::Succeeded => CompletionOutcome::Completed,
            JobOutcome::Cancelled => {
                CompletionOutcome::Cancelled(ReleasedJob::new(running.job, running.attempt))
            }
            JobOutcome::PermanentFailure => {
                if running.cancellation_requested {
                    CompletionOutcome::Cancelled(ReleasedJob::new(running.job, running.attempt))
                } else {
                    CompletionOutcome::Failed(ReleasedJob::new(running.job, running.attempt))
                }
            }
            JobOutcome::RetryableFailure if running.cancellation_requested => {
                CompletionOutcome::Cancelled(ReleasedJob::new(running.job, running.attempt))
            }
            JobOutcome::RetryableFailure
                if self.shutdown_phase != ShutdownPhase::Open
                    && !running.job.durability.is_durable() =>
            {
                CompletionOutcome::Cancelled(ReleasedJob::new(running.job, running.attempt))
            }
            JobOutcome::RetryableFailure => match running.job.retry.decision(run.attempt, now_ms) {
                RetryDecision::RetryAt { ready_at_ms } => {
                    if let Some(superseding) = running
                        .retry_superseded_by
                        .or_else(|| self.queued_superseding_refresh_for(&running.job))
                    {
                        CompletionOutcome::Superseded {
                            released: ReleasedJob::new(running.job, running.attempt),
                            superseding,
                        }
                    } else {
                        let attempt = run
                            .attempt
                            .checked_add(1)
                            .expect("scheduler retry attempt space exhausted");
                        let job_id = running.job.id;
                        self.queue_mut(running.job.priority).push_back(QueuedJob {
                            job: running.job,
                            admission: running.admission,
                            attempt,
                            ready_at_ms,
                        });
                        CompletionOutcome::Retried {
                            job_id,
                            attempt,
                            ready_at_ms,
                        }
                    }
                }
                RetryDecision::Exhausted => {
                    CompletionOutcome::Failed(ReleasedJob::new(running.job, running.attempt))
                }
            },
        };
        self.record_completion_outcome(&completion);
        self.update_drained();
        Ok(completion)
    }

    pub(crate) fn cancel(
        &mut self,
        cancellation_id: CancellationId,
        now_ms: u64,
    ) -> CancellationOutcome {
        for priority in [
            SyncPriority::Interactive,
            SyncPriority::Foreground,
            SyncPriority::Maintenance,
        ] {
            let Some(position) = self
                .queue(priority)
                .iter()
                .position(|queued| queued.job.cancellation_id == cancellation_id)
            else {
                continue;
            };
            let queued = &self.queue(priority)[position];
            if queued.job.durability.is_durable() {
                return CancellationOutcome::Protected {
                    job_id: queued.job.id,
                };
            }
            let queued = self
                .queue_mut(priority)
                .remove(position)
                .expect("located cancellation queue position exists");
            self.reset_wait_if_ineligible(priority, now_ms);
            let cancelled = ReleasedJob::new(queued.job, queued.attempt);
            self.counters.cancellation_completed =
                self.counters.cancellation_completed.saturating_add(1);
            self.update_drained();
            return CancellationOutcome::Cancelled(cancelled);
        }

        let Some(job_id) = self.running.iter().find_map(|(job_id, running)| {
            (running.job.cancellation_id == cancellation_id).then_some(*job_id)
        }) else {
            return CancellationOutcome::NotFound;
        };
        let running = self
            .running
            .get_mut(&job_id)
            .expect("located running cancellation exists");
        if running.job.durability.is_durable() {
            return CancellationOutcome::Protected {
                job_id: running.job.id,
            };
        }
        let directive = CancellationDirective::from_running(running);
        if running.cancellation_requested {
            CancellationOutcome::AlreadyRequested(directive)
        } else {
            running.cancellation_requested = true;
            self.counters.cancellation_requested =
                self.counters.cancellation_requested.saturating_add(1);
            CancellationOutcome::Requested(directive)
        }
    }

    pub(crate) fn begin_shutdown(&mut self, now_ms: u64) -> ShutdownReport {
        if self.shutdown_phase != ShutdownPhase::Open {
            return ShutdownReport {
                phase: self.shutdown_phase,
                cancelled: Vec::new(),
                cancellation_requested: Vec::new(),
            };
        }

        self.shutdown_phase = ShutdownPhase::Draining;
        let mut cancelled = Vec::new();
        Self::cancel_ephemeral_queue(&mut self.interactive, &mut cancelled);
        Self::cancel_ephemeral_queue(&mut self.foreground, &mut cancelled);
        Self::cancel_ephemeral_queue(&mut self.maintenance, &mut cancelled);
        self.reset_wait_if_ineligible(SyncPriority::Foreground, now_ms);
        self.reset_wait_if_ineligible(SyncPriority::Maintenance, now_ms);
        let mut cancellation_requested = Vec::new();
        for running in self.running.values_mut() {
            if !running.job.durability.is_durable() && !running.cancellation_requested {
                cancellation_requested.push(CancellationDirective::from_running(running));
                running.cancellation_requested = true;
            }
        }
        cancelled.sort_unstable_by_key(|released| released.job.id);
        cancellation_requested.sort_unstable_by_key(|directive| directive.run.job_id);
        self.counters.cancellation_completed = self
            .counters
            .cancellation_completed
            .saturating_add(u64::try_from(cancelled.len()).unwrap_or(u64::MAX));
        self.counters.cancellation_requested = self
            .counters
            .cancellation_requested
            .saturating_add(u64::try_from(cancellation_requested.len()).unwrap_or(u64::MAX));
        self.update_drained();
        ShutdownReport {
            phase: self.shutdown_phase,
            cancelled,
            cancellation_requested,
        }
    }

    pub(crate) fn shutdown_phase(&self) -> ShutdownPhase {
        self.shutdown_phase
    }

    pub(crate) fn is_drained(&self) -> bool {
        self.shutdown_phase == ShutdownPhase::Drained
    }

    pub(crate) fn counters(&self) -> SchedulerCounters {
        SchedulerCounters {
            admitted: self.counters.admitted,
            queued_depth: self.queued_len(),
            running_depth: self.running.len(),
            coalesced: self.counters.coalesced,
            cancellation_requested: self.counters.cancellation_requested,
            cancellation_completed: self.counters.cancellation_completed,
            completed: self.counters.completed,
            failed: self.counters.failed,
            retried: self.counters.retried,
            skipped_fresh: self.counters.skipped_fresh,
            rejected_at_capacity: self.counters.rejected_at_capacity,
            rejected_duplicate_identity: self.counters.rejected_duplicate_identity,
            rejected_shutting_down: self.counters.rejected_shutting_down,
            queue_high_water: self.counters.queue_high_water,
            running_high_water: self.counters.running_high_water,
            shutdown_phase: self.shutdown_phase,
        }
    }

    fn reject_admission(
        &mut self,
        job: SyncJob,
        reason: AdmissionRejectionReason,
    ) -> Result<AdmissionOutcome, AdmissionRejection> {
        let rejected = match reason {
            AdmissionRejectionReason::AtCapacity => &mut self.counters.rejected_at_capacity,
            AdmissionRejectionReason::DuplicateIdentity => {
                &mut self.counters.rejected_duplicate_identity
            }
            AdmissionRejectionReason::ShuttingDown => &mut self.counters.rejected_shutting_down,
        };
        *rejected = rejected.saturating_add(1);
        Err(AdmissionRejection { reason, job })
    }

    fn record_completion_outcome(&mut self, outcome: &CompletionOutcome) {
        let counter = match outcome {
            CompletionOutcome::Completed => &mut self.counters.completed,
            CompletionOutcome::Retried { .. } => &mut self.counters.retried,
            CompletionOutcome::Failed(_) => &mut self.counters.failed,
            CompletionOutcome::Cancelled(_) => &mut self.counters.cancellation_completed,
            CompletionOutcome::Superseded { .. } => &mut self.counters.coalesced,
        };
        *counter = counter.saturating_add(1);
        if matches!(outcome, CompletionOutcome::Retried { .. }) {
            self.record_depth_high_water();
        }
    }

    fn record_depth_high_water(&mut self) {
        let queued = self.queued_len();
        let running = self.running.len();
        debug_assert!(
            queued + running <= self.config.admission_capacity,
            "scheduler admission capacity exceeded"
        );
        debug_assert!(
            running <= self.config.running_capacity,
            "scheduler running capacity exceeded"
        );
        self.counters.queue_high_water = self.counters.queue_high_water.max(queued);
        self.counters.running_high_water = self.counters.running_high_water.max(running);
    }

    fn queued_len(&self) -> usize {
        self.interactive.len() + self.foreground.len() + self.maintenance.len()
    }

    fn active_len(&self) -> usize {
        self.queued_len() + self.running.len()
    }

    fn has_active_identity(&self, job_id: SyncJobId, cancellation_id: CancellationId) -> bool {
        self.running.values().any(|running| {
            running.job.id == job_id || running.job.cancellation_id == cancellation_id
        }) || self
            .interactive
            .iter()
            .chain(&self.foreground)
            .chain(&self.maintenance)
            .any(|queued| queued.job.id == job_id || queued.job.cancellation_id == cancellation_id)
    }

    fn queued_replacement_for(&self, job: &SyncJob) -> Option<(SyncPriority, usize)> {
        if !matches!(job.replacement, ReplacementClass::Refresh(_)) {
            return None;
        }
        [
            SyncPriority::Interactive,
            SyncPriority::Foreground,
            SyncPriority::Maintenance,
        ]
        .into_iter()
        .find_map(|priority| {
            self.queue(priority)
                .iter()
                .position(|queued| {
                    queued.job.target == job.target
                        && queued.job.replacement == job.replacement
                        && !queued.job.durability.is_durable()
                        && job.priority.can_supersede(queued.job.priority)
                })
                .map(|position| (priority, position))
        })
    }

    fn queued_superseding_refresh_for(&self, job: &SyncJob) -> Option<AdmissionToken> {
        if !matches!(job.replacement, ReplacementClass::Refresh(_)) {
            return None;
        }
        [
            SyncPriority::Interactive,
            SyncPriority::Foreground,
            SyncPriority::Maintenance,
        ]
        .into_iter()
        .find_map(|priority| {
            self.queue(priority)
                .iter()
                .find(|queued| {
                    queued.job.target == job.target
                        && queued.job.replacement == job.replacement
                        && !queued.job.durability.is_durable()
                        && queued.job.priority.can_supersede(job.priority)
                })
                .map(|queued| queued.admission)
        })
    }

    fn cancel_ephemeral_queue(queue: &mut VecDeque<QueuedJob>, cancelled: &mut Vec<ReleasedJob>) {
        let mut retained = VecDeque::with_capacity(queue.len());
        while let Some(queued) = queue.pop_front() {
            if queued.job.durability.is_durable() {
                retained.push_back(queued);
            } else {
                cancelled.push(ReleasedJob::new(queued.job, queued.attempt));
            }
        }
        *queue = retained;
    }

    fn update_drained(&mut self) {
        if self.shutdown_phase == ShutdownPhase::Draining && self.active_len() == 0 {
            self.shutdown_phase = ShutdownPhase::Drained;
        }
    }

    fn reset_wait_if_ineligible(&mut self, priority: SyncPriority, now_ms: u64) {
        if self.dispatchable_position(priority, now_ms).is_some() {
            return;
        }
        match priority {
            SyncPriority::Interactive => {}
            SyncPriority::Foreground => self.foreground_wait_dispatches = 0,
            SyncPriority::Maintenance => self.maintenance_wait_dispatches = 0,
        }
    }

    fn dispatchable_position(&self, priority: SyncPriority, now_ms: u64) -> Option<usize> {
        self.queue(priority).iter().position(|queued| {
            queued.ready_at_ms <= now_ms
                && (!matches!(queued.job.replacement, ReplacementClass::Refresh(_))
                    || !self
                        .running
                        .values()
                        .any(|running| running.job.target == queued.job.target))
        })
    }

    fn queue_mut(&mut self, priority: SyncPriority) -> &mut VecDeque<QueuedJob> {
        match priority {
            SyncPriority::Interactive => &mut self.interactive,
            SyncPriority::Foreground => &mut self.foreground,
            SyncPriority::Maintenance => &mut self.maintenance,
        }
    }

    fn queue(&self, priority: SyncPriority) -> &VecDeque<QueuedJob> {
        match priority {
            SyncPriority::Interactive => &self.interactive,
            SyncPriority::Foreground => &self.foreground,
            SyncPriority::Maintenance => &self.maintenance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation_target(id: u64) -> SyncTargetKey {
        SyncTargetKey::new(SyncTargetKind::Conversation, id)
    }

    fn ephemeral_job(id: u64) -> SyncJob {
        SyncJob::new(
            SyncJobId::new(id),
            CancellationId::new(id + 1_000),
            conversation_target(id),
            SyncPriority::Foreground,
            SyncDurability::Ephemeral,
            FreshnessPolicy::Always,
            ReplacementClass::Never,
            RetryPolicy::Never,
        )
        .unwrap()
    }

    #[test]
    fn sync_job_exposes_only_opaque_scheduling_identity_and_contracts() {
        let job = ephemeral_job(7);

        assert_eq!(job.id(), SyncJobId::new(7));
        assert_eq!(job.cancellation_id(), CancellationId::new(1_007));
        assert_eq!(job.target(), conversation_target(7));
        assert_eq!(job.priority(), SyncPriority::Foreground);
        assert_eq!(job.durability(), SyncDurability::Ephemeral);
        assert_eq!(job.freshness(), FreshnessPolicy::Always);
        assert_eq!(job.replacement(), ReplacementClass::Never);
        assert_eq!(job.retry(), RetryPolicy::Never);
    }

    #[test]
    fn durable_actions_and_read_markers_cannot_be_replaceable_or_skippable() {
        for durability in [SyncDurability::DurableAction, SyncDurability::ReadMarker] {
            let replaceable = SyncJob::new(
                SyncJobId::new(1),
                CancellationId::new(2),
                conversation_target(3),
                SyncPriority::Interactive,
                durability,
                FreshnessPolicy::Always,
                ReplacementClass::Refresh(RefreshClass::ConversationHistory),
                RetryPolicy::Never,
            );
            assert_eq!(
                replaceable.unwrap_err(),
                JobContractError::DurableWorkCannotBeReplaceable
            );

            let skippable = SyncJob::new(
                SyncJobId::new(4),
                CancellationId::new(5),
                conversation_target(6),
                SyncPriority::Interactive,
                durability,
                FreshnessPolicy::IfOlderThan { max_age_ms: 500 },
                ReplacementClass::Never,
                RetryPolicy::Never,
            );
            assert_eq!(
                skippable.unwrap_err(),
                JobContractError::DurableWorkMustAlwaysRun
            );
        }
    }

    #[test]
    fn freshness_decision_is_deterministic_and_handles_clock_rollback() {
        let policy = FreshnessPolicy::IfOlderThan { max_age_ms: 1_000 };

        assert_eq!(
            policy.decision(5_999, Some(5_000)),
            FreshnessDecision::SkipFresh
        );
        assert_eq!(policy.decision(6_000, Some(5_000)), FreshnessDecision::Run);
        assert_eq!(
            policy.decision(5_999, Some(5_500)),
            FreshnessDecision::SkipFresh
        );
        assert_eq!(
            policy.decision(4_000, Some(5_000)),
            FreshnessDecision::SkipFresh
        );
        assert_eq!(policy.decision(5_999, None), FreshnessDecision::Run);
        assert_eq!(
            FreshnessPolicy::Always.decision(5_999, Some(5_999)),
            FreshnessDecision::Run
        );
    }

    #[test]
    fn fixed_retry_policy_is_bounded_and_saturating() {
        let policy = RetryPolicy::fixed(3, 250).unwrap();

        assert_eq!(
            policy.decision(1, 1_000),
            RetryDecision::RetryAt { ready_at_ms: 1_250 }
        );
        assert_eq!(
            policy.decision(2, u64::MAX - 100),
            RetryDecision::RetryAt {
                ready_at_ms: u64::MAX
            }
        );
        assert_eq!(policy.decision(3, 1_000), RetryDecision::Exhausted);
        assert_eq!(
            RetryPolicy::Never.decision(1, 1_000),
            RetryDecision::Exhausted
        );
        assert_eq!(
            RetryPolicy::fixed(0, 250).unwrap_err(),
            RetryPolicyError::ZeroAttempts
        );
    }

    fn scheduler(
        admission_capacity: usize,
        running_capacity: usize,
        starvation_dispatches: u64,
    ) -> SyncScheduler {
        SyncScheduler::new(
            SchedulerConfig::new(admission_capacity, running_capacity, starvation_dispatches)
                .unwrap(),
        )
    }

    fn with_priority(mut job: SyncJob, priority: SyncPriority) -> SyncJob {
        job.priority = priority;
        job
    }

    fn admit(scheduler: &mut SyncScheduler, job: SyncJob) -> AdmissionOutcome {
        scheduler.admit(job, 10_000, None).unwrap()
    }

    fn dispatch_and_complete(scheduler: &mut SyncScheduler, now_ms: u64) -> SyncJobId {
        let dispatched = scheduler.dispatch_next(now_ms).unwrap();
        let id = dispatched.job().id();
        assert_eq!(
            scheduler.complete(dispatched.run(), JobOutcome::Succeeded, now_ms),
            Ok(CompletionOutcome::Completed)
        );
        id
    }

    #[test]
    fn config_requires_nonzero_nested_capacity_and_starvation_bound() {
        assert_eq!(
            SchedulerConfig::new(0, 1, 2).unwrap_err(),
            SchedulerConfigError::ZeroAdmissionCapacity
        );
        assert_eq!(
            SchedulerConfig::new(2, 0, 2).unwrap_err(),
            SchedulerConfigError::ZeroRunningCapacity
        );
        assert_eq!(
            SchedulerConfig::new(2, 3, 2).unwrap_err(),
            SchedulerConfigError::RunningCapacityExceedsAdmission
        );
        assert_eq!(
            SchedulerConfig::new(2, 1, 0).unwrap_err(),
            SchedulerConfigError::ZeroStarvationBound
        );
    }

    #[test]
    fn admission_reserves_total_capacity_before_dispatch_or_spawn() {
        let mut scheduler = scheduler(2, 1, 3);
        let first = ephemeral_job(1);
        let second = ephemeral_job(2);
        let rejected = ephemeral_job(3);

        assert!(matches!(
            admit(&mut scheduler, first),
            AdmissionOutcome::Accepted {
                token: AdmissionToken {
                    job_id: SyncJobId(1),
                    generation: 1,
                },
                coalesced: None
            }
        ));
        admit(&mut scheduler, second);

        let rejection = scheduler.admit(rejected, 10_000, None).unwrap_err();
        assert_eq!(rejection.reason(), AdmissionRejectionReason::AtCapacity);
        assert_eq!(rejection.into_job().id(), SyncJobId::new(3));

        let running = scheduler.dispatch_next(10_000).unwrap();
        assert!(scheduler.dispatch_next(10_000).is_none());
        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Succeeded, 10_000),
            Ok(CompletionOutcome::Completed)
        );
        assert!(scheduler.dispatch_next(10_000).is_some());
    }

    #[test]
    fn one_admitted_job_can_only_be_dispatched_once() {
        let mut scheduler = scheduler(3, 2, 3);
        admit(&mut scheduler, ephemeral_job(1));

        let first = scheduler.dispatch_next(10_000).unwrap();
        assert_eq!(first.job().id(), SyncJobId::new(1));
        assert!(scheduler.dispatch_next(10_000).is_none());

        admit(&mut scheduler, ephemeral_job(2));
        let second = scheduler.dispatch_next(10_000).unwrap();
        assert_eq!(second.job().id(), SyncJobId::new(2));
        assert!(scheduler.dispatch_next(10_000).is_none());
    }

    #[test]
    fn active_job_and_cancellation_identities_are_unique() {
        let mut scheduler = scheduler(3, 2, 3);
        let admitted = ephemeral_job(1);
        admit(&mut scheduler, admitted.clone());

        let duplicate_job_id = SyncJob::new(
            admitted.id(),
            CancellationId::new(9_999),
            conversation_target(2),
            SyncPriority::Foreground,
            SyncDurability::Ephemeral,
            FreshnessPolicy::Always,
            ReplacementClass::Never,
            RetryPolicy::Never,
        )
        .unwrap();
        let rejection = scheduler.admit(duplicate_job_id, 10_000, None).unwrap_err();
        assert_eq!(
            rejection.reason(),
            AdmissionRejectionReason::DuplicateIdentity
        );

        let running = scheduler.dispatch_next(10_000).unwrap();
        let duplicate_cancellation_id = SyncJob::new(
            SyncJobId::new(3),
            admitted.cancellation_id(),
            conversation_target(3),
            SyncPriority::Foreground,
            SyncDurability::Ephemeral,
            FreshnessPolicy::Always,
            ReplacementClass::Never,
            RetryPolicy::Never,
        )
        .unwrap();
        let rejection = scheduler
            .admit(duplicate_cancellation_id, 10_000, None)
            .unwrap_err();
        assert_eq!(
            rejection.reason(),
            AdmissionRejectionReason::DuplicateIdentity
        );
        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Succeeded, 10_000),
            Ok(CompletionOutcome::Completed)
        );
    }

    #[test]
    fn running_and_total_capacity_remain_reserved_after_dispatch() {
        let mut scheduler = scheduler(3, 2, 3);
        for id in 1..=3 {
            admit(&mut scheduler, ephemeral_job(id));
        }

        let first = scheduler.dispatch_next(10_000).unwrap();
        let second = scheduler.dispatch_next(10_000).unwrap();
        assert!(scheduler.dispatch_next(10_000).is_none());
        assert_eq!(
            scheduler
                .admit(ephemeral_job(4), 10_000, None)
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::AtCapacity
        );

        assert_eq!(
            scheduler.complete(first.run(), JobOutcome::Succeeded, 10_000),
            Ok(CompletionOutcome::Completed)
        );
        admit(&mut scheduler, ephemeral_job(4));
        assert!(scheduler.dispatch_next(10_000).is_some());
        assert!(scheduler.dispatch_next(10_000).is_none());
        assert_eq!(second.attempt(), 1);
    }

    #[test]
    fn stale_run_token_cannot_complete_a_readmitted_identity() {
        let mut scheduler = scheduler(1, 1, 3);
        admit(&mut scheduler, ephemeral_job(1));
        let old_run = scheduler.dispatch_next(10_000).unwrap();
        assert_eq!(
            scheduler.complete(old_run.run(), JobOutcome::Succeeded, 10_000),
            Ok(CompletionOutcome::Completed)
        );

        admit(&mut scheduler, ephemeral_job(1));
        let current_run = scheduler.dispatch_next(10_000).unwrap();
        assert_eq!(
            scheduler.complete(old_run.run(), JobOutcome::Succeeded, 10_000),
            Err(CompletionError::StaleRun)
        );
        assert_eq!(
            scheduler.complete(current_run.run(), JobOutcome::Succeeded, 10_000),
            Ok(CompletionOutcome::Completed)
        );
    }

    #[test]
    fn dispatch_prefers_interactive_then_foreground_then_maintenance() {
        let mut scheduler = scheduler(3, 1, 10);
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(1), SyncPriority::Maintenance),
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(2), SyncPriority::Foreground),
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(3), SyncPriority::Interactive),
        );

        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(3)
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(2)
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(1)
        );
    }

    #[test]
    fn accepted_maintenance_runs_after_the_bounded_higher_priority_burst() {
        let mut scheduler = scheduler(8, 1, 2);
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(1), SyncPriority::Maintenance),
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(2), SyncPriority::Interactive),
        );

        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(2)
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(3), SyncPriority::Interactive),
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(3)
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(4), SyncPriority::Interactive),
        );

        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(1)
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(4)
        );
    }

    #[test]
    fn higher_priority_work_before_maintenance_arrives_does_not_spend_its_fairness_budget() {
        let mut scheduler = scheduler(8, 1, 2);
        for id in 1..=4 {
            admit(
                &mut scheduler,
                with_priority(ephemeral_job(id), SyncPriority::Interactive),
            );
            assert_eq!(
                dispatch_and_complete(&mut scheduler, 10_000),
                SyncJobId::new(id)
            );
        }

        admit(
            &mut scheduler,
            with_priority(ephemeral_job(5), SyncPriority::Maintenance),
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(6), SyncPriority::Interactive),
        );

        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(6)
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(7), SyncPriority::Interactive),
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(7)
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(8), SyncPriority::Interactive),
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(5)
        );
    }

    #[test]
    fn foreground_work_is_not_starved_by_continuous_interactive_work() {
        let mut scheduler = scheduler(6, 1, 2);
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(1), SyncPriority::Foreground),
        );
        for id in 2..=4 {
            admit(
                &mut scheduler,
                with_priority(ephemeral_job(id), SyncPriority::Interactive),
            );
        }

        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(2)
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(3)
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(1)
        );
    }

    #[test]
    fn each_lane_counts_only_strictly_higher_priority_dispatches_for_fairness() {
        let mut scheduler = scheduler(4, 1, 2);
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(1), SyncPriority::Maintenance),
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(2), SyncPriority::Foreground),
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(3), SyncPriority::Interactive),
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(4), SyncPriority::Interactive),
        );

        let order = (0..4)
            .map(|_| dispatch_and_complete(&mut scheduler, 10_000))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                SyncJobId::new(3),
                SyncJobId::new(4),
                SyncJobId::new(1),
                SyncJobId::new(2),
            ]
        );
    }

    #[test]
    fn same_priority_jobs_dispatch_in_admission_order() {
        let mut scheduler = scheduler(3, 1, 3);
        for id in 1..=3 {
            admit(&mut scheduler, ephemeral_job(id));
        }

        for id in 1..=3 {
            assert_eq!(
                dispatch_and_complete(&mut scheduler, 10_000),
                SyncJobId::new(id)
            );
        }
    }

    #[test]
    fn fresh_ephemeral_work_is_returned_without_consuming_capacity() {
        let mut scheduler = scheduler(1, 1, 3);
        let mut fresh = ephemeral_job(1);
        fresh.freshness = FreshnessPolicy::IfOlderThan { max_age_ms: 1_000 };

        match scheduler.admit(fresh, 10_000, Some(9_500)).unwrap() {
            AdmissionOutcome::SkippedFresh(job) => assert_eq!(job.id(), SyncJobId::new(1)),
            other => panic!("unexpected admission outcome: {other:?}"),
        }
        admit(&mut scheduler, ephemeral_job(2));
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(2)
        );
    }

    fn replaceable_job(id: u64, target_id: u64, refresh_class: RefreshClass) -> SyncJob {
        SyncJob::new(
            SyncJobId::new(id),
            CancellationId::new(id + 1_000),
            conversation_target(target_id),
            SyncPriority::Foreground,
            SyncDurability::Ephemeral,
            FreshnessPolicy::Always,
            ReplacementClass::Refresh(refresh_class),
            RetryPolicy::Never,
        )
        .unwrap()
    }

    fn durable_job(id: u64, durability: SyncDurability, retry: RetryPolicy) -> SyncJob {
        SyncJob::new(
            SyncJobId::new(id),
            CancellationId::new(id + 1_000),
            conversation_target(id),
            SyncPriority::Foreground,
            durability,
            FreshnessPolicy::Always,
            ReplacementClass::Never,
            retry,
        )
        .unwrap()
    }

    #[test]
    fn full_capacity_replacement_matches_both_target_and_refresh_class() {
        let mut scheduler = scheduler(3, 1, 3);
        admit(
            &mut scheduler,
            replaceable_job(1, 10, RefreshClass::ConversationHistory),
        );
        admit(
            &mut scheduler,
            replaceable_job(2, 10, RefreshClass::Presence),
        );
        admit(
            &mut scheduler,
            replaceable_job(3, 20, RefreshClass::ConversationHistory),
        );

        assert_eq!(
            scheduler
                .admit(
                    replaceable_job(4, 10, RefreshClass::ConversationHistory),
                    10_000,
                    None,
                )
                .unwrap(),
            AdmissionOutcome::Accepted {
                token: AdmissionToken::new(SyncJobId::new(4), 4),
                coalesced: Some(ReleasedJob::new(
                    replaceable_job(1, 10, RefreshClass::ConversationHistory),
                    1,
                )),
            }
        );
        assert_eq!(
            scheduler
                .admit(ephemeral_job(5), 10_000, None)
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::AtCapacity
        );

        let dispatched = (0..3)
            .map(|_| dispatch_and_complete(&mut scheduler, 10_000))
            .collect::<Vec<_>>();
        assert_eq!(
            dispatched,
            vec![SyncJobId::new(2), SyncJobId::new(3), SyncJobId::new(4),]
        );
    }

    #[test]
    fn running_refresh_is_not_coalesced_or_cancelled_by_a_new_generation() {
        let mut scheduler = scheduler(2, 1, 3);
        admit(
            &mut scheduler,
            replaceable_job(1, 10, RefreshClass::ConversationHistory),
        );
        let running = scheduler.dispatch_next(10_000).unwrap();

        assert_eq!(
            scheduler
                .admit(
                    replaceable_job(2, 10, RefreshClass::ConversationHistory),
                    10_000,
                    None,
                )
                .unwrap(),
            AdmissionOutcome::Accepted {
                token: AdmissionToken::new(SyncJobId::new(2), 2),
                coalesced: None,
            }
        );
        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Succeeded, 10_000),
            Ok(CompletionOutcome::Completed)
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(2)
        );
    }

    #[test]
    fn lower_priority_refresh_cannot_replace_queued_interactive_work() {
        let mut scheduler = scheduler(3, 1, 3);
        let mut interactive = replaceable_job(1, 10, RefreshClass::UserDirectory);
        interactive.priority = SyncPriority::Interactive;
        assert_eq!(
            scheduler.admit(interactive.clone(), 10_000, None).unwrap(),
            AdmissionOutcome::Accepted {
                token: AdmissionToken::new(SyncJobId::new(1), 1),
                coalesced: None,
            }
        );

        let mut maintenance = replaceable_job(2, 10, RefreshClass::UserDirectory);
        maintenance.priority = SyncPriority::Maintenance;
        assert_eq!(
            scheduler.admit(maintenance.clone(), 10_000, None).unwrap(),
            AdmissionOutcome::Accepted {
                token: AdmissionToken::new(SyncJobId::new(2), 2),
                coalesced: None,
            }
        );

        let first = scheduler.dispatch_next(10_000).unwrap();
        assert_eq!(first.job().id(), interactive.id());
        assert_eq!(
            scheduler.complete(first.run(), JobOutcome::Succeeded, 10_000),
            Ok(CompletionOutcome::Completed)
        );
        assert_eq!(scheduler.dispatch_next(10_000).unwrap().job(), &maintenance);
    }

    #[test]
    fn interactive_refresh_replaces_queued_maintenance_work() {
        let mut scheduler = scheduler(2, 1, 3);
        let mut maintenance = replaceable_job(1, 10, RefreshClass::UserDirectory);
        maintenance.priority = SyncPriority::Maintenance;
        scheduler.admit(maintenance.clone(), 10_000, None).unwrap();

        let mut interactive = replaceable_job(2, 10, RefreshClass::UserDirectory);
        interactive.priority = SyncPriority::Interactive;
        assert_eq!(
            scheduler.admit(interactive.clone(), 10_000, None).unwrap(),
            AdmissionOutcome::Accepted {
                token: AdmissionToken::new(SyncJobId::new(2), 2),
                coalesced: Some(ReleasedJob::new(maintenance, 1)),
            }
        );
        assert_eq!(scheduler.dispatch_next(10_000).unwrap().job(), &interactive);
    }

    #[test]
    fn queued_refresh_waits_for_running_work_on_the_same_target() {
        let mut scheduler = scheduler(2, 2, 3);
        admit(
            &mut scheduler,
            replaceable_job(1, 10, RefreshClass::ConversationHistory),
        );
        let running = scheduler.dispatch_next(10_000).unwrap();
        admit(
            &mut scheduler,
            replaceable_job(2, 10, RefreshClass::ConversationHistory),
        );

        assert!(scheduler.dispatch_next(10_000).is_none());
        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Succeeded, 10_000),
            Ok(CompletionOutcome::Completed)
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(2)
        );
    }

    #[test]
    fn newest_queued_refresh_coalesces_while_the_current_generation_runs() {
        let mut scheduler = scheduler(3, 2, 3);
        let older_queued = replaceable_job(2, 10, RefreshClass::ConversationHistory);
        let newest = replaceable_job(3, 10, RefreshClass::ConversationHistory);
        admit(
            &mut scheduler,
            replaceable_job(1, 10, RefreshClass::ConversationHistory),
        );
        let running = scheduler.dispatch_next(10_000).unwrap();
        admit(&mut scheduler, older_queued.clone());

        assert_eq!(
            scheduler.admit(newest.clone(), 10_001, None).unwrap(),
            AdmissionOutcome::Accepted {
                token: AdmissionToken::new(newest.id(), 3),
                coalesced: Some(ReleasedJob::new(older_queued, 1)),
            }
        );
        assert!(scheduler.dispatch_next(10_001).is_none());

        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Succeeded, 10_001),
            Ok(CompletionOutcome::Completed)
        );
        assert_eq!(dispatch_and_complete(&mut scheduler, 10_001), newest.id());
    }

    #[test]
    fn blocked_interactive_refresh_does_not_hide_dispatchable_maintenance_work() {
        let mut scheduler = scheduler(3, 2, 3);
        admit(
            &mut scheduler,
            replaceable_job(1, 10, RefreshClass::ConversationHistory),
        );
        let running = scheduler.dispatch_next(10_000).unwrap();
        admit(
            &mut scheduler,
            with_priority(
                replaceable_job(2, 10, RefreshClass::ConversationHistory),
                SyncPriority::Interactive,
            ),
        );
        admit(
            &mut scheduler,
            with_priority(ephemeral_job(3), SyncPriority::Maintenance),
        );

        let unrelated = scheduler.dispatch_next(10_000).unwrap();
        assert_eq!(unrelated.job().id(), SyncJobId::new(3));
        assert_eq!(
            scheduler.complete(unrelated.run(), JobOutcome::Succeeded, 10_000),
            Ok(CompletionOutcome::Completed)
        );
        assert!(scheduler.dispatch_next(10_000).is_none());

        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Succeeded, 10_000),
            Ok(CompletionOutcome::Completed)
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(2)
        );
    }

    #[test]
    fn queued_cancellation_releases_capacity_but_durable_work_is_protected() {
        let mut scheduler = scheduler(2, 1, 3);
        let ephemeral = ephemeral_job(1);
        let durable = durable_job(2, SyncDurability::ReadMarker, RetryPolicy::Never);
        admit(&mut scheduler, ephemeral.clone());
        admit(&mut scheduler, durable.clone());

        assert_eq!(
            scheduler.cancel(ephemeral.cancellation_id(), 10_000),
            CancellationOutcome::Cancelled(ReleasedJob::new(ephemeral.clone(), 1))
        );
        admit(&mut scheduler, ephemeral_job(3));
        assert_eq!(
            scheduler.cancel(durable.cancellation_id(), 10_000),
            CancellationOutcome::Protected {
                job_id: durable.id()
            }
        );
        assert_eq!(
            scheduler.cancel(CancellationId::new(99_999), 10_000),
            CancellationOutcome::NotFound
        );
    }

    #[test]
    fn cancelling_the_last_waiting_job_resets_that_lanes_fairness_credit() {
        let mut scheduler = scheduler(4, 1, 1);
        let old_maintenance = with_priority(ephemeral_job(1), SyncPriority::Maintenance);
        scheduler
            .admit(old_maintenance.clone(), 10_000, None)
            .unwrap();
        scheduler
            .admit(
                with_priority(ephemeral_job(2), SyncPriority::Interactive),
                10_000,
                None,
            )
            .unwrap();
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(2)
        );
        assert!(matches!(
            scheduler.cancel(old_maintenance.cancellation_id(), 10_000),
            CancellationOutcome::Cancelled(_)
        ));

        scheduler
            .admit(
                with_priority(ephemeral_job(3), SyncPriority::Maintenance),
                10_000,
                None,
            )
            .unwrap();
        scheduler
            .admit(
                with_priority(ephemeral_job(4), SyncPriority::Interactive),
                10_000,
                None,
            )
            .unwrap();
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(4)
        );
    }

    #[test]
    fn cancelling_the_only_eligible_job_does_not_transfer_credit_to_a_delayed_retry() {
        let mut scheduler = scheduler(4, 1, 1);
        let mut delayed_retry = with_priority(ephemeral_job(1), SyncPriority::Maintenance);
        delayed_retry.retry = RetryPolicy::fixed(2, 250).unwrap();
        scheduler.admit(delayed_retry.clone(), 1_000, None).unwrap();
        let first_attempt = scheduler.dispatch_next(1_000).unwrap();
        assert!(matches!(
            scheduler.complete(first_attempt.run(), JobOutcome::RetryableFailure, 1_000),
            Ok(CompletionOutcome::Retried {
                ready_at_ms: 1_250,
                ..
            })
        ));

        let eligible_maintenance = with_priority(ephemeral_job(2), SyncPriority::Maintenance);
        scheduler
            .admit(eligible_maintenance.clone(), 1_001, None)
            .unwrap();
        scheduler
            .admit(
                with_priority(ephemeral_job(3), SyncPriority::Interactive),
                1_001,
                None,
            )
            .unwrap();
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 1_001),
            SyncJobId::new(3)
        );
        assert!(matches!(
            scheduler.cancel(eligible_maintenance.cancellation_id(), 1_001),
            CancellationOutcome::Cancelled(_)
        ));

        scheduler
            .admit(
                with_priority(ephemeral_job(4), SyncPriority::Interactive),
                1_002,
                None,
            )
            .unwrap();
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 1_250),
            SyncJobId::new(4)
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 1_250),
            delayed_retry.id()
        );
    }

    #[test]
    fn cross_lane_coalescing_does_not_transfer_stale_fairness_credit() {
        let mut scheduler = scheduler(4, 1, 1);
        let old = with_priority(
            replaceable_job(1, 10, RefreshClass::ConversationHistory),
            SyncPriority::Maintenance,
        );
        scheduler.admit(old, 10_000, None).unwrap();
        scheduler
            .admit(
                with_priority(ephemeral_job(2), SyncPriority::Interactive),
                10_000,
                None,
            )
            .unwrap();
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(2)
        );
        let replacement = with_priority(
            replaceable_job(3, 10, RefreshClass::ConversationHistory),
            SyncPriority::Foreground,
        );
        assert!(matches!(
            scheduler.admit(replacement, 10_000, None).unwrap(),
            AdmissionOutcome::Accepted {
                coalesced: Some(_),
                ..
            }
        ));
        scheduler
            .admit(
                with_priority(ephemeral_job(4), SyncPriority::Maintenance),
                10_000,
                None,
            )
            .unwrap();
        scheduler
            .admit(
                with_priority(ephemeral_job(5), SyncPriority::Interactive),
                10_000,
                None,
            )
            .unwrap();

        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            SyncJobId::new(5)
        );
    }

    #[test]
    fn running_cancellation_keeps_capacity_until_the_worker_acknowledges_it() {
        let mut scheduler = scheduler(1, 1, 3);
        let job = ephemeral_job(1);
        admit(&mut scheduler, job.clone());
        let running = scheduler.dispatch_next(10_000).unwrap();
        let directive = CancellationDirective {
            run: running.run(),
            cancellation_id: job.cancellation_id(),
        };

        assert_eq!(
            scheduler.cancel(job.cancellation_id(), 10_000),
            CancellationOutcome::Requested(directive)
        );
        assert_eq!(
            scheduler.cancel(job.cancellation_id(), 10_000),
            CancellationOutcome::AlreadyRequested(directive)
        );
        assert_eq!(
            scheduler
                .admit(ephemeral_job(2), 10_000, None)
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::AtCapacity
        );
        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Cancelled, 10_000),
            Ok(CompletionOutcome::Cancelled(ReleasedJob::new(job, 1)))
        );
        admit(&mut scheduler, ephemeral_job(2));
    }

    #[test]
    fn unrequested_running_cancellation_is_rejected_without_releasing_capacity() {
        let mut scheduler = scheduler(1, 1, 3);
        let job = ephemeral_job(1);
        admit(&mut scheduler, job.clone());
        let running = scheduler.dispatch_next(10_000).unwrap();

        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Cancelled, 10_000),
            Err(CompletionError::CancellationNotRequested)
        );
        assert_eq!(
            scheduler
                .admit(ephemeral_job(2), 10_000, None)
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::AtCapacity
        );
        assert!(matches!(
            scheduler.cancel(job.cancellation_id(), 10_000),
            CancellationOutcome::Requested(_)
        ));
        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Cancelled, 10_000),
            Ok(CompletionOutcome::Cancelled(ReleasedJob::new(job, 1)))
        );
    }

    #[test]
    fn durable_running_work_rejects_cancellation_without_releasing_capacity() {
        for durability in [SyncDurability::DurableAction, SyncDurability::ReadMarker] {
            let mut scheduler = scheduler(1, 1, 3);
            let job = durable_job(1, durability, RetryPolicy::Never);
            admit(&mut scheduler, job.clone());
            let running = scheduler.dispatch_next(10_000).unwrap();

            assert_eq!(
                scheduler.cancel(job.cancellation_id(), 10_000),
                CancellationOutcome::Protected { job_id: job.id() }
            );
            assert_eq!(
                scheduler.complete(running.run(), JobOutcome::Cancelled, 10_000),
                Err(CompletionError::DurableCancellationForbidden)
            );
            assert_eq!(
                scheduler
                    .admit(ephemeral_job(2), 10_000, None)
                    .unwrap_err()
                    .reason(),
                AdmissionRejectionReason::AtCapacity
            );
            assert_eq!(
                scheduler.complete(running.run(), JobOutcome::Succeeded, 10_000),
                Ok(CompletionOutcome::Completed)
            );
        }
    }

    #[test]
    fn delayed_retry_retains_admission_and_exhausts_at_the_exact_attempt_limit() {
        let mut scheduler = scheduler(1, 1, 3);
        let mut job = ephemeral_job(1);
        job.retry = RetryPolicy::fixed(3, 250).unwrap();
        scheduler.admit(job.clone(), 1_000, None).unwrap();

        let first = scheduler.dispatch_next(1_000).unwrap();
        assert_eq!(
            scheduler.complete(first.run(), JobOutcome::RetryableFailure, 1_000),
            Ok(CompletionOutcome::Retried {
                job_id: job.id(),
                attempt: 2,
                ready_at_ms: 1_250,
            })
        );
        assert_eq!(
            scheduler
                .admit(ephemeral_job(2), 1_001, None)
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::AtCapacity
        );
        assert!(scheduler.dispatch_next(1_249).is_none());

        let second = scheduler.dispatch_next(1_250).unwrap();
        assert_eq!(second.attempt(), 2);
        assert_eq!(
            scheduler.complete(first.run(), JobOutcome::Succeeded, 1_250),
            Err(CompletionError::StaleRun)
        );
        assert_eq!(
            scheduler.complete(second.run(), JobOutcome::RetryableFailure, 1_250),
            Ok(CompletionOutcome::Retried {
                job_id: job.id(),
                attempt: 3,
                ready_at_ms: 1_500,
            })
        );

        let third = scheduler.dispatch_next(1_500).unwrap();
        assert_eq!(third.attempt(), 3);
        assert_eq!(
            scheduler.complete(third.run(), JobOutcome::RetryableFailure, 1_500),
            Ok(CompletionOutcome::Failed(ReleasedJob::new(job, 3)))
        );
        admit(&mut scheduler, ephemeral_job(2));
    }

    #[test]
    fn newer_queued_refresh_supersedes_an_older_running_retry() {
        let mut scheduler = scheduler(2, 1, 3);
        let mut older = replaceable_job(1, 10, RefreshClass::ConversationHistory);
        older.retry = RetryPolicy::fixed(2, 250).unwrap();
        let newer = replaceable_job(2, 10, RefreshClass::ConversationHistory);
        scheduler.admit(older.clone(), 1_000, None).unwrap();
        let running = scheduler.dispatch_next(1_000).unwrap();
        scheduler.admit(newer.clone(), 1_001, None).unwrap();

        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::RetryableFailure, 1_010),
            Ok(CompletionOutcome::Superseded {
                released: ReleasedJob::new(older, 1),
                superseding: AdmissionToken::new(newer.id(), 2),
            })
        );
        assert_eq!(dispatch_and_complete(&mut scheduler, 1_010), newer.id());
        assert!(scheduler.dispatch_next(1_010).is_none());
    }

    #[test]
    fn lower_priority_refresh_does_not_supersede_an_interactive_retry() {
        let mut scheduler = scheduler(2, 1, 3);
        let mut interactive = with_priority(
            replaceable_job(1, 10, RefreshClass::UserDirectory),
            SyncPriority::Interactive,
        );
        interactive.retry = RetryPolicy::fixed(2, 250).unwrap();
        let maintenance = with_priority(
            replaceable_job(2, 10, RefreshClass::UserDirectory),
            SyncPriority::Maintenance,
        );
        scheduler.admit(interactive.clone(), 1_000, None).unwrap();
        let running = scheduler.dispatch_next(1_000).unwrap();
        scheduler.admit(maintenance.clone(), 1_001, None).unwrap();

        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::RetryableFailure, 1_010),
            Ok(CompletionOutcome::Retried {
                job_id: interactive.id(),
                attempt: 2,
                ready_at_ms: 1_260,
            })
        );
        assert_eq!(scheduler.dispatch_next(1_260).unwrap().job(), &interactive);
    }

    #[test]
    fn different_refresh_classes_for_the_same_target_are_serialized() {
        let mut scheduler = scheduler(2, 2, 3);
        let older = replaceable_job(1, 10, RefreshClass::Presence);
        let newer = replaceable_job(2, 10, RefreshClass::ConversationHistory);
        scheduler.admit(older.clone(), 1_000, None).unwrap();
        let older_run = scheduler.dispatch_next(1_000).unwrap();
        scheduler.admit(newer.clone(), 1_001, None).unwrap();

        assert!(scheduler.dispatch_next(1_001).is_none());
        assert_eq!(
            scheduler.complete(older_run.run(), JobOutcome::Succeeded, 1_010),
            Ok(CompletionOutcome::Completed)
        );
        assert_eq!(dispatch_and_complete(&mut scheduler, 1_010), newer.id());
    }

    #[test]
    fn queued_refresh_waits_for_non_refresh_work_on_the_same_target() {
        let mut scheduler = scheduler(2, 2, 3);
        let mut non_refresh = ephemeral_job(1);
        non_refresh.target = conversation_target(10);
        let refresh = replaceable_job(2, 10, RefreshClass::ConversationHistory);
        scheduler.admit(non_refresh.clone(), 1_000, None).unwrap();
        let non_refresh_run = scheduler.dispatch_next(1_000).unwrap();
        scheduler.admit(refresh.clone(), 1_001, None).unwrap();

        assert!(scheduler.dispatch_next(1_001).is_none());
        assert_eq!(
            scheduler.complete(non_refresh_run.run(), JobOutcome::Succeeded, 1_010),
            Ok(CompletionOutcome::Completed)
        );
        assert_eq!(dispatch_and_complete(&mut scheduler, 1_010), refresh.id());
    }

    #[test]
    fn reused_job_id_gets_a_new_admission_after_serialized_refresh_runs() {
        let mut scheduler = scheduler(2, 2, 3);
        let mut older = replaceable_job(1, 10, RefreshClass::ConversationHistory);
        older.retry = RetryPolicy::fixed(2, 250).unwrap();
        let newer = replaceable_job(2, 10, RefreshClass::ConversationHistory);
        scheduler.admit(older.clone(), 1_000, None).unwrap();
        let older_run = scheduler.dispatch_next(1_000).unwrap();
        let AdmissionOutcome::Accepted {
            token: newer_admission,
            ..
        } = scheduler.admit(newer, 1_001, None).unwrap()
        else {
            unreachable!("newer refresh must be admitted");
        };
        assert!(scheduler.dispatch_next(1_001).is_none());
        assert_eq!(
            scheduler.complete(older_run.run(), JobOutcome::RetryableFailure, 1_010),
            Ok(CompletionOutcome::Superseded {
                released: ReleasedJob::new(older, 1),
                superseding: newer_admission,
            })
        );

        let newer_run = scheduler.dispatch_next(1_010).unwrap();
        assert_eq!(
            scheduler.complete(newer_run.run(), JobOutcome::Succeeded, 1_010),
            Ok(CompletionOutcome::Completed)
        );

        let unrelated_reuse = replaceable_job(2, 20, RefreshClass::ConversationHistory);
        let AdmissionOutcome::Accepted {
            token: reused_admission,
            ..
        } = scheduler.admit(unrelated_reuse, 1_011, None).unwrap()
        else {
            unreachable!("unrelated reuse must be admitted");
        };
        assert_ne!(newer_admission, reused_admission);
    }

    #[test]
    fn delayed_retry_can_be_cancelled_with_its_attempt_handed_back() {
        let mut scheduler = scheduler(1, 1, 3);
        let mut job = ephemeral_job(1);
        job.retry = RetryPolicy::fixed(2, 250).unwrap();
        scheduler.admit(job.clone(), 1_000, None).unwrap();
        let running = scheduler.dispatch_next(1_000).unwrap();
        assert!(matches!(
            scheduler.complete(running.run(), JobOutcome::RetryableFailure, 1_000),
            Ok(CompletionOutcome::Retried { attempt: 2, .. })
        ));

        assert_eq!(
            scheduler.cancel(job.cancellation_id(), 1_000),
            CancellationOutcome::Cancelled(ReleasedJob::new(job, 2))
        );
        admit(&mut scheduler, ephemeral_job(2));
    }

    #[test]
    fn delayed_retry_does_not_head_block_ready_work_in_the_same_lane() {
        let mut scheduler = scheduler(2, 1, 3);
        let mut delayed = ephemeral_job(1);
        delayed.retry = RetryPolicy::fixed(2, 250).unwrap();
        scheduler.admit(delayed, 1_000, None).unwrap();
        let first = scheduler.dispatch_next(1_000).unwrap();
        assert!(matches!(
            scheduler.complete(first.run(), JobOutcome::RetryableFailure, 1_000),
            Ok(CompletionOutcome::Retried {
                ready_at_ms: 1_250,
                ..
            })
        ));
        scheduler.admit(ephemeral_job(2), 1_001, None).unwrap();

        assert_eq!(
            dispatch_and_complete(&mut scheduler, 1_001),
            SyncJobId::new(2)
        );
        assert!(scheduler.dispatch_next(1_249).is_none());
        assert_eq!(
            scheduler.dispatch_next(1_250).unwrap().job().id(),
            SyncJobId::new(1)
        );
    }

    #[test]
    fn exact_replacement_of_a_delayed_retry_returns_its_attempt_at_full_capacity() {
        let mut scheduler = scheduler(1, 1, 3);
        let mut older = replaceable_job(1, 10, RefreshClass::ConversationHistory);
        older.retry = RetryPolicy::fixed(2, 250).unwrap();
        scheduler.admit(older.clone(), 1_000, None).unwrap();
        let running = scheduler.dispatch_next(1_000).unwrap();
        assert!(matches!(
            scheduler.complete(running.run(), JobOutcome::RetryableFailure, 1_000),
            Ok(CompletionOutcome::Retried { attempt: 2, .. })
        ));
        let newer = replaceable_job(2, 10, RefreshClass::ConversationHistory);

        assert_eq!(
            scheduler.admit(newer.clone(), 1_001, None).unwrap(),
            AdmissionOutcome::Accepted {
                token: AdmissionToken::new(newer.id(), 2),
                coalesced: Some(ReleasedJob::new(older, 2)),
            }
        );
        assert_eq!(
            scheduler.dispatch_next(1_001).unwrap().job().id(),
            newer.id()
        );
    }

    #[test]
    fn cancellation_request_races_never_retry_or_release_before_completion() {
        for (outcome, expected) in [
            (JobOutcome::Succeeded, None),
            (
                JobOutcome::PermanentFailure,
                Some(JobOutcome::PermanentFailure),
            ),
            (
                JobOutcome::RetryableFailure,
                Some(JobOutcome::RetryableFailure),
            ),
        ] {
            let mut scheduler = scheduler(1, 1, 3);
            let mut job = ephemeral_job(1);
            job.retry = RetryPolicy::fixed(2, 250).unwrap();
            scheduler.admit(job.clone(), 1_000, None).unwrap();
            let running = scheduler.dispatch_next(1_000).unwrap();
            assert!(matches!(
                scheduler.cancel(job.cancellation_id(), 1_000),
                CancellationOutcome::Requested(_)
            ));

            let completion = scheduler.complete(running.run(), outcome, 1_000);
            if expected.is_some() {
                assert_eq!(
                    completion,
                    Ok(CompletionOutcome::Cancelled(ReleasedJob::new(job, 1)))
                );
            } else {
                assert_eq!(completion, Ok(CompletionOutcome::Completed));
            }
            scheduler.admit(ephemeral_job(2), 1_001, None).unwrap();
            assert_eq!(
                scheduler.dispatch_next(1_500).unwrap().job().id(),
                SyncJobId::new(2)
            );
        }
    }

    #[test]
    fn shutdown_cancels_best_effort_and_drains_durable_work() {
        let mut scheduler = scheduler(4, 1, 3);
        let running_ephemeral = with_priority(ephemeral_job(1), SyncPriority::Interactive);
        let queued_ephemeral = ephemeral_job(2);
        let durable_action = durable_job(3, SyncDurability::DurableAction, RetryPolicy::Never);
        let read_marker = durable_job(4, SyncDurability::ReadMarker, RetryPolicy::Never);
        admit(&mut scheduler, running_ephemeral.clone());
        let running = scheduler.dispatch_next(10_000).unwrap();
        admit(&mut scheduler, queued_ephemeral.clone());
        admit(&mut scheduler, durable_action.clone());
        admit(&mut scheduler, read_marker.clone());

        let report = scheduler.begin_shutdown(10_000);
        assert_eq!(report.phase(), ShutdownPhase::Draining);
        assert_eq!(
            report.cancelled(),
            &[ReleasedJob::new(queued_ephemeral.clone(), 1)]
        );
        assert_eq!(
            report.cancellation_requested(),
            &[CancellationDirective {
                run: running.run(),
                cancellation_id: running_ephemeral.cancellation_id(),
            }]
        );
        assert_eq!(
            scheduler
                .admit(ephemeral_job(5), 10_000, None)
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::ShuttingDown
        );
        assert!(scheduler.dispatch_next(10_000).is_none());

        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Cancelled, 10_000),
            Ok(CompletionOutcome::Cancelled(ReleasedJob::new(
                running_ephemeral,
                1,
            )))
        );
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            durable_action.id()
        );
        assert_eq!(scheduler.shutdown_phase(), ShutdownPhase::Draining);
        assert_eq!(
            dispatch_and_complete(&mut scheduler, 10_000),
            read_marker.id()
        );
        assert_eq!(scheduler.shutdown_phase(), ShutdownPhase::Drained);
        assert!(scheduler.is_drained());

        let repeated = scheduler.begin_shutdown(10_000);
        assert_eq!(repeated.phase(), ShutdownPhase::Drained);
        assert!(repeated.cancelled().is_empty());
        assert!(repeated.cancellation_requested().is_empty());
    }

    #[test]
    fn shutdown_allows_durable_delayed_retry_to_finish() {
        let mut scheduler = scheduler(1, 1, 3);
        let job = durable_job(
            1,
            SyncDurability::DurableAction,
            RetryPolicy::fixed(2, 250).unwrap(),
        );
        scheduler.admit(job.clone(), 1_000, None).unwrap();
        let first = scheduler.dispatch_next(1_000).unwrap();
        assert_eq!(
            scheduler.begin_shutdown(1_000).phase(),
            ShutdownPhase::Draining
        );

        assert_eq!(
            scheduler.complete(first.run(), JobOutcome::RetryableFailure, 1_000),
            Ok(CompletionOutcome::Retried {
                job_id: job.id(),
                attempt: 2,
                ready_at_ms: 1_250,
            })
        );
        assert!(scheduler.dispatch_next(1_249).is_none());
        let retry = scheduler.dispatch_next(1_250).unwrap();
        assert_eq!(
            scheduler.complete(retry.run(), JobOutcome::Succeeded, 1_250),
            Ok(CompletionOutcome::Completed)
        );
        assert_eq!(scheduler.shutdown_phase(), ShutdownPhase::Drained);
    }

    #[test]
    fn shutdown_releases_ephemeral_delayed_retry_with_its_attempt() {
        let mut scheduler = scheduler(1, 1, 3);
        let mut job = ephemeral_job(1);
        job.retry = RetryPolicy::fixed(2, 250).unwrap();
        scheduler.admit(job.clone(), 1_000, None).unwrap();
        let running = scheduler.dispatch_next(1_000).unwrap();
        assert!(matches!(
            scheduler.complete(running.run(), JobOutcome::RetryableFailure, 1_000),
            Ok(CompletionOutcome::Retried { attempt: 2, .. })
        ));

        let report = scheduler.begin_shutdown(1_000);
        assert_eq!(report.phase(), ShutdownPhase::Drained);
        assert_eq!(report.cancelled(), &[ReleasedJob::new(job, 2)]);
    }

    #[test]
    fn repeated_shutdown_while_draining_does_not_repeat_cancellation_directives() {
        let mut scheduler = scheduler(1, 1, 3);
        let job = ephemeral_job(1);
        scheduler.admit(job.clone(), 1_000, None).unwrap();
        let running = scheduler.dispatch_next(1_000).unwrap();

        let first = scheduler.begin_shutdown(1_000);
        assert_eq!(first.phase(), ShutdownPhase::Draining);
        assert_eq!(first.cancellation_requested().len(), 1);
        let repeated = scheduler.begin_shutdown(1_000);
        assert_eq!(repeated.phase(), ShutdownPhase::Draining);
        assert!(repeated.cancelled().is_empty());
        assert!(repeated.cancellation_requested().is_empty());

        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Cancelled, 1_000),
            Ok(CompletionOutcome::Cancelled(ReleasedJob::new(job, 1)))
        );
        assert_eq!(scheduler.shutdown_phase(), ShutdownPhase::Drained);
    }

    #[test]
    fn durable_terminal_failure_is_handed_back_before_shutdown_drains() {
        let mut scheduler = scheduler(1, 1, 3);
        let job = durable_job(1, SyncDurability::ReadMarker, RetryPolicy::Never);
        scheduler.admit(job.clone(), 1_000, None).unwrap();
        let running = scheduler.dispatch_next(1_000).unwrap();
        assert_eq!(
            scheduler.begin_shutdown(1_000).phase(),
            ShutdownPhase::Draining
        );

        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::PermanentFailure, 1_000),
            Ok(CompletionOutcome::Failed(ReleasedJob::new(job, 1)))
        );
        assert_eq!(scheduler.shutdown_phase(), ShutdownPhase::Drained);
    }

    #[test]
    fn empty_shutdown_is_immediately_drained_and_rejects_new_work() {
        let mut scheduler = scheduler(1, 1, 3);

        assert_eq!(
            scheduler.begin_shutdown(10_000).phase(),
            ShutdownPhase::Drained
        );
        let rejection = scheduler.admit(ephemeral_job(1), 10_000, None).unwrap_err();
        assert_eq!(rejection.reason(), AdmissionRejectionReason::ShuttingDown);
        assert_eq!(rejection.into_job().id(), SyncJobId::new(1));
    }

    fn assert_admission_counters_reconcile(counters: SchedulerCounters) {
        let accounted = counters
            .completed()
            .saturating_add(counters.failed())
            .saturating_add(counters.cancellation_completed())
            .saturating_add(counters.coalesced())
            .saturating_add(counters.queued_depth() as u64)
            .saturating_add(counters.running_depth() as u64);
        assert_eq!(counters.admitted(), accounted);
    }

    #[test]
    fn scheduler_counters_are_redacted_and_empty_at_start() {
        let mut scheduler = scheduler(2, 1, 3);
        let initial = scheduler.counters();

        assert_eq!(initial.admitted(), 0);
        assert_eq!(initial.queued_depth(), 0);
        assert_eq!(initial.running_depth(), 0);
        assert_eq!(initial.coalesced(), 0);
        assert_eq!(initial.cancellation_requested(), 0);
        assert_eq!(initial.cancellation_completed(), 0);
        assert_eq!(initial.completed(), 0);
        assert_eq!(initial.failed(), 0);
        assert_eq!(initial.retried(), 0);
        assert_eq!(initial.skipped_fresh(), 0);
        assert_eq!(initial.rejected(AdmissionRejectionReason::AtCapacity), 0);
        assert_eq!(
            initial.rejected(AdmissionRejectionReason::DuplicateIdentity),
            0
        );
        assert_eq!(initial.rejected(AdmissionRejectionReason::ShuttingDown), 0);
        assert_eq!(initial.queue_high_water(), 0);
        assert_eq!(initial.running_high_water(), 0);
        assert_eq!(initial.shutdown_phase(), ShutdownPhase::Open);

        let mut fresh = ephemeral_job(123);
        fresh.freshness = FreshnessPolicy::IfOlderThan { max_age_ms: 500 };
        assert!(matches!(
            scheduler.admit(fresh, 1_000, Some(900)),
            Ok(AdmissionOutcome::SkippedFresh(_))
        ));
        assert_eq!(scheduler.counters().skipped_fresh(), 1);
        assert_eq!(scheduler.counters().admitted(), 0);
        scheduler
            .admit(ephemeral_job(987_654_321), 1_000, None)
            .unwrap();
        let rendered = format!("{:?}", scheduler.counters());
        for forbidden in [
            "987654321",
            "SyncJobId",
            "CancellationId",
            "SyncTarget",
            "opaque_id",
            "target",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "counter snapshot exposed {forbidden}: {rendered}"
            );
        }
    }

    #[test]
    fn burst_admission_and_dispatch_never_exceed_configured_bounds() {
        const ADMISSION_CAPACITY: usize = 32;
        const RUNNING_CAPACITY: usize = 3;
        const BURST_SIZE: u64 = 100;
        let mut scheduler = scheduler(ADMISSION_CAPACITY, RUNNING_CAPACITY, 3);

        for id in 1..=BURST_SIZE {
            let result = scheduler.admit(ephemeral_job(id), 1_000, None);
            if id <= ADMISSION_CAPACITY as u64 {
                assert!(matches!(result, Ok(AdmissionOutcome::Accepted { .. })));
            } else {
                assert_eq!(
                    result.unwrap_err().reason(),
                    AdmissionRejectionReason::AtCapacity
                );
            }
            let counters = scheduler.counters();
            assert!(counters.queued_depth() + counters.running_depth() <= ADMISSION_CAPACITY);
        }

        loop {
            let mut batch = Vec::new();
            while let Some(dispatched) = scheduler.dispatch_next(1_000) {
                batch.push(dispatched);
                let counters = scheduler.counters();
                assert!(counters.running_depth() <= RUNNING_CAPACITY);
                assert!(counters.queued_depth() + counters.running_depth() <= ADMISSION_CAPACITY);
            }
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() <= RUNNING_CAPACITY);
            for dispatched in batch {
                assert_eq!(
                    scheduler.complete(dispatched.run(), JobOutcome::Succeeded, 1_000),
                    Ok(CompletionOutcome::Completed)
                );
            }
        }

        let counters = scheduler.counters();
        assert_eq!(counters.admitted(), ADMISSION_CAPACITY as u64);
        assert_eq!(counters.completed(), ADMISSION_CAPACITY as u64);
        assert_eq!(
            counters.rejected(AdmissionRejectionReason::AtCapacity),
            BURST_SIZE - ADMISSION_CAPACITY as u64
        );
        assert_eq!(counters.queue_high_water(), ADMISSION_CAPACITY);
        assert_eq!(counters.running_high_water(), RUNNING_CAPACITY);
        assert_eq!(counters.queued_depth(), 0);
        assert_eq!(counters.running_depth(), 0);
        assert_admission_counters_reconcile(counters);
    }

    #[test]
    fn counters_account_exactly_through_coalescing_retry_cancellation_and_shutdown() {
        let mut scheduler = scheduler(5, 2, 3);
        let old_refresh = replaceable_job(1, 10, RefreshClass::ConversationHistory);
        let running_cancel = ephemeral_job(2);
        let durable_retry = durable_job(
            3,
            SyncDurability::DurableAction,
            RetryPolicy::fixed(2, 250).unwrap(),
        );
        let durable_failure = durable_job(4, SyncDurability::ReadMarker, RetryPolicy::Never);
        let shutdown_cancel = ephemeral_job(5);
        for job in [
            old_refresh.clone(),
            running_cancel.clone(),
            durable_retry.clone(),
            durable_failure.clone(),
            shutdown_cancel.clone(),
        ] {
            scheduler.admit(job, 1_000, None).unwrap();
        }
        assert_eq!(
            scheduler
                .admit(ephemeral_job(6), 1_000, None)
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::AtCapacity
        );
        assert_eq!(
            scheduler
                .admit(old_refresh.clone(), 1_000, None)
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::DuplicateIdentity
        );

        let new_refresh = replaceable_job(7, 10, RefreshClass::ConversationHistory);
        assert!(matches!(
            scheduler.admit(new_refresh.clone(), 1_000, None),
            Ok(AdmissionOutcome::Accepted {
                coalesced: Some(_),
                ..
            })
        ));
        let counters = scheduler.counters();
        assert_eq!(counters.admitted(), 6);
        assert_eq!(counters.coalesced(), 1);
        assert_eq!(counters.queued_depth(), 5);
        assert_eq!(counters.queue_high_water(), 5);
        assert_eq!(counters.rejected(AdmissionRejectionReason::AtCapacity), 1);
        assert_eq!(
            counters.rejected(AdmissionRejectionReason::DuplicateIdentity),
            1
        );
        assert_admission_counters_reconcile(counters);

        let cancel_run = scheduler.dispatch_next(1_000).unwrap();
        let retry_run = scheduler.dispatch_next(1_000).unwrap();
        assert_eq!(cancel_run.job().id(), running_cancel.id());
        assert_eq!(retry_run.job().id(), durable_retry.id());
        assert!(scheduler.dispatch_next(1_000).is_none());
        assert!(matches!(
            scheduler.cancel(running_cancel.cancellation_id(), 1_000),
            CancellationOutcome::Requested(_)
        ));
        assert!(matches!(
            scheduler.cancel(running_cancel.cancellation_id(), 1_000),
            CancellationOutcome::AlreadyRequested(_)
        ));
        assert_eq!(
            scheduler.complete(cancel_run.run(), JobOutcome::Cancelled, 1_000),
            Ok(CompletionOutcome::Cancelled(ReleasedJob::new(
                running_cancel,
                1
            )))
        );
        assert!(matches!(
            scheduler.complete(retry_run.run(), JobOutcome::RetryableFailure, 1_000),
            Ok(CompletionOutcome::Retried {
                attempt: 2,
                ready_at_ms: 1_250,
                ..
            })
        ));

        let failure_run = scheduler.dispatch_next(1_000).unwrap();
        let shutdown_run = scheduler.dispatch_next(1_000).unwrap();
        assert_eq!(failure_run.job().id(), durable_failure.id());
        assert_eq!(shutdown_run.job().id(), shutdown_cancel.id());
        assert_eq!(
            scheduler.complete(failure_run.run(), JobOutcome::PermanentFailure, 1_000),
            Ok(CompletionOutcome::Failed(ReleasedJob::new(
                durable_failure,
                1
            )))
        );

        let report = scheduler.begin_shutdown(1_000);
        assert_eq!(report.phase(), ShutdownPhase::Draining);
        assert_eq!(
            report.cancelled(),
            &[ReleasedJob::new(new_refresh.clone(), 1)]
        );
        assert_eq!(report.cancellation_requested().len(), 1);
        assert_eq!(
            scheduler
                .admit(ephemeral_job(8), 1_000, None)
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::ShuttingDown
        );
        assert_eq!(
            scheduler.complete(shutdown_run.run(), JobOutcome::Cancelled, 1_000),
            Ok(CompletionOutcome::Cancelled(ReleasedJob::new(
                shutdown_cancel,
                1
            )))
        );
        assert!(scheduler.dispatch_next(1_249).is_none());
        let durable_retry_run = scheduler.dispatch_next(1_250).unwrap();
        assert_eq!(durable_retry_run.job().id(), durable_retry.id());
        assert_eq!(durable_retry_run.attempt(), 2);
        assert_eq!(
            scheduler.complete(durable_retry_run.run(), JobOutcome::Succeeded, 1_250),
            Ok(CompletionOutcome::Completed)
        );

        let counters = scheduler.counters();
        assert_eq!(counters.admitted(), 6);
        assert_eq!(counters.coalesced(), 1);
        assert_eq!(counters.cancellation_requested(), 2);
        assert_eq!(counters.cancellation_completed(), 3);
        assert_eq!(counters.completed(), 1);
        assert_eq!(counters.failed(), 1);
        assert_eq!(counters.retried(), 1);
        assert_eq!(counters.rejected(AdmissionRejectionReason::AtCapacity), 1);
        assert_eq!(
            counters.rejected(AdmissionRejectionReason::DuplicateIdentity),
            1
        );
        assert_eq!(counters.rejected(AdmissionRejectionReason::ShuttingDown), 1);
        assert_eq!(counters.queue_high_water(), 5);
        assert_eq!(counters.running_high_water(), 2);
        assert_eq!(counters.queued_depth(), 0);
        assert_eq!(counters.running_depth(), 0);
        assert_eq!(counters.shutdown_phase(), ShutdownPhase::Drained);
        assert_admission_counters_reconcile(counters);
    }

    #[test]
    fn running_refresh_supersession_counts_as_one_coalesced_admission() {
        let mut scheduler = scheduler(2, 1, 3);
        let mut older = replaceable_job(1, 10, RefreshClass::ConversationHistory);
        older.retry = RetryPolicy::fixed(2, 250).unwrap();
        scheduler.admit(older.clone(), 1_000, None).unwrap();
        let older_run = scheduler.dispatch_next(1_000).unwrap();
        let newer = replaceable_job(2, 10, RefreshClass::ConversationHistory);
        scheduler.admit(newer.clone(), 1_001, None).unwrap();

        assert_eq!(scheduler.counters().coalesced(), 0);
        assert!(matches!(
            scheduler.complete(older_run.run(), JobOutcome::RetryableFailure, 1_010),
            Ok(CompletionOutcome::Superseded { .. })
        ));
        let counters = scheduler.counters();
        assert_eq!(counters.coalesced(), 1);
        assert_eq!(counters.retried(), 0);
        assert_admission_counters_reconcile(counters);

        let newer_run = scheduler.dispatch_next(1_010).unwrap();
        assert_eq!(
            scheduler.complete(newer_run.run(), JobOutcome::Succeeded, 1_010),
            Ok(CompletionOutcome::Completed)
        );
        let counters = scheduler.counters();
        assert_eq!(counters.completed(), 1);
        assert_eq!(counters.queued_depth(), 0);
        assert_eq!(counters.running_depth(), 0);
        assert_admission_counters_reconcile(counters);
    }

    #[test]
    fn shutdown_drains_a_durable_burst_without_silent_discard() {
        const ADMISSION_CAPACITY: usize = 24;
        const RUNNING_CAPACITY: usize = 4;
        let mut scheduler = scheduler(ADMISSION_CAPACITY, RUNNING_CAPACITY, 3);
        for id in 1..=ADMISSION_CAPACITY as u64 {
            let durability = if id % 2 == 0 {
                SyncDurability::DurableAction
            } else {
                SyncDurability::ReadMarker
            };
            scheduler
                .admit(durable_job(id, durability, RetryPolicy::Never), 1_000, None)
                .unwrap();
        }

        let report = scheduler.begin_shutdown(1_000);
        assert_eq!(report.phase(), ShutdownPhase::Draining);
        assert!(report.cancelled().is_empty());
        assert!(report.cancellation_requested().is_empty());
        while !scheduler.is_drained() {
            let mut batch = Vec::new();
            while let Some(dispatched) = scheduler.dispatch_next(1_000) {
                batch.push(dispatched);
                assert!(scheduler.counters().running_depth() <= RUNNING_CAPACITY);
            }
            assert!(!batch.is_empty());
            for dispatched in batch {
                assert_eq!(
                    scheduler.complete(dispatched.run(), JobOutcome::Succeeded, 1_000),
                    Ok(CompletionOutcome::Completed)
                );
            }
        }

        let counters = scheduler.counters();
        assert_eq!(counters.admitted(), ADMISSION_CAPACITY as u64);
        assert_eq!(counters.completed(), ADMISSION_CAPACITY as u64);
        assert_eq!(counters.failed(), 0);
        assert_eq!(counters.coalesced(), 0);
        assert_eq!(counters.cancellation_requested(), 0);
        assert_eq!(counters.cancellation_completed(), 0);
        assert_eq!(counters.queued_depth(), 0);
        assert_eq!(counters.running_depth(), 0);
        assert_eq!(counters.queue_high_water(), ADMISSION_CAPACITY);
        assert_eq!(counters.running_high_water(), RUNNING_CAPACITY);
        assert_eq!(counters.shutdown_phase(), ShutdownPhase::Drained);
        assert_admission_counters_reconcile(counters);
    }

    #[test]
    fn invalid_and_idempotent_operations_are_counter_neutral() {
        let mut scheduler = scheduler(1, 1, 3);
        let job = ephemeral_job(1);
        scheduler.admit(job.clone(), 1_000, None).unwrap();
        let running = scheduler.dispatch_next(1_000).unwrap();

        let before_invalid = scheduler.counters();
        assert!(scheduler.dispatch_next(1_000).is_none());
        assert_eq!(
            scheduler.cancel(CancellationId::new(99_999), 1_000),
            CancellationOutcome::NotFound
        );
        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Cancelled, 1_000),
            Err(CompletionError::CancellationNotRequested)
        );
        let run = running.run();
        assert_eq!(
            scheduler.complete(
                JobRun {
                    generation: run.generation + 1,
                    ..run
                },
                JobOutcome::Succeeded,
                1_000
            ),
            Err(CompletionError::StaleRun)
        );
        assert_eq!(
            scheduler.complete(
                JobRun {
                    attempt: run.attempt + 1,
                    ..run
                },
                JobOutcome::Succeeded,
                1_000
            ),
            Err(CompletionError::StaleAttempt)
        );
        assert_eq!(
            scheduler.complete(
                JobRun {
                    job_id: SyncJobId::new(99_999),
                    ..run
                },
                JobOutcome::Succeeded,
                1_000
            ),
            Err(CompletionError::UnknownJob)
        );
        assert_eq!(scheduler.counters(), before_invalid);

        assert!(matches!(
            scheduler.cancel(job.cancellation_id(), 1_000),
            CancellationOutcome::Requested(_)
        ));
        let after_request = scheduler.counters();
        assert_eq!(after_request.cancellation_requested(), 1);
        assert!(matches!(
            scheduler.cancel(job.cancellation_id(), 1_000),
            CancellationOutcome::AlreadyRequested(_)
        ));
        assert_eq!(scheduler.counters(), after_request);
        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Succeeded, 1_000),
            Ok(CompletionOutcome::Completed)
        );
        let counters = scheduler.counters();
        assert_eq!(counters.completed(), 1);
        assert_eq!(counters.cancellation_requested(), 1);
        assert_eq!(counters.cancellation_completed(), 0);
        assert_admission_counters_reconcile(counters);

        let mut durable_scheduler = SyncScheduler::new(SchedulerConfig::new(1, 1, 3).unwrap());
        let durable = durable_job(2, SyncDurability::ReadMarker, RetryPolicy::Never);
        durable_scheduler
            .admit(durable.clone(), 1_000, None)
            .unwrap();
        let queued = durable_scheduler.counters();
        assert_eq!(
            durable_scheduler.cancel(durable.cancellation_id(), 1_000),
            CancellationOutcome::Protected {
                job_id: durable.id()
            }
        );
        assert_eq!(durable_scheduler.counters(), queued);
        let durable_run = durable_scheduler.dispatch_next(1_000).unwrap();
        let running = durable_scheduler.counters();
        assert_eq!(
            durable_scheduler.cancel(durable.cancellation_id(), 1_000),
            CancellationOutcome::Protected {
                job_id: durable.id()
            }
        );
        assert_eq!(
            durable_scheduler.complete(durable_run.run(), JobOutcome::Cancelled, 1_000),
            Err(CompletionError::DurableCancellationForbidden)
        );
        assert_eq!(durable_scheduler.counters(), running);
        assert_eq!(
            durable_scheduler.complete(durable_run.run(), JobOutcome::Succeeded, 1_000),
            Ok(CompletionOutcome::Completed)
        );
        assert_admission_counters_reconcile(durable_scheduler.counters());
    }

    #[test]
    fn rejection_counters_follow_scheduler_precedence() {
        let mut scheduler = scheduler(1, 1, 3);
        let job = ephemeral_job(1);
        scheduler.admit(job.clone(), 1_000, None).unwrap();
        let running = scheduler.dispatch_next(1_000).unwrap();

        let mut duplicate_fresh = job.clone();
        duplicate_fresh.freshness = FreshnessPolicy::IfOlderThan { max_age_ms: 500 };
        assert_eq!(
            scheduler
                .admit(duplicate_fresh, 1_000, Some(900))
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::DuplicateIdentity
        );
        let mut distinct_fresh = ephemeral_job(2);
        distinct_fresh.freshness = FreshnessPolicy::IfOlderThan { max_age_ms: 500 };
        assert!(matches!(
            scheduler.admit(distinct_fresh, 1_000, Some(900)),
            Ok(AdmissionOutcome::SkippedFresh(_))
        ));
        assert_eq!(
            scheduler
                .admit(ephemeral_job(3), 1_000, None)
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::AtCapacity
        );

        assert_eq!(
            scheduler.begin_shutdown(1_000).phase(),
            ShutdownPhase::Draining
        );
        assert_eq!(
            scheduler
                .admit(job.clone(), 1_000, None)
                .unwrap_err()
                .reason(),
            AdmissionRejectionReason::ShuttingDown
        );
        assert_eq!(
            scheduler.complete(running.run(), JobOutcome::Cancelled, 1_000),
            Ok(CompletionOutcome::Cancelled(ReleasedJob::new(job, 1)))
        );

        let counters = scheduler.counters();
        assert_eq!(counters.admitted(), 1);
        assert_eq!(counters.skipped_fresh(), 1);
        assert_eq!(
            counters.rejected(AdmissionRejectionReason::DuplicateIdentity),
            1
        );
        assert_eq!(counters.rejected(AdmissionRejectionReason::AtCapacity), 1);
        assert_eq!(counters.rejected(AdmissionRejectionReason::ShuttingDown), 1);
        assert_eq!(counters.cancellation_requested(), 1);
        assert_eq!(counters.cancellation_completed(), 1);
        assert_eq!(counters.shutdown_phase(), ShutdownPhase::Drained);
        assert_admission_counters_reconcile(counters);
    }

    #[test]
    fn full_capacity_replacement_burst_never_spikes_queue_depth() {
        const CAPACITY: usize = 8;
        const RUNNING_CAPACITY: usize = 2;
        const REPLACEMENTS: u64 = 64;
        let mut scheduler = scheduler(CAPACITY, RUNNING_CAPACITY, 3);
        for slot in 0..CAPACITY {
            scheduler
                .admit(
                    replaceable_job(
                        slot as u64 + 1,
                        slot as u64 + 10_000,
                        RefreshClass::ConversationHistory,
                    ),
                    1_000,
                    None,
                )
                .unwrap();
        }

        for replacement in 0..REPLACEMENTS {
            let slot = replacement as usize % CAPACITY;
            assert!(matches!(
                scheduler.admit(
                    replaceable_job(
                        replacement + 1_000,
                        slot as u64 + 10_000,
                        RefreshClass::ConversationHistory,
                    ),
                    1_001 + replacement,
                    None,
                ),
                Ok(AdmissionOutcome::Accepted {
                    coalesced: Some(_),
                    ..
                })
            ));
            let counters = scheduler.counters();
            assert_eq!(counters.queued_depth(), CAPACITY);
            assert_eq!(counters.running_depth(), 0);
            assert_eq!(counters.queue_high_water(), CAPACITY);
            assert_admission_counters_reconcile(counters);
        }

        loop {
            let mut batch = Vec::new();
            while let Some(dispatched) = scheduler.dispatch_next(u64::MAX) {
                batch.push(dispatched);
            }
            if batch.is_empty() {
                break;
            }
            for dispatched in batch {
                assert_eq!(
                    scheduler.complete(dispatched.run(), JobOutcome::Succeeded, u64::MAX),
                    Ok(CompletionOutcome::Completed)
                );
            }
        }
        let counters = scheduler.counters();
        assert_eq!(counters.admitted(), CAPACITY as u64 + REPLACEMENTS);
        assert_eq!(counters.coalesced(), REPLACEMENTS);
        assert_eq!(counters.completed(), CAPACITY as u64);
        assert_eq!(counters.queue_high_water(), CAPACITY);
        assert_eq!(counters.running_high_water(), RUNNING_CAPACITY);
        assert_admission_counters_reconcile(counters);
    }

    #[test]
    fn retry_then_replacement_counts_retry_and_coalescing_once_each() {
        let mut scheduler = scheduler(1, 1, 3);
        let mut older = replaceable_job(1, 10, RefreshClass::ConversationHistory);
        older.retry = RetryPolicy::fixed(2, 250).unwrap();
        scheduler.admit(older, 1_000, None).unwrap();
        let first = scheduler.dispatch_next(1_000).unwrap();
        assert!(matches!(
            scheduler.complete(first.run(), JobOutcome::RetryableFailure, 1_000),
            Ok(CompletionOutcome::Retried { .. })
        ));
        let counters = scheduler.counters();
        assert_eq!(counters.retried(), 1);
        assert_eq!(counters.coalesced(), 0);
        assert_admission_counters_reconcile(counters);

        let newer = replaceable_job(2, 10, RefreshClass::ConversationHistory);
        assert!(matches!(
            scheduler.admit(newer, 1_001, None),
            Ok(AdmissionOutcome::Accepted {
                coalesced: Some(_),
                ..
            })
        ));
        let counters = scheduler.counters();
        assert_eq!(counters.retried(), 1);
        assert_eq!(counters.coalesced(), 1);
        assert_eq!(counters.queued_depth(), 1);
        assert_admission_counters_reconcile(counters);

        let replacement = scheduler.dispatch_next(1_001).unwrap();
        assert_eq!(
            scheduler.complete(replacement.run(), JobOutcome::Succeeded, 1_001),
            Ok(CompletionOutcome::Completed)
        );
        let counters = scheduler.counters();
        assert_eq!(counters.completed(), 1);
        assert_eq!(counters.retried(), 1);
        assert_eq!(counters.coalesced(), 1);
        assert_admission_counters_reconcile(counters);
    }
}
