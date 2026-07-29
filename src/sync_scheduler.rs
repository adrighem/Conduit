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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SyncJobId(u64);

impl SyncJobId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdmissionOutcome {
    Accepted { job_id: SyncJobId },
    SkippedFresh(SyncJob),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionRejectionReason {
    AtCapacity,
    DuplicateIdentity,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct JobRun {
    job_id: SyncJobId,
    attempt: u32,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchedJob {
    job: SyncJob,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompletionOutcome {
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionError {
    UnknownJob,
    StaleRun,
    StaleAttempt,
}

#[derive(Debug, Clone)]
struct QueuedJob {
    job: SyncJob,
    attempt: u32,
    ready_at_ms: u64,
}

#[derive(Debug, Clone)]
struct RunningJob {
    job: SyncJob,
    attempt: u32,
    generation: u64,
}

/// A pure state machine. A caller may spawn work only after `dispatch_next`
/// returns it; at that point both admission and running capacity are reserved.
pub(crate) struct SyncScheduler {
    config: SchedulerConfig,
    interactive: VecDeque<QueuedJob>,
    foreground: VecDeque<QueuedJob>,
    maintenance: VecDeque<QueuedJob>,
    running: HashMap<SyncJobId, RunningJob>,
    next_run_generation: u64,
    foreground_wait_dispatches: u64,
    maintenance_wait_dispatches: u64,
}

impl SyncScheduler {
    pub(crate) fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            interactive: VecDeque::new(),
            foreground: VecDeque::new(),
            maintenance: VecDeque::new(),
            running: HashMap::new(),
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
        if self.has_active_identity(job.id, job.cancellation_id) {
            return Err(AdmissionRejection {
                reason: AdmissionRejectionReason::DuplicateIdentity,
                job,
            });
        }
        if job.freshness.decision(now_ms, last_success_at_ms) == FreshnessDecision::SkipFresh {
            return Ok(AdmissionOutcome::SkippedFresh(job));
        }
        if self.active_len() >= self.config.admission_capacity {
            return Err(AdmissionRejection {
                reason: AdmissionRejectionReason::AtCapacity,
                job,
            });
        }

        let job_id = job.id;
        self.queue_mut(job.priority).push_back(QueuedJob {
            job,
            attempt: 1,
            ready_at_ms: now_ms,
        });
        Ok(AdmissionOutcome::Accepted { job_id })
    }

    pub(crate) fn dispatch_next(&mut self, now_ms: u64) -> Option<DispatchedJob> {
        if self.running.len() >= self.config.running_capacity {
            return None;
        }

        let interactive = Self::eligible_position(&self.interactive, now_ms);
        let foreground = Self::eligible_position(&self.foreground, now_ms);
        let maintenance = Self::eligible_position(&self.maintenance, now_ms);
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
            attempt: queued.attempt,
            generation,
        };
        self.running.insert(
            queued.job.id,
            RunningJob {
                job: queued.job,
                attempt: queued.attempt,
                generation,
            },
        );
        Some(dispatched)
    }

    pub(crate) fn complete(
        &mut self,
        run: JobRun,
        outcome: JobOutcome,
        _now_ms: u64,
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

        match outcome {
            JobOutcome::Succeeded => {
                self.running.remove(&run.job_id);
                Ok(CompletionOutcome::Completed)
            }
        }
    }

    fn active_len(&self) -> usize {
        self.interactive.len() + self.foreground.len() + self.maintenance.len() + self.running.len()
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

    fn eligible_position(queue: &VecDeque<QueuedJob>, now_ms: u64) -> Option<usize> {
        queue.iter().position(|queued| queued.ready_at_ms <= now_ms)
    }

    fn queue_mut(&mut self, priority: SyncPriority) -> &mut VecDeque<QueuedJob> {
        match priority {
            SyncPriority::Interactive => &mut self.interactive,
            SyncPriority::Foreground => &mut self.foreground,
            SyncPriority::Maintenance => &mut self.maintenance,
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
                job_id: SyncJobId(1)
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
}
