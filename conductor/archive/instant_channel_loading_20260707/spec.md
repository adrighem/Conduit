# Track Spec: Near Instant Channel Loading

## Goal
Make opening Slack channels feel near instant by showing a small latest-message timeline immediately whenever recent history can be prepared ahead of selection.

## Context
Channel history already supports caching and older-message pagination, but a first open can still feel network-bound when no local history exists for that channel. Channels with large histories should not require rendering or fetching a large timeline before the latest messages are visible.

## Requirements
- Fetch only a small latest-message page for initial channel history loads.
- Render cached channel history using the same small recent-message preview.
- Warm recent channel history in the background after conversation refresh, without changing read state or interrupting sidebar rendering.
- Bound background prefetching so it does not become an unbounded Slack API sweep on large workspaces.
- Keep existing explicit **Load older messages** pagination behavior.
- Add unit coverage for cache-preview and prefetch-candidate decisions.

## Acceptance Criteria
- Opening a channel with prefetched or cached history renders the latest message preview before the fresh Slack history request completes.
- Fresh initial channel history requests use roughly 30 messages.
- Background prefetch stores recent history for a bounded set of likely channel candidates.
- Prefetch failures are best-effort and do not surface disruptive user-facing errors.
- Existing tests pass, and a compile command is run after code changes.

## Out of Scope
- Full offline message archive.
- Local full-text search.
- Unbounded prefetch of every conversation in a workspace.
- Changing Slack read/unread semantics during prefetch.
