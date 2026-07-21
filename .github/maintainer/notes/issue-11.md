# ISSUE:11 — WorkspaceCoordinator authority

- Status: open; highest-leverage architecture dependency
- Confidence: high
- Impact: P0 duplication remains because the coordinator computes patches and store batches while legacy runtime and GTK paths stay authoritative
- Intent: migrate one complete surface at a time so each mutation yields at most one revisioned patch and one atomic store batch
- Relationship: dependency root for ISSUE:12 and the ownership seams used by ISSUE:13
- Risks: leaving legacy and coordinator paths active together prolongs the migration tax and inconsistent merge rules
- Next step: finish Phase 2 verification, then move vertical surfaces onto typed patches and delete each replaced compatibility path immediately
- Public action: none taken
