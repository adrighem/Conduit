# Realtime Sync Design

This document covers Big Feature 2 from `docs/modernization-plan.md`: realtime event ingestion.

## Assessment

Realtime sync makes sense for Conduit only as an optional advanced capability. It should not be required for the default lightweight GNOME desktop experience.

The official Slack Events API and Socket Mode model requires app-level Slack configuration and an `xapp-` token with `connections:write`. That is a different setup burden from Conduit's current user-token PKCE flow. Adding it as a default requirement would make the app harder to install and package.

## Current Slice

Conduit implements optional live Socket Mode ingestion.

Socket Mode is enabled only when an app-level token is provided through `CONDUIT_SLACK_APP_TOKEN` or `SLACK_APP_TOKEN`. The default user-token authentication path remains unchanged, and the app continues to work with manual refresh and direct Web API calls when no app token is configured.

The runtime starts a single Socket Mode connection after workspace authentication and aborts it on sign-out or reconnect. It calls `apps.connections.open`, connects to the temporary WebSocket URL, acknowledges every envelope with its `envelope_id`, and reconnects after Slack disconnect or refresh requests. If Slack reports `link_disabled` because Socket Mode is disabled in the app settings, Conduit keeps retrying with capped backoff so the running client reconnects automatically once the Slack app is enabled again.

The first reducer set covers:

- New message events.
- Edited message events.
- Deleted message events.
- Reaction added or removed events.
- Conversation membership, rename, archive, and related events that should refresh the sidebar.

Unsupported envelopes are acknowledged and ignored.

## Deferred Architecture

Future Socket Mode work should add:

- A native app-token setup and secure storage path, separate from the user token.
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

When Socket Mode is configured, connection state is logged through debug output. It should not add persistent sidebar noise.

## Security And Packaging

- Do not request bot scopes in the default PKCE flow.
- Do not store app tokens in cache files. The current implementation reads them from the environment only.
- Do not require Socket Mode for Flatpak packaging or normal user setup.
- Keep logs free of access tokens, app tokens, authorization codes, and Socket Mode URLs.

## Revisit Criteria

Expand live Socket Mode after:

1. A secure app-token setup path exists.
2. Read-marker reducers exist.
3. Runtime state updates can apply additional targeted message/conversation deltas.
4. Activity can represent mention, reply, and reaction notifications beyond conversation unread counts.
