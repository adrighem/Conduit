# ISSUE:2 — Download and open unsupported attachments

- Status: remaining cache lifecycle work implemented locally; closure-ready after remote CI
- Confidence: high
- Implemented: internal action URL, authenticated Slack-only HTTPS download, atomic bounded writes, byte-safe deterministic filenames, local default-app opening, progress/error feedback, 30-day expiry, and 1 GiB oldest-first eviction
- Validation: focused expiry, size-cap, protected-download, and UTF-8 filename tests plus the full Rust/Meson suites
- Public action: none taken
