# ISSUE:10 — Lazy WebViews and on-demand emoji DOM

- Status: open; bounded picker automation complete at `c115324`, manual Phase 1 checkpoint pending
- Confidence: high
- Impact: avoids an unused thread renderer and repeated full emoji catalogs in initial timeline documents
- Intent: create the thread WebView on demand and materialize only bounded picker results while preserving ISSUE:4 and ISSUE:5 behavior
- Relationship: independent enough to measure and ship separately from the coordinator migration
- Risks: teardown may hurt reopen latency; related views trade lower overhead for shared crash/memory scope
- Current evidence: initial document size fell from 748,787 to 58,762 bytes, eager picker choices fell from 2,126 to 0, and the sampled release generation time fell from 5,531 to 407 microseconds; 736 release tests and the production WebKitGTK headless picker test passed
- Next step: obtain explicit live-workspace confirmation for search, categories, custom emoji, keyboard, focus, reaction, and scroll behavior; then checkpoint Phase 1 and start lazy thread WebView lifecycle coverage
- Public action: none taken
