# ISSUE:12 — Bounded runtime and incremental presentation

- Status: open; deterministic scheduler foundation complete at `147f934`, manual Phase 1 checkpoint pending
- Confidence: high
- Impact: P1 unbounded admission, whole-catalog cloning, broad snapshots, and full sidebar rebuilds amplify workspace size and bursts
- Intent: bound prioritized work before spawning, coalesce only replaceable synchronization, and update presentation through keyed projections
- Relationship: spans workspace-pipeline Phases 3 and 4 and must consume authoritative coordinator patches from ISSUE:11
- Risks: backpressure must never silently drop durable user actions or read markers; selection and shutdown behavior must survive incremental updates
- Current evidence: the pure scheduler now reserves admission before spawn, bounds running work, protects durable actions and read markers, coalesces exact targets, handles acknowledged cancellation/retry/shutdown, and exposes redacted conserving counters; 50 focused and 782 integrated tests passed
- Next step: obtain explicit manual confirmation of the deterministic foundation, checkpoint Phase 1, and integrate it only after ISSUE:11 conversation patch authority is complete
- Public action: none taken
