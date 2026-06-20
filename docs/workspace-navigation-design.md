# Workspace Navigation Design

## Assessment

Workspace navigation modernization makes sense for Conduit, but it should be split into small native GTK improvements.

The sidebar is the app's highest-frequency surface. It should expose Slack conversation state clearly without trying to reproduce every official Slack navigation feature at once.

## Current Slice

This slice improves row state fidelity:

- Muted conversations are detected from Slack conversation properties.
- Slack Connect or externally shared conversations are detected from Slack conversation properties.
- Sidebar rows expose muted and external indicators with accessible labels and tooltips.
- The indicators use installed Adwaita symbols:
  - `notifications-disabled-symbolic` for muted conversations.
  - `network-workgroup-symbolic` for external or shared conversations.

This is useful on its own because users need to understand why a row behaves differently before Conduit has full notification preferences or Slack Connect management.

## UI

The existing sidebar row remains compact:

- Conversation type icon.
- Title.
- Optional unread badge.
- Optional muted indicator.
- Optional external/shared indicator.

The indicators are icons, not text badges, so long channel and DM names still get most of the row width.

## Deferred

The following still belongs in later slices:

- Multi-workspace switcher.
- Custom and collapsible sidebar sections.
- Drag and drop section reordering.
- Presence and avatars.
- Per-conversation notification preference editing.
- Slack Connect organization names and management affordances.

## Tests

The implementation should keep unit coverage for:

- Muted and external/shared property detection.
- Sidebar row accessible labels for muted and external state.
- Sidebar section model propagation.
