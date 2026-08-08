# Realtime Sync Design

This document covers Big Feature 2 from `docs/modernization-plan.md`: realtime event ingestion.

## Assessment

Realtime sync makes sense for Conduit only as an optional advanced capability. It should not be required for the default lightweight GNOME desktop experience.

The official Slack Events API and Socket Mode model requires app-level Slack configuration and an `xapp-` token with `connections:write`. That is a different setup burden from Conduit's current user-token PKCE flow. Adding it as a default requirement would make the app harder to install and package.

## Current Slice

Conduit implements optional live ingestion through either official Socket Mode or Slack's browser-session WebSocket.

OAuth workspaces use Socket Mode when an app-level token is stored or provided through `CONDUIT_SLACK_APP_TOKEN` or `SLACK_APP_TOKEN`. Imported XOXC/XOXD workspaces instead use the browser-session WebSocket automatically and do not need an app token. The app continues to work with manual refresh and direct Web API calls when no realtime transport is configured.

The runtime starts one supervisor-owned realtime connection after workspace authentication. On sign-out, session replacement, or runtime shutdown, the supervisor stops transport admission and waits for accepted persistence work before the old session completes. Socket Mode calls `apps.connections.open` and acknowledges admitted envelopes with their `envelope_id`; browser sessions call `client.getWebSocketURL` and consume browser RTM events. Both transports reconnect with capped backoff after disconnects. If Slack reports `link_disabled`, Conduit keeps retrying so the running client reconnects once the link is enabled again.

One session-owned actor queue carries messages, user/profile changes, and reactions. Message and
reaction effects remain ordered behind it before UI fan-out; user changes also use it for cache
persistence. The actor admits at most 256 pending events. Its asynchronous transport callback waits
for capacity instead of dropping or reordering an event. Socket Mode admission and acknowledgment
share a three-second deadline; a timeout or closed actor leaves the envelope unacknowledged and
reconnects so Slack can retry it. Browser RTM applies socket backpressure while admission waits. The
actor drains before a normal reconnect or supervised session shutdown. Live trace events report
queue high-water marks at depth 1 and each new power-of-two peak.

Huddle observations and user commands use a separate session-owned actor. Its channel holds at most
64 pending items and processes accepted items in FIFO admission order. Observations may occupy no
more than 56 positions, reserving eight positions for commands. An observation waiting for that
budget is not yet admitted, so a later command can use reserved capacity before it; out-of-band
lifecycle supervision can also proceed. Neither case reorders accepted channel items. Observation
producers wait for admission rather than dropping state. For Socket Mode, that wait shares the
transport's three-second admission and acknowledgment deadline, so a timeout or closed huddle actor
leaves the Slack envelope unacknowledged for retry. A huddle command completes only after its
coordinator transition and resulting effects have finished.

Session replacement and shutdown stop and drain the realtime actor first, then drain and reset the
huddle actor and wait for its task to finish. Huddle actor metrics contain queue and lifecycle
metadata only, never huddle payload content or Slack identifiers.

The optional generic native media engine and synthetic harness use another session-scoped mailbox for
custom GStreamer callbacks. It holds at most 64 entries. SDP, ICE, and failure callbacks are reliable
FIFO items; statistics use one latest-only entry. Reliable callbacks evict pending statistics first.
If reliable callbacks fill the mailbox, the next reliable callback clears queued negotiation values,
emits one terminal `AdmissionSaturated` failure, and closes that generation. Generation checks reject
callbacks left over from a stopped session after restart.

Native media admission limits SDP to 256 KiB, each ICE candidate to 8 KiB, and remote ICE to 256
candidates per session. Only one offer promise, one statistics promise, one incoming audio branch,
and one incoming video branch may exist at a time. Each GStreamer queue allows eight buffers and
250 ms, uses no byte limit, and leaks downstream. Stop closes and clears callback admission before
capture and pipeline teardown; repeated stops are safe.

No production runtime event pump consumes this mailbox, and no verified Slack bootstrap or Amazon
Chime adapter exists. Production native joining therefore remains unavailable. Portal lifecycle,
production signalling, and synthetic-harness hardening remain a separate next slice.

GTK-to-runtime commands use a separate bounded admission layer before task creation. A reserved FIFO
serves session and control work, while navigation, interactive, upload, background, and image lanes
have independent queue and task caps. Durable actions and read markers stay FIFO and serialize per
target. Only explicitly replaceable synchronization is coalesced or superseded. Saturation returns
the rejected command to GTK immediately so request-specific UI state can recover without blocking
the main thread. Session starts remain FIFO and wait for older accepted durable/read work before
replacement; normal runtime shutdown drains every accepted command lease.

Runtime-to-GTK publication uses a separate FIFO mailbox with capacity 256. Non-progress events wait
synchronously for space behind FIFO tickets, so task cancellation cannot interrupt publication
after a workspace patch has been persisted. Attachment-download and file-upload progress are the
only lossy events. They share a 32-entry sub-cap and replace older queued progress for the same
session, request, context, and progress kind; progress is dropped when either cap is full or reliable
publication is already waiting. Matching success or error events remove stale progress before
entering the reliable FIFO. Sender closure drains accepted events before EOF, while receiver closure
wakes blocked producers. GTK consumes events serially and yields after each eight-event batch.
Mailbox metrics cover admission, dequeue, blocking, closure, depth, peak depth, and coalesced or
dropped progress.

The workspace coordinator classifies every normalized message with the canonical attention policy.
Realtime persistence first performs a pure preview, then atomically records the observation and
notification claim. The committed reduction reclassifies under the latest live preference snapshot
before any native-notification candidate is emitted. SQLite retains the 512 most recently recorded
message identities per conversation and the 512 most recent notification claims per workspace.
Within those bounded windows, `already_observed` redelivery cannot create duplicate candidates or
unread state; `at_or_before_read_cursor` means the durable local read cursor rejected an older
delivery. See [Attention And Notifications](attention-and-notifications.md) for policy, raw-unread,
and measurement details.

The first reducer set covers:

- New message events.
- Edited message events.
- Deleted message events.
- Reaction added or removed events.
- User/profile and huddle-status updates.
- Conversation membership, rename, archive, and related events that should refresh the sidebar.

Unsupported envelopes are acknowledged and ignored.

## Deferred Architecture

Future Socket Mode work should add:

- Realtime reducers for direct-message and group-DM activity that Slack does not deliver as plain message payloads.
- Read-marker reducers once the read-state model exists.
- Activity aggregation for mentions, thread replies, and reactions.

Events that cannot be reduced safely should trigger a targeted refresh rather than a full workspace refresh.

## UI Policy

Realtime should be invisible when unavailable. The app should keep working with:

- Manual refresh.
- Cached conversations and histories.
- Direct Web API calls.

Preferences shows the live handshake state. Browser-session workspaces show an XOXC/XOXD status row and hide the irrelevant app-token editor; OAuth workspaces retain the Socket Mode token editor.

Preferences → Notifications updates the running attention policy without restarting the connection.

## Security And Packaging

- Do not request bot scopes in the default PKCE flow.
- Do not store app tokens or browser-session credentials in cache files. App tokens and imported sessions use the system keyring; environment configuration remains available for development and packaging.
- Do not require Socket Mode for Flatpak packaging or normal user setup.
- Keep logs free of access tokens, app tokens, authorization codes, and Socket Mode URLs.
- Opt into the privacy-scoped attention target with
  `RUST_LOG=conduit::attention=trace conduit`. That target contains only counters, booleans, and
  stable category codes—never message text, configured terms, or workspace/user/conversation/message
  identifiers. General `--debug` output is outside that target-specific guarantee.

Attention snapshots are emitted after the actor drains, but their counters and peak queue depth are
cumulative for the runtime session. Attention-ledger outcomes report observation/claim handling,
not the success of unrelated message-history, user/profile, or reaction persistence.

## Revisit Criteria

Expand live Socket Mode after:

1. Read-marker reducers exist.
2. Runtime state updates can apply additional targeted message/conversation deltas.
3. Activity can represent mention, reply, and reaction notifications beyond conversation unread counts.
