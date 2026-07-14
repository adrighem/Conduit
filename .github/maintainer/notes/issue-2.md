# ISSUE:2 — Download and open unsupported attachments

- Status: core behavior shipped in `8b3965d`; keep open for cache lifecycle hardening
- Confidence: high in secure download/open behavior; high that unique downloads can accumulate
- Implemented: internal action URL, authenticated Slack-only HTTPS download, atomic bounded writes, safe deterministic filenames, local default-app opening, progress/error feedback
- Remaining acceptance gap: deterministic reuse prevents duplicates, but the attachment cache has no TTL, total-size bound, or eviction policy
- Recommended next step: add age and total-size cleanup, plus byte-safe filename truncation
- Public action: none taken
