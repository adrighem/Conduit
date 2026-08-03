# ISSUE:10 — Lazy WebViews and on-demand emoji DOM

- Status: open; bounded picker Phase 1 checkpointed at `cc481cf`; lazy thread WebView Phase 2 checkpointed at `d2292ea`; animated native status emoji Phase 3 implemented through `5dc6101`, manual verification pending
- Confidence: high
- Impact: avoids an unused thread renderer and repeated full emoji catalogs in initial timeline documents
- Intent: create the thread WebView on demand and materialize only bounded picker results while preserving ISSUE:4 and ISSUE:5 behavior
- Relationship: independent enough to measure and ship separately from the coordinator migration
- Risks: teardown may hurt reopen latency; related views trade lower overhead for shared crash/memory scope
- Current evidence: initial document size fell from 748,787 to 58,762 bytes, eager reaction-picker choices fell from 2,126 to 0, and sampled release generation fell from 5,531 to 407 microseconds; the native status chooser reuses the same bounded 64-result search/category protocol without another WebView; release startup process-tree PSS fell from 220,974 to 175,719 KiB (20.5%) and the process count fell from four to three while first thread-open and retained-reopen latency remained flat; the status grid now decodes and schedules animated custom-emoji frames, with a two-frame GIF regression fixture; 891 tests pass with 3 ignored and all 17 Meson suites pass
- Next step: obtain explicit live-workspace confirmation that animated custom emoji move in the Set Status picker while static and Unicode emoji still render normally; then checkpoint Phase 3 and complete the track
- Public action: implementation commits only; no issue comments, labels, or closures
- Exact-head validation: local formatting and compile checks pass; 891 Rust tests pass with 3 ignored and all 17 Meson suites pass. Phase 3 remote validation is pending. Phase 2 passed CI `30817337591`, CodeQL `30817337576`, and guarded release automation `30818103640` after fixing Rust 1.97's `let_and_return` lint from the first run.
