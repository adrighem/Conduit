# Realtime Sync Design

This document covers Big Feature 2 from `docs/modernization-plan.md`: realtime event ingestion.

## Assessment

Realtime sync makes sense for Conduit only as an optional advanced capability. It should not be required for the default lightweight GNOME desktop experience.

The official Slack Events API and Socket Mode model requires app-level Slack configuration and an `xapp-` token with `connections:write`. That is a different setup burden from Conduit's current user-token PKCE flow. Adding it as a default requirement would make the app harder to install and package.

## Current Slice

Conduit implements optional live ingestion through either official Socket Mode or Slack's browser-session WebSocket.

OAuth workspaces use Socket Mode when an app-level token is stored or provided through `CONDUIT_SLACK_APP_TOKEN` or `SLACK_APP_TOKEN`. Imported XOXC/XOXD workspaces instead use the browser-session WebSocket automatically and do not need an app token. The app continues to work with manual refresh and direct Web API calls when no realtime transport is configured.

The runtime starts one realtime connection after workspace authentication and aborts it on sign-out or reconnect. Socket Mode calls `apps.connections.open` and acknowledges every envelope with its `envelope_id`; browser sessions call `client.getWebSocketURL` and consume browser RTM events. Both transports reconnect with capped backoff after disconnects. If Slack reports `link_disabled`, Conduit keeps retrying so the running client reconnects once the link is enabled again.

The first reducer set covers:

- New message events.
- Edited message events.
- Deleted message events.
- Reaction added or removed events.
- Conversation membership, rename, archive, and related events that should refresh the sidebar.

Unsupported envelopes are acknowledged and ignored.

## Deferred Architecture

Future Socket Mode work should add:

- Realtime reducers for direct-message and group-DM activity that Slack does not deliver as plain message payloads.
- User/profile update reducers.
- Read-marker reducers once the read-state model exists.
- Activity aggregation for mentions, thread replies, and reactions.

Events that cannot be reduced safely should trigger a targeted refresh rather than a full workspace refresh.

## UI Policy

Realtime should be invisible when unavailable. The app should keep working with:

- Manual refresh.
- Cached conversations and histories.
- Direct Web API calls.

Preferences shows the live handshake state. Browser-session workspaces show an XOXC/XOXD status row and hide the irrelevant app-token editor; OAuth workspaces retain the Socket Mode token editor.

## Security And Packaging

- Do not request bot scopes in the default PKCE flow.
- Do not store app tokens or browser-session credentials in cache files. App tokens and imported sessions use the system keyring; environment configuration remains available for development and packaging.
- Do not require Socket Mode for Flatpak packaging or normal user setup.
- Keep logs free of access tokens, app tokens, authorization codes, and Socket Mode URLs.

## Revisit Criteria

Expand live Socket Mode after:

1. Read-marker reducers exist.
2. Runtime state updates can apply additional targeted message/conversation deltas.
3. Activity can represent mention, reply, and reaction notifications beyond conversation unread counts.
