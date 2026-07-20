# Immediate conversation switcher with lazy population

## Summary

The Ctrl+K path currently clones, ranks, and creates GTK widgets for the complete conversation, channel, and people catalog before presenting its window. Present the shell first and incrementally populate results without changing search ranking or activation behavior.

## Requirements

- Present and focus the switcher before building its result sections or row widgets.
- Show an explicit loading state until the first result batch is ready.
- Append rows in bounded idle-time batches so large workspaces do not monopolize GTK's main loop.
- Cancel stale batches when the query changes, discovery updates arrive, or the dialog closes.
- Preserve section ordering, relevance ranking, row actions, keyboard focus, and empty-result behavior.
- Apply the same safe lazy population path to the message-forwarding conversation picker.
- Add deterministic unit coverage for batching, ordering, and stale-generation behavior plus the existing headless keyboard regression.

## Acceptance criteria

- Ctrl+K calls `present()` before catalog snapshot/ranking and row population begin.
- No callback appends more than the configured row batch size.
- Typing during initial population replaces, rather than interleaves with, stale rows.
- Closing the picker prevents queued callbacks from touching it.
- Existing picker ranking and keyboard shortcut tests continue to pass.

## Out of scope

- Changing the conversation ranking algorithm or adding new searchable fields.
- Replacing every sidebar list with `GtkListView` in this iteration.
