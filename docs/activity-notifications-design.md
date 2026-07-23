# Activity And Notifications Design

> **Status: Historical design record.** This document is preserved as implementation history. Its unread-source and notification-flow statements below are superseded by [Attention And Notifications](attention-and-notifications.md), [Sidebar Design](sidebar-design.md), and [Realtime Sync Design](realtime-sync-design.md); see [README.md](../README.md) for current capabilities.

## Assessment

This feature makes sense for Conduit, but only as a focused attention surface.

The official Slack client has a broad Activity product that aggregates mentions, replies, reactions, saved items, workflow updates, and notification policy. Conduit should not clone that full surface. A lightweight GNOME client should first answer a smaller question: "what conversations currently need my attention?"

## Current Slice

This slice adds a native sidebar entry named **Activity** and a main-pane attention list built from the conversation state Conduit already loads.

The Activity list:

- Includes conversations with unread activity from `users.conversations`.
- Uses the same unread-field interpretation as the sidebar, including Slack extra fields whose names contain `unread`.
- Resolves DM titles through the existing user-name cache.
- Sorts direct messages before group DMs, then private channels, public channels, and unknown conversations; within each group, higher unread counts sort first.
- Opens the selected conversation through the normal history load and read-marker flow.
- Uses the existing native GIO notification path for newly detected messages.

This gives users a single attention-first destination without adding another Slack token type, background event stream, or large local notification database.

## UI

The sidebar primary row is now:

- Home
- Activity
- Later

Activity uses `emblem-important-symbolic`, which is available in the Adwaita icon set used by this project.

The main-pane Activity view is rendered in the existing message WebKit document renderer, matching Search and Later. That keeps the implementation small and consistent with the current shell. A later native GTK list can replace this if the view grows richer than a simple attention list.

## Notifications

Conduit already sends native desktop notifications through `gio::Notification` when a refreshed conversation contains a newer message than the previous local timestamp.

This slice keeps notification behavior conservative:

- No new background polling.
- No per-workspace notification database.
- No notification preference UI.
- No app badge implementation until the target desktop/portal behavior is validated.

## Deferred

The following still belongs in later slices:

- Mentions, thread replies, and reactions aggregated from Slack APIs or realtime events.
- Saved items with reminder/due metadata in Activity.
- Quiet hours and preview preferences.
- Conversation mute and per-conversation notification state.
- App badge support where the runtime environment exposes it reliably.
- Deeper notification deduplication once realtime events exist.

## Tests

The implementation should keep unit coverage for:

- Activity item filtering and sorting.
- Extra unread-field support.
- Activity HTML row rendering and empty state.
- Sidebar selection semantics while Activity is active.
