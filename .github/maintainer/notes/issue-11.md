# ISSUE:11 — WorkspaceCoordinator authority

- Status: open; partially progressed and still the highest-leverage architecture dependency at `80887f8`
- Confidence: high
- Impact: P0 duplication remains because the coordinator computes patches and store batches while legacy runtime and GTK paths stay authoritative
- Intent: migrate one complete surface at a time so each mutation yields at most one revisioned patch and one atomic store batch
- Relationship: dependency root for ISSUE:12 and the ownership seams used by ISSUE:13
- Risks: leaving legacy and coordinator paths active together prolongs the migration tax and inconsistent merge rules
- Current evidence: the coordinator models revisions, patches, and store batches, but production reductions are mostly logged and discarded while runtime storage, legacy events, GTK catalogs, histories, and thread state remain separately authoritative
- Next step: migrate conversation membership and metadata as one complete vertical slice, execute one store batch, apply one typed patch, and delete the replaced legacy events and GTK mutations
- Public action: none taken
