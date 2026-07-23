# Realtime Sync Design

This document covers Big Feature 2 from `docs/modernization-plan.md`: realtime event ingestion.

## Assessment

Realtime sync makes sense for Conduit only as an optional advanced capability. It should not be required for the default lightweight GNOME desktop experience.

The official Slack Events API and Socket Mode model requires app-level Slack configuration and an `xapp-` token with `connections:write`. That is a different setup burden from Conduit's current user-token PKCE flow. Adding it as a default requirement would make the app harder to install and package.

## Current Slice

Conduit implements optional live ingestion through either official Socket Mode or Slack's browser-session WebSocket.

OAuth workspaces use Socket Mode when an app-level token is stored or provided through `CONDUIT_SLACK_APP_TOKEN` or `SLACK_APP_TOKEN`. Imported XOXC/XOXD workspaces instead use the browser-session WebSocket automatically and do not need an app token. The app continues to work with manual refresh and direct Web API calls when no realtime transport is configured.

The runtime starts one realtime connection after workspace authentication and aborts it on sign-out or reconnect. Socket Mode calls `apps.connections.open` and acknowledges every envelope with its `envelope_id`; browser sessions call `client.getWebSocketURL` and consume browser RTM events. Both transports reconnect with capped backoff after disconnects. If Slack reports `link_disabled`, Conduit keeps retrying so the running client reconnects once the link is enabled again.

One session-owned actor queue carries messages, user/profile changes, and reactions. Message and
reaction effects remain ordered behind it before UI fan-out; user changes also use it for cache
persistence. The transport callback is synchronous, so the actor uses an unbounded queue rather
than blocking the connection while waiting for capacity. It drains before reconnect. Live trace
events report queue high-water marks at depth 1 and each new power-of-two peak.

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
