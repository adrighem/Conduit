# ISSUE:10 — Lazy WebViews and on-demand emoji DOM

- Status: open; Phase 4 reopened after a live-workspace report that the selected custom emoji preview shows only its `:name:` text; Phases 1 through 3 remain checkpointed
- Confidence: high
- Impact: avoids an unused thread renderer and repeated full emoji catalogs in initial timeline documents
- Intent: create the thread WebView on demand and materialize only bounded picker results while preserving ISSUE:4 and ISSUE:5 behavior
- Relationship: independent enough to measure and ship separately from the coordinator migration
- Risks: teardown may hurt reopen latency; related views trade lower overhead for shared crash/memory scope
- Current evidence: initial document size fell from 748,787 to 58,762 bytes, eager reaction-picker choices fell from 2,126 to 0, and sampled release generation fell from 5,531 to 407 microseconds; the native status chooser reuses the same bounded 64-result search/category protocol without another WebView; release startup process-tree PSS fell from 220,974 to 175,719 KiB (20.5%) and the process count fell from four to three while first thread-open and retained-reopen latency remained flat; the status grid now decodes and schedules animated custom-emoji frames, with a two-frame GIF regression fixture; 891 tests pass with 3 ignored and all 17 Meson suites pass
- Next step: reproduce the selected custom emoji preview regression, render the image and animation beside the status text, and verify it in a live workspace
- Public action: implementation commits only; no issue comments, labels, or closures
- Exact-head validation: local formatting and compile checks pass; 891 Rust tests pass with 3 ignored and all 17 Meson suites pass. Phase 3 passed CI `30820076215`, CodeQL `30820074669`, and guarded release automation `30820784870`. Manual live-workspace verification passed on 2026-08-03.
