# ISSUE:10 — Lazy WebViews and on-demand emoji DOM

- Status: open; bounded picker Phase 1 checkpointed at `cc481cf`; lazy thread WebView Phase 2 implemented through `9a16326`, manual verification pending
- Confidence: high
- Impact: avoids an unused thread renderer and repeated full emoji catalogs in initial timeline documents
- Intent: create the thread WebView on demand and materialize only bounded picker results while preserving ISSUE:4 and ISSUE:5 behavior
- Relationship: independent enough to measure and ship separately from the coordinator migration
- Risks: teardown may hurt reopen latency; related views trade lower overhead for shared crash/memory scope
- Current evidence: initial document size fell from 748,787 to 58,762 bytes, eager reaction-picker choices fell from 2,126 to 0, and sampled release generation fell from 5,531 to 407 microseconds; the native status chooser reuses the same bounded 64-result search/category protocol without another WebView; release startup process-tree PSS fell from 220,974 to 175,719 KiB (20.5%) and the process count fell from four to three while first thread-open and retained-reopen latency remained flat; 891 tests pass with 3 ignored and all 17 Meson suites pass
- Next step: obtain explicit live-workspace confirmation that first thread open, close, retained reopen, focus, scroll, and reaction-picker behavior remain correct; then checkpoint Phase 2
- Public action: implementation commits only; no issue comments, labels, or closures
- Exact-head validation: local formatting and compile checks pass; 891 Rust tests pass with 3 ignored and all 17 Meson suites pass. The first Phase 2 CI run `30817044181` exposed Rust 1.97's `let_and_return` lint in lazy renderer creation; the direct-return fix is awaiting its replacement run. The earlier Phase 1 checkpoint passed CI `30809821964`, CodeQL `30809821289`, and guarded release automation `30810336492`.
