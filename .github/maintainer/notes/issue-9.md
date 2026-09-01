# ISSUE:9 — Bounded WebKit asset pipeline

- Status: closed; implementation prepared after `9d9019b`, verified and closed on GitHub on 2026-08-31
- Confidence: high
- Impact: P0 memory, copy-amplification, and cache-lifecycle risk from full-size base64 data URIs
- Intent: replace data-URI payloads with workspace-scoped opaque asset keys backed by MIME-validated, byte-bounded memory and disk caches
- Relationship: uses ISSUE:2 as security/cache precedent and belongs in the existing workspace-pipeline Phase 4 asset slice
- Risks: custom-scheme lifecycle, video range handling, offline cache invalidation, and cross-workspace isolation need explicit tests
- Current evidence: preview events now carry scoped descriptors, message HTML accepts only typed opaque asset URIs, and the scheme serves MIME/signature-checked raw files with bounded range responses
- Implemented bounds: 8/16 MiB per preview, 64 MiB/2,048-entry descriptor ready set, bounded request state, and a deterministic 512 MiB/16,384-entry/30-day disk cache
- Next step: issue fully completed and closed
- Public action: implementation commits pushed; closed on GitHub with full summary comment on 2026-08-31
