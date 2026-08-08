# ISSUE:12 — Bounded runtime and incremental presentation

- Status: open; Phase 6 manually verified and archived; borrowed catalog presentation reads completed in `130266c`
- Confidence: high
- Impact: P1 transport admission and remaining whole-sidebar source work can still amplify bursts and workspace size
- Intent: bound prioritized work before spawning, coalesce only replaceable synchronization, and update presentation through keyed projections
- Relationship: spans workspace-pipeline Phases 3 and 4 and must consume authoritative coordinator patches from ISSUE:11
- Risks: backpressure must never silently drop durable user actions or read markers; selection and shutdown behavior must survive incremental updates
- Current evidence: Phase 6 removed broad workspace snapshots and compatibility events; sidebar GTK mutations are keyed and virtualized; ordinary presentation scans now borrow the conversation catalog without deep-cloning and sorting every conversation; patch application distinguishes full resets from sorted, deduplicated changed IDs; 985 default tests pass with 3 ignored, strict Clippy passes, and all 17 serial Meson suites pass at `130266c`
- Next step: use changed IDs for safe targeted sidebar source-row updates with full fallbacks, then design bounded per-lane runtime transport admission without dropping durable commands or blocking GTK
- Public action: implementation commits and Conductor notes pushed; no issue comments, labels, or closures
