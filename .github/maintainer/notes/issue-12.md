# ISSUE:12 — Bounded runtime and incremental presentation

- Status: open; scheduler admission, incremental sidebar, and batched TimelinePresenter delivery complete through `b1f136a`
- Confidence: high
- Impact: P1 unbounded admission, whole-catalog cloning, broad snapshots, and full sidebar rebuilds amplify workspace size and bursts
- Intent: bound prioritized work before spawning, coalesce only replaceable synchronization, and update presentation through keyed projections
- Relationship: spans workspace-pipeline Phases 3 and 4 and must consume authoritative coordinator patches from ISSUE:11
- Risks: backpressure must never silently drop durable user actions or read markers; selection and shutdown behavior must survive incremental updates
- Current evidence: non-navigation jobs receive unique cancellation identities, so startup can enqueue live refresh and emit `SyncCompleted`; the sidebar uses a virtualized `GtkListView` backed by projection-driven `gio::ListStore` operations; each main/thread WebView queues revision-aware patches during loading and emits one `TimelineDelta` per GTK frame; empty timelines remain patchable; full timeline documents load only for navigation or revision/corruption recovery; 889 default tests pass with 3 ignored and all 17 Meson suites pass
- Next step: add the MIME-checked `conduit-asset://` cache-key scheme and remove nested root resize observers
- Public action: implementation commits and Conductor notes pushed; no issue comments, labels, or closures
