# ISSUE:5 — Reusable emoji picker and Escape cancellation

- Status: shared picker contract implemented locally; closure-ready after remote CI
- Confidence: high
- Implemented: widget-independent picker model, shared catalog/filter ordering, accessible labels and selection semantics; native composer and WebView reaction frontends consume that contract; reactions now support Up/Down/Enter and active-descendant state in addition to unified cancellation
- Boundary: GTK widget code cannot run inside WebKit, so each frontend retains a thin interaction adapter while sharing the Rust-owned model and behavior contract
- Public action: none taken
