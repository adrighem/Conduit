# ISSUE:13 — Focused controllers and services

- Status: open; supporting extraction work, revalidated at `80887f8`
- Confidence: high
- Impact: P1 conflict and ownership risk in `window.rs` and `runtime.rs`
- Intent: leave the window as GTK composition and the runtime as supervision/routing while pure typed controllers and services own behavior
- Relationship: not a duplicate of ISSUE:11 or ISSUE:12, but extractions should happen inside those vertical migrations rather than as a standalone mega-refactor
- Risks: line-count-driven extraction can add indirection without establishing authoritative ownership
- Current evidence: `window.rs` and `runtime.rs` have grown to roughly 12,900 and 9,600 lines; useful service seams exist, but both files still own broad dispatch and mixed domain/UI state
- Next step: extract only when a coordinator or presentation slice provides a tested seam, delete the replaced fields and handlers in the same slice, and document dependency direction as ownership moves
- Public action: none taken
