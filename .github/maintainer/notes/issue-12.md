# ISSUE:12 — Bounded runtime and incremental presentation

- Status: open; Phase 6 manually verified and archived; borrowed catalog reads completed in `130266c`; targeted sidebar source updates completed in `80e672f`; realtime persistence admission bounded in `fcf641c`
- Confidence: high
- Impact: P1 command, UI-event, huddle, and native-media transport admission can still amplify bursts; session shutdown can abort accepted persistence work
- Intent: bound prioritized work before spawning, coalesce only replaceable synchronization, and update presentation through keyed projections
- Relationship: spans workspace-pipeline Phases 3 and 4 and must consume authoritative coordinator patches from ISSUE:11
- Risks: backpressure must never silently drop durable user actions or read markers; selection and shutdown behavior must survive incremental updates
- Current evidence: Phase 6 removed broad workspace snapshots and compatibility events; sidebar GTK mutations are keyed and virtualized; ordinary presentation scans borrow the conversation catalog; presentation-only unread patches now build only changed rows and replace indexed visible positions in O(changed rows), while structural and membership cases fall back to a full rebuild; realtime persistence now has an awaited FIFO capacity of 256, keeps queue metrics inside that bound, withholds Socket Mode ACKs on admission timeout or rejection, preserves order, and drains before normal reconnect; the 1,200-event release burst drained in 1.96 seconds with peak 256 and zero rejects; 997 default tests pass with 3 ignored, strict all-target Clippy passes, and all 17 serial Meson suites pass at `fcf641c`
- Next step: add supervisor-owned session shutdown that drains accepted durable work, then classify command admission explicitly as durable, coalescible, or supersedable before bounding command, UI-event, and huddle lanes
- Public action: implementation commits pushed through the normal mainline workflow; no issue comments, labels, or closures
