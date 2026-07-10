# History Pagination And Read State Design

## Assessment

This feature makes sense for a lightweight practical GNOME Slack client.

Conduit should not mirror an entire workspace archive in the background, but a fixed first page of messages is not enough for daily use. Users need to move farther back in a conversation, keep thread paging consistent with channel paging, and have the app advance Slack's read cursor when they actively open a conversation.

## Current Slice

This slice implements the smallest useful native version:

- Slack history calls return page metadata with `has_more` and `response_metadata.next_cursor`.
- Channel history fetches use a larger bounded page size and expose a **Load older messages** action at the top of the WebKit message timeline.
- Thread replies use the same cursor model and expose **Load more replies** at the bottom of the thread timeline.
- The thread pane opens as a readable side rail, not as a fixed one-third split: narrow windows use a balanced split, normal windows target roughly 380px for the thread, and wide windows grow it toward 40% with a 500px cap.
- The GTK window keeps cursors in memory and merges loaded pages by Slack timestamp, newest first, before rendering chronologically.
- Fresh channel history loads mark the conversation read at the newest loaded message timestamp when the token has a read-marker write scope.
- Read-marker calls are best effort and debug logged; failures do not block message loading.
- The local sidebar unread state is cleared immediately for the opened conversation so badges and unread filtering follow the user's read action.
- OAuth now requests the user write scopes needed by Slack's read-marker API for public channels, private channels, DMs, and group DMs.

Cached message bodies are still stored under the app cache directory as derived state. Cursor state is intentionally not persisted in this slice because cursors are short-lived API pagination handles, not durable message state.

## API Model

`conversations.history` returns recent channel messages newest first. Cursor requests continue toward older history, so the channel timeline can place the pagination action above the oldest rendered message.

`conversations.replies` pages thread replies through the same cursor envelope, but the thread is rendered as a reply sequence. Conduit labels this as loading more replies rather than older messages.

`conversations.mark` updates the read cursor for the user token owner. Conduit only attempts it after a successful fresh channel history load, skips duplicate marks for the same channel/timestamp, and does not call it for cached startup data.

## Deferred

The following still belongs in later Big Feature work:

- Timestamp-based newer-message refresh that merges into an already paged conversation without replacing older loaded pages.
- Gap markers when a cached history has missing ranges.
- Explicit message-level mark unread/read actions.
- Persisted pagination ranges if the simple JSON cache stops being sufficient.
- Realtime read-marker reducers once optional Socket Mode exists.

## Tests

The implementation should keep unit coverage for:

- Slack page metadata interpretation.
- Scope parsing for read-marker capability checks.
- Message-page merge ordering and deduplication.
- Local unread state clearing.
- Message HTML pagination action placement and URL encoding.
