/* sync_scheduler.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Pure contracts and deterministic scheduling for bounded synchronization work.

// Runtime integration follows after the issue #11 authority slice.
#![allow(dead_code)]

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
}
