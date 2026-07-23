use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::attention::{AttentionDecision, AttentionReason, DeliveryState};
use crate::workspace_pipeline::MutationOrigin;

const ORIGIN_COUNT: usize = 4;
const DELIVERY_COUNT: usize = 5;
const PERSISTENCE_OUTCOME_COUNT: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttentionPersistenceOutcome {
    NotApplicable,
    Accepted,
    AlreadyObserved,
    AtOrBeforeReadCursor,
    Failed,
}

impl AttentionPersistenceOutcome {
    const fn code(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Accepted => "accepted",
            Self::AlreadyObserved => "already_observed",
            Self::AtOrBeforeReadCursor => "at_or_before_read_cursor",
            Self::Failed => "failed",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::NotApplicable => 0,
            Self::Accepted => 1,
            Self::AlreadyObserved => 2,
            Self::AtOrBeforeReadCursor => 3,
            Self::Failed => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttentionMetricsSnapshot {
    pub(crate) committed_decisions: u64,
    pub(crate) unread_decisions: u64,
    pub(crate) notification_candidates: u64,
    reason_counts: [u64; AttentionReason::COUNT],
    origin_counts: [u64; ORIGIN_COUNT],
    delivery_counts: [u64; DELIVERY_COUNT],
    persistence_counts: [u64; PERSISTENCE_OUTCOME_COUNT],
    pub(crate) notification_claims: u64,
    pub(crate) queue_enqueued: u64,
    pub(crate) queue_dequeued: u64,
    pub(crate) queue_depth: u64,
    pub(crate) queue_peak_depth: u64,
    pub(crate) queue_rejected: u64,
}

#[cfg(test)]
impl AttentionMetricsSnapshot {
    pub(crate) fn reason_count(&self, reason: AttentionReason) -> u64 {
        self.reason_counts[reason.index()]
    }

    pub(crate) fn origin_count(&self, origin: MutationOrigin) -> u64 {
        self.origin_counts[origin_index(origin)]
    }

    pub(crate) fn delivery_count(&self, delivery: DeliveryState) -> u64 {
        self.delivery_counts[delivery_index(delivery)]
    }

    pub(crate) fn persistence_count(&self, outcome: AttentionPersistenceOutcome) -> u64 {
        self.persistence_counts[outcome.index()]
    }

    pub(crate) fn delta_since(&self, baseline: &Self) -> Self {
        Self {
            committed_decisions: self
                .committed_decisions
                .saturating_sub(baseline.committed_decisions),
            unread_decisions: self
                .unread_decisions
                .saturating_sub(baseline.unread_decisions),
            notification_candidates: self
                .notification_candidates
                .saturating_sub(baseline.notification_candidates),
            reason_counts: std::array::from_fn(|index| {
                self.reason_counts[index].saturating_sub(baseline.reason_counts[index])
            }),
            origin_counts: std::array::from_fn(|index| {
                self.origin_counts[index].saturating_sub(baseline.origin_counts[index])
            }),
            delivery_counts: std::array::from_fn(|index| {
                self.delivery_counts[index].saturating_sub(baseline.delivery_counts[index])
            }),
            persistence_counts: std::array::from_fn(|index| {
                self.persistence_counts[index].saturating_sub(baseline.persistence_counts[index])
            }),
            notification_claims: self
                .notification_claims
                .saturating_sub(baseline.notification_claims),
            queue_enqueued: self.queue_enqueued.saturating_sub(baseline.queue_enqueued),
            queue_dequeued: self.queue_dequeued.saturating_sub(baseline.queue_dequeued),
            queue_depth: self.queue_depth,
            queue_peak_depth: self.queue_peak_depth,
            queue_rejected: self.queue_rejected.saturating_sub(baseline.queue_rejected),
        }
    }
}

#[derive(Debug)]
pub(crate) struct AttentionMetrics {
    committed_decisions: AtomicU64,
    unread_decisions: AtomicU64,
    notification_candidates: AtomicU64,
    reason_counts: [AtomicU64; AttentionReason::COUNT],
    origin_counts: [AtomicU64; ORIGIN_COUNT],
    delivery_counts: [AtomicU64; DELIVERY_COUNT],
    persistence_counts: [AtomicU64; PERSISTENCE_OUTCOME_COUNT],
    notification_claims: AtomicU64,
    queue: Mutex<AttentionQueueMetrics>,
}

#[derive(Debug, Default)]
struct AttentionQueueMetrics {
    enqueued: u64,
    dequeued: u64,
    depth: u64,
    peak_depth: u64,
    rejected: u64,
}

impl Default for AttentionMetrics {
    fn default() -> Self {
        Self {
            committed_decisions: AtomicU64::new(0),
            unread_decisions: AtomicU64::new(0),
            notification_candidates: AtomicU64::new(0),
            reason_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            origin_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            delivery_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            persistence_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            notification_claims: AtomicU64::new(0),
            queue: Mutex::new(AttentionQueueMetrics::default()),
        }
    }
}

impl AttentionMetrics {
    pub(crate) fn record_decision(
        &self,
        revision: u64,
        origin: MutationOrigin,
        delivery: DeliveryState,
        decision: &AttentionDecision,
    ) {
        self.committed_decisions.fetch_add(1, Ordering::Relaxed);
        self.unread_decisions
            .fetch_add(u64::from(decision.record_unread), Ordering::Relaxed);
        self.notification_candidates
            .fetch_add(u64::from(decision.send_notification), Ordering::Relaxed);
        self.origin_counts[origin_index(origin)].fetch_add(1, Ordering::Relaxed);
        self.delivery_counts[delivery_index(delivery)].fetch_add(1, Ordering::Relaxed);
        for reason in decision.reasons.iter().copied() {
            self.reason_counts[reason.index()].fetch_add(1, Ordering::Relaxed);
        }

        tracing::trace!(
            target: "conduit::attention",
            parent: None,
            event = "attention_decision",
            revision,
            origin = origin_code(origin),
            delivery = delivery_code(delivery),
            record_unread = decision.record_unread,
            notification_candidate = decision.send_notification,
            reasons = ?AttentionReasonCodes(&decision.reasons),
        );
    }

    pub(crate) fn record_persistence(
        &self,
        outcome: AttentionPersistenceOutcome,
        notification_claimed: bool,
    ) {
        self.persistence_counts[outcome.index()].fetch_add(1, Ordering::Relaxed);
        self.notification_claims
            .fetch_add(u64::from(notification_claimed), Ordering::Relaxed);
        tracing::trace!(
            target: "conduit::attention",
            parent: None,
            event = "attention_persistence",
            outcome = outcome.code(),
            notification_claimed,
        );
    }

    pub(crate) fn record_queue_send<T, E>(
        &self,
        send: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let mut queue = self
            .queue
            .lock()
            .expect("attention queue metrics lock poisoned");
        let result = send();
        let high_water = match &result {
            Ok(_) => {
                queue.enqueued = queue.enqueued.saturating_add(1);
                queue.depth = queue.depth.saturating_add(1);
                let previous_peak = queue.peak_depth;
                queue.peak_depth = queue.peak_depth.max(queue.depth);
                (queue.peak_depth > previous_peak
                    && (queue.peak_depth == 1 || queue.peak_depth.is_power_of_two()))
                .then_some(queue.peak_depth)
            }
            Err(_) => {
                queue.rejected = queue.rejected.saturating_add(1);
                None
            }
        };
        drop(queue);
        if let Some(depth) = high_water {
            tracing::trace!(
                target: "conduit::attention",
                parent: None,
                event = "attention_queue_high_water",
                queue_depth = depth,
                queue_peak_depth = depth,
            );
        }
        result
    }

    pub(crate) fn dequeue_queue_slot(&self) {
        let mut queue = self
            .queue
            .lock()
            .expect("attention queue metrics lock poisoned");
        debug_assert!(queue.depth > 0, "attention queue depth underflow");
        queue.depth = queue.depth.saturating_sub(1);
        queue.dequeued = queue.dequeued.saturating_add(1);
    }

    pub(crate) fn snapshot(&self) -> AttentionMetricsSnapshot {
        let queue = self
            .queue
            .lock()
            .expect("attention queue metrics lock poisoned");
        AttentionMetricsSnapshot {
            committed_decisions: self.committed_decisions.load(Ordering::Relaxed),
            unread_decisions: self.unread_decisions.load(Ordering::Relaxed),
            notification_candidates: self.notification_candidates.load(Ordering::Relaxed),
            reason_counts: std::array::from_fn(|index| {
                self.reason_counts[index].load(Ordering::Relaxed)
            }),
            origin_counts: std::array::from_fn(|index| {
                self.origin_counts[index].load(Ordering::Relaxed)
            }),
            delivery_counts: std::array::from_fn(|index| {
                self.delivery_counts[index].load(Ordering::Relaxed)
            }),
            persistence_counts: std::array::from_fn(|index| {
                self.persistence_counts[index].load(Ordering::Relaxed)
            }),
            notification_claims: self.notification_claims.load(Ordering::Relaxed),
            queue_enqueued: queue.enqueued,
            queue_dequeued: queue.dequeued,
            queue_depth: queue.depth,
            queue_peak_depth: queue.peak_depth,
            queue_rejected: queue.rejected,
        }
    }

    pub(crate) fn trace_snapshot(&self) {
        let snapshot = self.snapshot();
        tracing::trace!(
            target: "conduit::attention",
            parent: None,
            event = "attention_metrics_snapshot",
            committed_decisions = snapshot.committed_decisions,
            unread_decisions = snapshot.unread_decisions,
            notification_candidates = snapshot.notification_candidates,
            notification_claims = snapshot.notification_claims,
            reasons = ?AttentionReasonCounts(&snapshot.reason_counts),
            ledger_not_applicable = snapshot.persistence_counts
                [AttentionPersistenceOutcome::NotApplicable.index()],
            ledger_accepted = snapshot.persistence_counts
                [AttentionPersistenceOutcome::Accepted.index()],
            ledger_already_observed = snapshot.persistence_counts
                [AttentionPersistenceOutcome::AlreadyObserved.index()],
            ledger_at_or_before_read_cursor = snapshot.persistence_counts
                [AttentionPersistenceOutcome::AtOrBeforeReadCursor.index()],
            ledger_failed = snapshot.persistence_counts
                [AttentionPersistenceOutcome::Failed.index()],
            queue_enqueued = snapshot.queue_enqueued,
            queue_dequeued = snapshot.queue_dequeued,
            queue_depth = snapshot.queue_depth,
            queue_peak_depth = snapshot.queue_peak_depth,
            queue_rejected = snapshot.queue_rejected,
        );
    }
}

struct AttentionReasonCodes<'a>(&'a [AttentionReason]);

impl fmt::Debug for AttentionReasonCodes<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = formatter.debug_list();
        for reason in self.0 {
            list.entry(&reason.code());
        }
        list.finish()
    }
}

struct AttentionReasonCounts<'a>(&'a [u64; AttentionReason::COUNT]);

impl fmt::Debug for AttentionReasonCounts<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = formatter.debug_map();
        for reason in AttentionReason::ALL {
            map.entry(&reason.code(), &self.0[reason.index()]);
        }
        map.finish()
    }
}

const fn origin_index(origin: MutationOrigin) -> usize {
    match origin {
        MutationOrigin::Cache => 0,
        MutationOrigin::WebApi => 1,
        MutationOrigin::Local => 2,
        MutationOrigin::Realtime => 3,
    }
}

const fn origin_code(origin: MutationOrigin) -> &'static str {
    match origin {
        MutationOrigin::Cache => "cache",
        MutationOrigin::WebApi => "web_api",
        MutationOrigin::Local => "local",
        MutationOrigin::Realtime => "realtime",
    }
}

const fn delivery_index(delivery: DeliveryState) -> usize {
    match delivery {
        DeliveryState::Fresh => 0,
        DeliveryState::Reconciled => 1,
        DeliveryState::Historical => 2,
        DeliveryState::Stale => 3,
        DeliveryState::Duplicate => 4,
    }
}

const fn delivery_code(delivery: DeliveryState) -> &'static str {
    match delivery {
        DeliveryState::Fresh => "fresh",
        DeliveryState::Reconciled => "reconciled",
        DeliveryState::Historical => "historical",
        DeliveryState::Stale => "stale",
        DeliveryState::Duplicate => "duplicate",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_count_each_committed_decision_and_multi_trigger_reason_once() {
        let metrics = AttentionMetrics::default();
        let decision = AttentionDecision {
            record_unread: true,
            send_notification: true,
            reasons: vec![
                AttentionReason::DirectMessage,
                AttentionReason::KeywordOrPhrase,
            ],
        };

        metrics.record_decision(7, MutationOrigin::Realtime, DeliveryState::Fresh, &decision);
        metrics.record_persistence(AttentionPersistenceOutcome::Accepted, true);
        let snapshot = metrics.snapshot();

        assert_eq!(snapshot.committed_decisions, 1);
        assert_eq!(snapshot.unread_decisions, 1);
        assert_eq!(snapshot.notification_candidates, 1);
        assert_eq!(snapshot.reason_count(AttentionReason::DirectMessage), 1);
        assert_eq!(snapshot.reason_count(AttentionReason::KeywordOrPhrase), 1);
        assert_eq!(snapshot.origin_count(MutationOrigin::Realtime), 1);
        assert_eq!(snapshot.delivery_count(DeliveryState::Fresh), 1);
        assert_eq!(
            snapshot.persistence_count(AttentionPersistenceOutcome::Accepted),
            1
        );
        assert_eq!(snapshot.notification_claims, 1);
    }

    #[test]
    fn queue_high_water_and_snapshot_delta_are_stable_after_drain() {
        let metrics = AttentionMetrics::default();
        let baseline = metrics.snapshot();
        metrics.record_queue_send(|| Ok::<_, ()>(())).unwrap();
        metrics.record_queue_send(|| Ok::<_, ()>(())).unwrap();
        metrics.dequeue_queue_slot();
        metrics.dequeue_queue_slot();
        metrics.record_persistence(AttentionPersistenceOutcome::AlreadyObserved, false);

        let delta = metrics.snapshot().delta_since(&baseline);

        assert_eq!(delta.queue_enqueued, 2);
        assert_eq!(delta.queue_dequeued, 2);
        assert_eq!(delta.queue_depth, 0);
        assert_eq!(delta.queue_peak_depth, 2);
        assert_eq!(
            delta.persistence_count(AttentionPersistenceOutcome::AlreadyObserved),
            1
        );
    }

    #[test]
    fn rejected_queue_send_does_not_inflate_depth_or_high_water() {
        let metrics = AttentionMetrics::default();

        assert!(metrics
            .record_queue_send(|| Err::<(), _>("worker closed"))
            .is_err());
        let snapshot = metrics.snapshot();

        assert_eq!(snapshot.queue_enqueued, 0);
        assert_eq!(snapshot.queue_dequeued, 0);
        assert_eq!(snapshot.queue_depth, 0);
        assert_eq!(snapshot.queue_peak_depth, 0);
        assert_eq!(snapshot.queue_rejected, 1);
    }
}
