# ISSUE:12 — Bounded runtime and incremental presentation

- Status: open; scheduler integration and pure sidebar projection complete through `5cb5167`
- Confidence: high
- Impact: P1 unbounded admission, whole-catalog cloning, broad snapshots, and full sidebar rebuilds amplify workspace size and bursts
- Intent: bound prioritized work before spawning, coalesce only replaceable synchronization, and update presentation through keyed projections
- Relationship: spans workspace-pipeline Phases 3 and 4 and must consume authoritative coordinator patches from ISSUE:11
- Risks: backpressure must never silently drop durable user actions or read markers; selection and shutdown behavior must survive incremental updates
- Current evidence: non-navigation jobs now receive unique cancellation identities, so startup can enqueue its live refresh and emit `SyncCompleted`; `SidebarProjection` emits reset, local splice, and content-update operations, with 1,430-row tests proving one changed row produces one update and one insertion produces one local splice; 883 default tests pass
- Next step: migrate the sidebar from `GtkListBox` to `GtkListView`, `gio::ListStore`, and stable single selection using the projection contract
- Public action: none taken
