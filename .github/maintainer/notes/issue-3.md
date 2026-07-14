# ISSUE:3 — Paste clipboard images to upload screenshots

- Status: composer workflow shipped in `8b3965d`; keep open unless scope is explicitly narrowed
- Confidence: high for main/thread composers; high that timeline-focused paste is not handled
- Implemented: image detection on both composer text views, PNG staging, thread-aware upload, normal text paste, progress/failure feedback, success/error/startup cleanup
- Remaining acceptance gap: paste is not captured when focus is elsewhere in the active conversation pane, such as the message timeline/WebView
- Recommended next step: add a conversation-scoped paste action/key path that excludes search fields, dialogs, and unrelated controls
- Public action: none taken
