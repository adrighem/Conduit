# ISSUE:13 — Focused controllers and services

- Status: open; supporting extraction work
- Confidence: high
- Impact: P1 conflict and ownership risk in `window.rs` and `runtime.rs`
- Intent: leave the window as GTK composition and the runtime as supervision/routing while pure typed controllers and services own behavior
- Relationship: not a duplicate of ISSUE:11 or ISSUE:12, but extractions should happen inside those vertical migrations rather than as a standalone mega-refactor
- Risks: line-count-driven extraction can add indirection without establishing authoritative ownership
- Next step: extract only when a coordinator or presentation slice provides a tested seam, and document dependency direction as ownership moves
- Public action: none taken
