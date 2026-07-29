# Bounded Synchronization Scheduler Plan

## Phase 1: Scheduler contracts and deterministic core

- [x] Task: Add failing tests for SyncJob identity, durability, priority, freshness, and replacement a5b6742
- [x] Task: Implement bounded pre-spawn admission with priority and starvation protection cded7d7
- [x] Task: Add target-aware coalescing, cancellation, retry, and shutdown behavior 841d550
- [~] Task: Add redacted queue counters and saturation regression coverage
- [ ] Task: Conductor - User Manual Verification 'Scheduler contracts and deterministic core' (Protocol in workflow.md)

## Phase 2: Authoritative runtime integration

- [ ] Task: Integrate the scheduler after the issue #11 conversation-authority slice
- [ ] Task: Move startup, navigation, refresh, and membership scheduling onto bounded admission
- [ ] Task: Enforce exact startup enrichment, history, and user-directory limits
- [ ] Task: Validate supersession, durable actions, read markers, retries, and clean shutdown
- [ ] Task: Conductor - User Manual Verification 'Authoritative runtime scheduler integration' (Protocol in workflow.md)
