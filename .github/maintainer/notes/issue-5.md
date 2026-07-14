# ISSUE:5 — Reusable emoji picker and Escape cancellation

- Status: Escape bug fixed in `8b3965d`; keep open for the reusable-component acceptance criteria
- Confidence: high
- Implemented: unified cancel path for Escape, close button, cancel event, and backdrop; propagation is stopped and opener focus restored; reaction-specific DOM naming was generalized
- Remaining acceptance gap: reactions still use an HTML dialog/JavaScript implementation while composers use a separate native GTK/Rust popover, so filtering, keyboard navigation, rendering, and accessibility state are duplicated rather than shared
- Recommended next step: extract a shared picker model/controller contract or explicitly narrow this issue and track reuse separately
- Public action: none taken
