# ISSUE:12 — Bounded runtime and incremental presentation

- Status: open; Phase 6 manually verified and archived; borrowed catalog reads completed in `130266c`; targeted sidebar source updates completed in `80e672f`
- Confidence: high
- Impact: P1 runtime transport admission can still amplify bursts before work reaches the bounded executors
- Intent: bound prioritized work before spawning, coalesce only replaceable synchronization, and update presentation through keyed projections
- Relationship: spans workspace-pipeline Phases 3 and 4 and must consume authoritative coordinator patches from ISSUE:11
- Risks: backpressure must never silently drop durable user actions or read markers; selection and shutdown behavior must survive incremental updates
- Current evidence: Phase 6 removed broad workspace snapshots and compatibility events; sidebar GTK mutations are keyed and virtualized; ordinary presentation scans borrow the conversation catalog; presentation-only unread patches now build only changed rows and replace indexed visible positions in O(changed rows), while reset, structural, ordering, membership, missing-row, and recent-DM boundary cases fall back to a full rebuild; Unreads refreshes no longer rebuild the sidebar twice; 993 default tests pass with 3 ignored, strict Clippy passes, and all 17 serial Meson suites pass at `80e672f`
- Next step: design bounded per-lane runtime transport admission with explicit durable, coalescible, and supersedable semantics, completion-driven dispatch, and shutdown draining before changing channel capacities
- Public action: implementation commits and Conductor notes pushed; no issue comments, labels, or closures
