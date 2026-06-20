# Realtime Sync Design

This document covers Big Feature 2 from `docs/modernization-plan.md`: realtime event ingestion.

## Assessment

Realtime sync makes sense for Conduit only as an optional advanced capability. It should not be required for the default lightweight GNOME desktop experience.

The official Slack Events API and Socket Mode model requires app-level Slack configuration and an `xapp-` token with `connections:write`. That is a different setup burden from Conduit's current user-token PKCE flow. Adding it as a default requirement would make the app harder to install and package.

## Decision For This Pass

Do not implement live Socket Mode ingestion in this pass.

The current codebase is better served by first finishing the local state, history pagination, and read-marker model that future realtime events would update. Without those reducers, Socket Mode would mostly trigger expensive whole-conversation refreshes and would add a long-lived token surface before the app has useful event semantics.

This pass records the future design and corrects project status documentation so Conduit does not claim Socket Mode support prematurely.

## Future Architecture

When implemented, realtime sync should use these components:

- A separate app-token setup path, clearly marked advanced.
- Secure storage for the app-level Socket Mode token, separate from the user token.
- A Slack app API client for `apps.connections.open`.
- A cancellable background Socket Mode task owned by the runtime.
- A small event envelope parser that acknowledges every Socket Mode envelope.
- Event reducers that update `WorkspaceStore` and then emit narrow runtime events.
- Backoff/reconnect behavior that never blocks manual refresh or normal Web API use.

## Event Scope

Initial event reducers should cover:

- New message.
- Edited message.
- Deleted message.
- Reaction added or removed.
- Channel rename, archive, or membership change.
- Direct-message and group-DM activity.
- User/profile updates.
- Read-marker updates once the read-state model exists.

Events that cannot be reduced safely should trigger a targeted refresh rather than a full workspace refresh.

## UI Policy

Realtime should be invisible when unavailable. The app should keep working with:

- Manual refresh.
- Cached conversations and histories.
- Direct Web API calls.

When Socket Mode is configured, the UI may show a compact status in debug output or an advanced preferences area. It should not add persistent sidebar noise.

## Security And Packaging

- Do not request bot scopes in the default PKCE flow.
- Do not store app tokens in cache files.
- Do not require Socket Mode for Flatpak packaging or normal user setup.
- Keep logs free of access tokens, app tokens, authorization codes, and Socket Mode URLs.

## Revisit Criteria

Implement live Socket Mode after:

1. History pagination and read-marker reducers exist.
2. Runtime state updates can apply targeted message/conversation deltas.
3. There is a secure app-token setup path.
4. The app can cancel/restart realtime tasks on sign-out and workspace changes.
