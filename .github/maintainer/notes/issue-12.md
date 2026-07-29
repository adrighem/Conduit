# ISSUE:12 — Bounded runtime and incremental presentation

- Status: open; follow ISSUE:11, revalidated at `80887f8`
- Confidence: high
- Impact: P1 unbounded admission, whole-catalog cloning, broad snapshots, and full sidebar rebuilds amplify workspace size and bursts
- Intent: bound prioritized work before spawning, coalesce only replaceable synchronization, and update presentation through keyed projections
- Relationship: spans workspace-pipeline Phases 3 and 4 and must consume authoritative coordinator patches from ISSUE:11
- Risks: backpressure must never silently drop durable user actions or read markers; selection and shutdown behavior must survive incremental updates
- Current evidence: command/event channels remain unbounded, connected work is spawned before lane capacity is acquired, broad snapshots and full catalog clones remain, and structural sidebar changes still rebuild a `GtkListBox`
- Next step: after the next ISSUE:11 slice, acquire bounded capacity before spawning with explicit durable versus replaceable semantics, then migrate sidebar presentation to incremental model operations
- Public action: none taken
