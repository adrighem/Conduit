# Deterministic Conversation Opening

## Summary

Make opening a conversation a generation-scoped transaction with one immutable semantic target and one viewport authority. Cached history should remain fast, fresh history and metadata should reconcile without restarting navigation, and read observation must begin only after the opening position has settled.

## Requirements

1. Every conversation opening must create a monotonically increasing session generation scoped to the selected channel.
2. An opening session must own one semantic target: latest message, first unread message after a snapshotted read cursor, or an explicit message timestamp.
3. Target priority must be explicit message, then first unread, then latest.
4. Cached and fresh history for the active request must participate in the same opening session; fresh history must not independently reinterpret the target.
5. Stale render callbacks and results from superseded opening generations must not reposition the active conversation.
6. JavaScript must be the sole owner of viewport geometry. Rust supplies semantic intent and message data but must not install competing scroll programs.
7. Initial positioning must happen behind a hidden positioning state and reveal the timeline only after the target has been applied over stable animation frames.
8. Deliberate user scrolling must end automatic initial positioning for that generation.
9. Read observation must be armed only after the initial target is committed.
10. Fresh history, user metadata, avatars, images, pagination, and realtime changes should use stable DOM reconciliation or anchor-preserving updates where feasible instead of replacing the document.
11. Conversation clicks must continue to open read conversations at latest and unread conversations at first unread; explicit search and permalink targets must remain centered.
12. Existing single-workspace behavior and Slack API semantics must remain unchanged.

## Acceptance Criteria

- Pure tests cover generation creation, immutable targets, target priority, cached-to-fresh transitions, stale generations, and user-interaction cancellation.
- An unread cursor snapshot cannot be changed by read advancement while the same opening session is positioning.
- Cached and fresh history can render in either order without producing more than one initial-position commit.
- An explicit target in an otherwise read conversation cannot compete with bottom positioning.
- The generated timeline contains one initial viewport controller rather than independent scroll-restoration and focus scripts.
- Read observers are not active until the controller commits the opening.
- WebKit integration coverage delays content growth and snapshot reconciliation and proves the committed target remains stable.
- Metadata and asset updates arriving during document loading are coalesced and cannot cause an older generation to replace the active conversation.
- `cargo fmt --check`, strict Clippy, all Rust tests, `cargo check`, Meson compile, and Meson tests pass under a sanitized allowlisted environment.

## Architectural Constraints

- Reuse the existing request/session identity, `WorkspaceViewState`, typed DOM patch protocol, and anchor-preserving timeline runtime.
- Keep semantic opening policy in headless Rust code; keep viewport measurements and scrolling in JavaScript.
- Do not add a frontend framework or replace WebKitGTK.
- Do not log message bodies, credentials, browser-session data, or complete environments.
- Prefer incremental compatibility adapters over a simultaneous rewrite of all timeline surfaces.

## Out of Scope

- Multi-workspace operation.
- Replacing the message timeline with native GTK widgets.
- Changing thread-opening behavior beyond sharing safe viewport primitives.
- Fetching complete Slack history solely to locate an unread boundary that Slack cannot expose in the available page.
- Redesigning message presentation or composer behavior.
