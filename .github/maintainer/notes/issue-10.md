# ISSUE:10 — Lazy WebViews and on-demand emoji DOM

- Status: open; separable P0 memory optimization
- Confidence: high
- Impact: avoids an unused thread renderer and repeated full emoji catalogs in initial timeline documents
- Intent: create the thread WebView on demand and materialize only bounded picker results while preserving ISSUE:4 and ISSUE:5 behavior
- Relationship: independent enough to measure and ship separately from the coordinator migration
- Risks: teardown may hurt reopen latency; related views trade lower overhead for shared crash/memory scope
- Next step: split WebView lifecycle and emoji materialization into measured slices with release-build PSS, HTML-size, and latency baselines
- Public action: none taken
