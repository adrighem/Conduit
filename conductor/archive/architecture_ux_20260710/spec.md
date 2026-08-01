# Architecture and Usability Modernization

## Summary

Evolve Conduit from a feature-rich prototype into a durable desktop architecture without discarding its tested GTK/WebKit approach. Correctness and state ownership come first, followed by responsiveness, clearer module boundaries, adaptive accessibility, and coherent product semantics.

## Requirements

1. Workspace cache updates must be serialized so concurrent refresh, history, thread, and user-name writes cannot lose unrelated state.
2. Runtime work must carry session/request identity. Stale responses and tasks from signed-out or replaced sessions must not change the visible workspace.
3. Independent network operations must not block behind image downloads, uploads, or unrelated Slack requests. GTK event delivery must be event-driven rather than timer-polled.
4. Workspace view state and transition logic must have a clear owner outside the GTK window subclass; pure transition logic must be unit tested.
5. Runtime failures must identify their operation and target so the UI can recover and present errors locally without marking unrelated surfaces as failed.
6. The shell must adapt to narrow windows, expose persistent accessible labels for authentication/composer controls, and communicate the active navigation surface.
7. Generated message documents must meet WCAG 2.2 AA contrast, support keyboard/touch/narrow-pane use, use logical CSS properties, avoid advertising unavailable actions, and expose locale-aware semantic HTML.
8. Product semantics must be coherent: notifications open their conversation, the misleading Home/Activity behavior is corrected, search results can navigate internally, and per-conversation drafts survive navigation and restart.
9. User-facing strings introduced or touched by this track must use the existing gettext path where supported.

## Acceptance Criteria

- Concurrent store-update tests preserve every updated state field.
- Tests prove stale channel/search/thread/session events are ignored.
- A slow image request or upload no longer prevents message/search/history requests from starting.
- The UI consumes runtime events without a repeating timeout.
- `window.rs` delegates workspace transition decisions to a tested state/controller module.
- Error events carry operation metadata and restore only relevant controls.
- The workspace remains usable at narrow widths, and authentication/composer inputs have accessible names.
- Message HTML passes semantic assertions and all tested foreground/background pairs meet WCAG AA contrast.
- Notifications have a stable ID and activation target; drafts round-trip through settings; search results expose internal navigation.
- `cargo fmt --check`, strict Clippy, all Rust tests, Meson compile, and Meson tests pass.

## Out of Scope

- Multi-workspace switching.
- Presence and avatar synchronization.
- A full Slack-wide mentions/replies/reactions Activity API that Slack does not currently expose through Conduit's configured endpoints. The current surface may be renamed to accurately describe unread conversations.
- Replacing WebKitGTK with a native message widget tree.
- CSS features with less than 80% browser support.
