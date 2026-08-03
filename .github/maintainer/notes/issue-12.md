# ISSUE:12 — Bounded runtime and incremental presentation

- Status: open; automated Phase 5 implementation complete through restricted asset delivery in `44f1c0f`
- Confidence: high
- Impact: P1 unbounded admission, whole-catalog cloning, broad snapshots, and full sidebar rebuilds amplify workspace size and bursts
- Intent: bound prioritized work before spawning, coalesce only replaceable synchronization, and update presentation through keyed projections
- Relationship: spans workspace-pipeline Phases 3 and 4 and must consume authoritative coordinator patches from ISSUE:11
- Risks: backpressure must never silently drop durable user actions or read markers; selection and shutdown behavior must survive incremental updates
- Current evidence: non-navigation jobs receive unique cancellation identities, so startup can enqueue live refresh and emit `SyncCompleted`; the sidebar uses a virtualized `GtkListView` backed by projection-driven `gio::ListStore` operations; each main/thread WebView emits one revision-aware `TimelineDelta` per GTK frame; cached previews resolve only through registered SHA-256 `conduit-asset` keys with exact MIME and 8/16 MiB bounds; timeline resize correction uses window resize and targeted media load events rather than a root `ResizeObserver`; 891 default tests pass with 3 ignored and all 17 serial Meson suites pass
- Next step: perform the Phase 5 manual verification protocol before checkpointing incremental GTK and WebKit presentation
- Public action: implementation commits and Conductor notes pushed; no issue comments, labels, or closures
