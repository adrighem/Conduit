# ISSUE:12 — Bounded runtime and incremental presentation

- Status: open; scheduler admission, incremental sidebar, and TimelinePresenter contract complete through `9a37fad`
- Confidence: high
- Impact: P1 unbounded admission, whole-catalog cloning, broad snapshots, and full sidebar rebuilds amplify workspace size and bursts
- Intent: bound prioritized work before spawning, coalesce only replaceable synchronization, and update presentation through keyed projections
- Relationship: spans workspace-pipeline Phases 3 and 4 and must consume authoritative coordinator patches from ISSUE:11
- Risks: backpressure must never silently drop durable user actions or read markers; selection and shutdown behavior must survive incremental updates
- Current evidence: non-navigation jobs receive unique cancellation identities, so startup can enqueue live refresh and emit `SyncCompleted`; the sidebar uses a virtualized `GtkListView` backed by projection-driven `gio::ListStore` operations; `TimelinePresenter` tests cover loading queues, revision mismatch, one-frame patch batching, prepend anchoring, delayed media, corruption fallback, and user-scroll cancellation; 886 default tests pass with 3 ignored
- Next step: route one batched `TimelineDelta` per frame and limit full document loads to initial navigation, revision mismatch, or unrecoverable corruption
- Public action: implementation commits and Conductor notes pushed; no issue comments, labels, or closures
