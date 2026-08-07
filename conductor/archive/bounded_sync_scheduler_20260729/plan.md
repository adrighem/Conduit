# Bounded Synchronization Scheduler Plan

## Phase 1: Scheduler contracts and deterministic core [checkpoint: 6590ad0]

- [x] Task: Add failing tests for SyncJob identity, durability, priority, freshness, and replacement a5b6742
- [x] Task: Implement bounded pre-spawn admission with priority and starvation protection cded7d7
- [x] Task: Add target-aware coalescing, cancellation, retry, and shutdown behavior 841d550
- [x] Task: Add redacted queue counters and saturation regression coverage 147f934
- [x] Task: Conductor - User Manual Verification 'Scheduler contracts and deterministic core' (Protocol in workflow.md) 6590ad0

## Phase 2: Authoritative runtime integration [checkpoint: c7c5f46]

- [x] Task: Integrate the scheduler after the issue #11 conversation-authority slice e056d48
- [x] Task: Move startup, navigation, refresh, and membership scheduling onto bounded admission 6590ad0
- [x] Task: Enforce exact startup enrichment, history, and user-directory limits 6590ad0
- [x] Task: Validate supersession, durable actions, read markers, retries, and clean shutdown 0e3939f
- [x] Task: Fix startup follow-up admission regression 91303bb
- [x] Task: Conductor - User Manual Verification 'Authoritative runtime scheduler integration' (Protocol in workflow.md) c7c5f46
