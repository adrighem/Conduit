# Bounded Synchronization Scheduler

## Summary

Define the scheduler foundation for issue #12 so runtime work has explicit admission capacity,
interactive priority, target-aware replacement rules, cancellation, freshness, and observable
queue behavior before it is integrated with the authoritative workspace coordinator.

## Requirements

1. `SyncJob` identifies a stable target key, priority, freshness policy, replacement class, and
   cancellation identity without containing credentials or private message content.
2. Capacity is reserved before asynchronous work is spawned.
3. Durable user actions and read markers are never silently dropped, coalesced, or cancelled.
4. Explicitly replaceable refresh work coalesces by target and cancels stale queued generations.
5. Interactive navigation and user work take precedence over maintenance without permanently
   starving accepted maintenance work.
6. Retry and freshness decisions are deterministic and independently testable.
7. Shutdown rejects new admission and drains or cancels accepted work according to its durability
   contract.
8. Counters expose admitted, queued, coalesced, cancelled, completed, rejected, and high-water
   values without sensitive fields.

## Acceptance Criteria

- Deterministic tests cover capacity, priority, fairness, coalescing, cancellation, freshness,
  retry, saturation, and shutdown.
- Tests prove that durable work is never silently discarded.
- A burst cannot create more waiting tasks than the configured admitted capacity.
- Replaceable work for one target cannot cancel work for another target.
- Metrics reconcile to a zero queue depth after drain.
- The module is independent of GTK and WebKitGTK.
- Runtime integration remains blocked until the issue #11 conversation-authority slice is merged.

## Out of Scope

- Wiring every existing runtime command through the scheduler.
- Replacing the sidebar or broad workspace snapshots.
- Changing Slack API retry semantics outside the scheduler contract.
- Introducing a general-purpose job framework.
