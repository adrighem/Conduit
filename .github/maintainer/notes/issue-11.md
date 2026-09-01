# ISSUE:11 — WorkspaceCoordinator authority

- Status: closed; atomic ordered persistence complete, typed conversation patch delivery completed; closed on GitHub on 2026-08-07
- Confidence: high
- Impact: P0 duplication remains because the coordinator computes patches and store batches while legacy runtime and GTK paths stay authoritative
- Intent: migrate one complete surface at a time so each mutation yields at most one revisioned patch and one atomic store batch
- Relationship: dependency root for ISSUE:12 and the ownership seams used by ISSUE:13
- Risks: compatibility events must not regain catalog authority, and recovered patches must remain session-scoped so request supersession cannot discard them
- Current evidence: every StoreChange has an atomic StoreHub executor; failed reductions survive cancellation and consecutive failures; repair preserves concurrent read overlays; metadata cannot roll back authoritative stars; full sanitized suite passed with 740 tests and final review found no high or medium issues
- Next step: issue fully completed and closed
- Public action: implementation commits pushed; closed on GitHub on 2026-08-07
