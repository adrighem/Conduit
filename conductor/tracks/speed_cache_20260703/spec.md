# Track Spec: Speed Optimization And Channel Caching

## Track Description

Improve perceived speed and stability while loading conversations and channel history.

## Problem

Conduit currently becomes awkward to use while conversation data and channel history are refreshing. The left sidebar is rebuilt during loading/status updates, which can interrupt filtering, selection, scrolling, and row activation. Opening a channel can still feel network-bound even though recent history is cached. The message pane also starts at the top of the timeline when users expect Slack-like bottom anchoring around the latest messages.

## Requirements

- Keep the left sidebar usable while conversations refresh.
- Preserve sidebar filter text, toggle state, selected row, and scroll position during background conversation updates.
- Avoid replacing a populated sidebar with a transient loading state.
- Reduce unnecessary full sidebar rebuilds when only loading/status or user-name cache state changes.
- Render recent cached channel history immediately when available.
- Keep fetching the latest channel page after cached history renders, so stale cache is refreshed.
- Treat cached history and fresh Slack history differently so cached loads do not clear pagination cursors or unread/read behavior incorrectly.
- Keep channel history focused on the latest messages first.
- Keep older-history pagination available from the top of the timeline.
- Cache merged channel history after older pages are loaded when that does not turn the cache into an unbounded archive.
- Avoid duplicate in-flight history loads for the same channel.
- Default the channel message pane to the bottom of the latest loaded timeline.
- Keep the pane stuck to the bottom when it was already at the bottom and a new message is received or sent.
- Do not force-scroll to the bottom when the user has intentionally scrolled up to read older messages.
- Preserve scroll position when older messages are prepended.

## Acceptance Criteria

- Refreshing conversations with existing cached or loaded conversations leaves the sidebar interactive and populated.
- Sidebar loading state appears in the footer or status area when data already exists, not as a list replacement.
- Filtering or activating a visible sidebar row continues to work during conversation refresh.
- Selecting a channel with cached history renders cached messages before the fresh Slack request completes.
- The latest fresh history page replaces or merges with cached history without losing available older-message pagination state.
- Loading older messages prepends older content and preserves the user's reading position.
- Opening a channel scrolls to the newest loaded message by default.
- If the user is at the bottom, incoming history refreshes and sent messages keep the view at the bottom.
- If the user has scrolled up, incoming updates do not steal the scroll position.
- Unit tests cover sidebar refresh policy, history load/cache decisions, duplicate-load prevention, and scroll intent decisions.
- `cargo test`, `cargo check`, and `meson compile -C _build` pass.

## Out Of Scope

- Full offline message archive or local search index.
- SQLite or a new database dependency.
- Realtime Socket Mode ingestion.
- Slack custom section sync or Slack-side sidebar ordering.
- Multi-workspace performance work.
- Avatar or presence caching.
