# ISSUE:10 — Lazy WebViews and on-demand emoji DOM

- Status: open; bounded reaction and native status picker automation complete through `46e1f95`, manual Phase 1 checkpoint pending
- Confidence: high
- Impact: avoids an unused thread renderer and repeated full emoji catalogs in initial timeline documents
- Intent: create the thread WebView on demand and materialize only bounded picker results while preserving ISSUE:4 and ISSUE:5 behavior
- Relationship: independent enough to measure and ship separately from the coordinator migration
- Risks: teardown may hurt reopen latency; related views trade lower overhead for shared crash/memory scope
- Current evidence: initial document size fell from 748,787 to 58,762 bytes, eager reaction-picker choices fell from 2,126 to 0, and sampled release generation fell from 5,531 to 407 microseconds; the native status chooser now reuses the same bounded 64-result search/category protocol in a reaction-style grid without another WebView; 891 tests pass with 3 ignored and all 17 serial Meson suites pass
- Next step: obtain explicit live-workspace confirmation for reaction and status search, categories, custom emoji, keyboard, focus, selection, and scroll behavior; then checkpoint Phase 1 and start lazy thread WebView lifecycle coverage
- Public action: implementation commits only; no issue comments, labels, or closures
- Exact-head validation: CI `30809821964`, CodeQL `30809821289`, and guarded release automation `30810336492` pass. The first CI run exposed Rust 1.97's `type_complexity` lint on the picker callback; `46e1f95` introduced a named handler type and the replacement run passed.
