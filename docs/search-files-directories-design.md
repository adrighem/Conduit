# Search, Files, And Directories Design

> **Status: Historical design record.** This document is preserved as implementation history and may not describe the current repository. See [README.md](../README.md) for current capabilities and [conductor/tech-stack.md](../conductor/tech-stack.md) for current architecture.

## Assessment

This feature makes sense for Conduit as focused native surfaces.

Search, files, people, and channels are daily Slack workflows, but Conduit should not become a document-management or enterprise-directory clone. The app should expose the high-value views that fit a lightweight desktop client and defer broad Slack-wide indexing until the local store justifies it.

## Current Slice

This slice adds a recent Files view:

- A sidebar Files navigation button.
- A Slack `files.list` call using the existing `files:read` user scope.
- A recent files document in the main pane.
- File rows showing title, file type, and size when Slack provides them.
- External opening through the existing link handler using Slack permalinks or private file URLs.

The primary sidebar navigation is changed to compact icon buttons for Home, Activity, Files, and Later so the row remains stable at the existing sidebar width.

## Existing Coverage

Conduit already has:

- Message search through `search.messages`.
- Local conversation filtering through the sidebar search entry.
- Conversation grouping for channels, DMs, group DMs, and unreads.
- File upload and message attachment rendering.

## Deferred

The following still belongs in later slices:

- Search result tabs for messages, files, people, and channels.
- Query filters for channel, person, date, saved items, and thread context.
- People directory using `users.list`.
- Channel directory beyond the existing sidebar sections.
- File pagination and richer previews.
- Download-to-cache actions and open-with-app integration.

## Tests

The implementation should keep unit coverage for:

- File metadata labels.
- Files document rendering and empty state.
- Runtime/API response parsing through the standard Slack response path.
- Full UI template validation.
