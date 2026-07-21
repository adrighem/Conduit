# Local State And Sync Design

> **Status: Historical design record.** This document is preserved as implementation history and may not describe the current repository. See [README.md](../README.md) for current capabilities and [conductor/tech-stack.md](../conductor/tech-stack.md) for current architecture.

This document covers Big Feature 1 from `docs/modernization-plan.md`: durable local store and rate-limited sync.

## Assessment

This feature makes sense for Conduit. A lightweight GNOME Slack client should not mirror an entire Slack workspace, but it should avoid blank startup and repeated network-only navigation when recent data is already available locally.

The practical design is a small cache, not a database-backed offline archive.

## Goals

- Store derived Slack state only under the app cache directory.
- Keep Slack tokens exclusively in the system keyring.
- Render cached conversations and recently loaded histories before network refreshes finish.
- Replace cached snapshots with successful Slack API responses.
- Avoid introducing SQLite until local query needs justify it.
- Handle Slack Web API `429` responses with `Retry-After` instead of failing immediately.

## Cache Location

All cache files live below:

```text
${XDG_CACHE_HOME:-~/.cache}/eu.vanadrighem.conduit/state
```

This keeps actual caching in a subfolder below `~/.cache`, alongside the existing WebKit and image-asset caches.

## Stored Data

The cache stores one JSON snapshot per authenticated workspace/user pair. The workspace key is a SHA-256 digest of the Slack team identity and current user identity, so filenames do not expose workspace or user IDs.

Stored state:

- Conversation list returned by Slack.
- Recently loaded channel histories.
- Recently loaded thread replies.

The cache intentionally does not store:

- Access tokens, refresh tokens, OAuth codes, or client IDs.
- Full workspace message archives.
- Search indexes.
- Cross-workspace global state.

## Runtime Flow

After authentication succeeds:

1. Build a `WorkspaceStore` for the authenticated team/user pair.
2. Load cached conversations, if present, and emit them to the UI.
3. Fetch fresh conversations from Slack.
4. Store the fresh conversation list.
5. Emit the fresh conversation list to the UI.

When a conversation or thread is opened:

1. Load cached messages for that channel/thread, if present, and emit them to the UI.
2. Fetch fresh messages from Slack.
3. Store the fresh messages.
4. Emit the fresh messages to the UI.

Cache read/write failures are debug-logged and do not fail the user-visible Slack operation.

## Rate-Limit Handling

Slack Web API form calls handle `429 Too Many Requests` by reading the `Retry-After` response header, waiting for that duration, and retrying the method. Invalid or missing retry values fall back to one second, and the delay is capped to keep the client responsive.

This is not a complete scheduler. Per-method request budgeting and background sync queues belong with later realtime/history pagination work.

## Future Work

- Add timestamp metadata and prune old cached histories.
- Cache user display names and avatars once presence/avatar work starts.
- Add event reducers once realtime ingestion lands.
- Revisit SQLite only if search, directories, or multi-workspace history need local queries.
