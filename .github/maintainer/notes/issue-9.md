# ISSUE:9 — Bounded WebKit asset pipeline

- Status: open; planned Phase 4 work
- Confidence: high
- Impact: P0 memory, copy-amplification, and cache-lifecycle risk from full-size base64 data URIs
- Intent: replace data-URI payloads with workspace-scoped opaque asset keys backed by MIME-validated, byte-bounded memory and disk caches
- Relationship: uses ISSUE:2 as security/cache precedent and belongs in the existing workspace-pipeline Phase 4 asset slice
- Risks: custom-scheme lifecycle, video range handling, offline cache invalidation, and cross-workspace isolation need explicit tests
- Next step: establish controlled release-build before/after measurements, then implement within Phase 4 rather than as a competing cache
- Public action: none taken
