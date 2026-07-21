# Improvement And Modernization Plan

> **Status: Historical plan record.** This document is preserved as implementation history and may not describe the current repository. See [README.md](../README.md) for current capabilities and [conductor/tech-stack.md](../conductor/tech-stack.md) for current architecture.

This plan compares Conduit to the official Slack desktop client from an architecture and product-scope perspective, then narrows the work to what makes sense for a lightweight practical GNOME desktop app.

The build order is intentionally incremental. Each Big Feature must ship as a small vertical slice with its own design notes, tests, commit, and push. After each Big Feature lands, reread this plan and the current repository state before starting the next slice.

## Product Direction

Conduit should not try to clone the official Slack client. Slack is now a broad collaboration suite with chat, Activity, Later, Files, Tools, Directories, canvases, lists, workflows, huddles, AI, administration surfaces, and enterprise integrations.

Conduit should instead be a reliable native GNOME client for daily messaging:

- Fast access to workspaces, channels, DMs, threads, unread activity, saved items, search, and files.
- Local state that makes the app useful during slow network periods without becoming a full offline Slack archive.
- Clear boundaries where Slack-only surfaces open in Slack instead of being reimplemented poorly.
- Native GTK/libadwaita UI for shell, navigation, controls, and preferences.
- WebKit only where rich message rendering or Slack web handoff is the pragmatic choice.

## Build Order

### 1. Durable Local Store And Rate-Limited Sync

Assessment: yes, implement natively.

This is the foundation for most other features. A lightweight client still needs a small local state layer so startup, refresh, and navigation are predictable. The store should live under the app cache directory, not in configuration or data directories, because it is derived Slack content and can be recreated.

Scope:

- Cache workspace conversation lists and recently loaded message histories.
- Keep tokens only in the existing keyring path, never in the cache.
- Prefer simple structured files first; avoid SQLite until query needs justify it.
- Use atomic writes to avoid corrupt cache files.
- Add rate-limit handling around Slack Web API calls, especially `429` plus `Retry-After`.
- Surface cached data before fresh network data when possible.

Out of scope for this slice:

- Full offline search.
- Full message archive sync.
- Cross-workspace global database.
- Sync conflict resolution beyond replacing cached snapshots with successful API responses.

Design file: `docs/local-state-and-sync-design.md`.

### 2. Realtime Event Ingestion

Assessment: yes, but only as an optional advanced integration.

Official Slack realtime behavior depends on Events API delivery. A desktop app can use Socket Mode, but Socket Mode requires app-level Slack configuration and a separate app token. That is not frictionless enough for the default first-run flow.

Scope:

- Add an optional Socket Mode settings path.
- Subscribe to events that update existing local state: new messages, message changes/deletes, reactions, read markers, channel metadata, and user/profile changes.
- Feed events into reducers that update the local store and then the UI.
- Preserve manual refresh as the fallback path.

Out of scope:

- Requiring Socket Mode for normal app use.
- Public HTTP event endpoint.
- Building admin tooling to configure a Slack app.

Design file: `docs/realtime-sync-design.md`.

### 3. Full History Pagination And Read Markers

Assessment: yes, implement natively.

The current fixed-size history fetch is enough for a prototype but not for daily use. Pagination and read markers are core chat-client behavior.

Scope:

- Older-message pagination.
- Newer-message refresh using timestamps.
- Gap detection when cached history is incomplete.
- Mark conversation read when appropriate.
- Add explicit mark unread/read actions where Slack API support is available.
- Keep thread pagination aligned with channel history pagination.

Out of scope:

- Full workspace archive mirroring.
- Aggressive background crawling of every channel.

Design file: `docs/history-and-read-state-design.md`.

### 4. Activity And Notifications

Assessment: yes, but narrower than official Slack.

Conduit needs a practical native Activity view for messages that need attention, not the entire official Slack notification preference matrix.

Scope:

- Activity list for DMs, mentions, thread replies, reactions, and saved items with due/reminder metadata if available.
- Native desktop notifications through GTK/GIO.
- App badge support where the platform exposes it.
- Local notification preferences for quiet hours, previews, and selected workspaces.

Out of scope:

- Mobile notification timing.
- Email notification settings.
- Slack AI recaps or summaries.
- Enterprise policy management.

Design file: `docs/activity-notifications-design.md`.

### 5. Rich Composer And Message Rendering

Assessment: yes, implement the common daily-use subset.

Official Slack has a deep composer and rich message surface. Conduit should support the parts users need constantly without building a full Block Kit authoring environment.

Scope:

- Multiline composer.
- Formatting controls for bold, italic, strike, code, quote, code block, ordered list, and bullet list.
- Emoji picker and custom emoji cache.
- Mention and channel autocomplete.
- Draft persistence in cache.
- Better rendering for edited/deleted messages, bot messages, attachments, files, reactions, and common Block Kit layouts.

Out of scope:

- Full WYSIWYG parity.
- Complete Block Kit interactivity.
- Workflow-trigger authoring from the composer.

Design file: `docs/composer-rendering-design.md`.

### 6. Workspace Navigation Modernization

Assessment: yes, but split into smaller subfeatures.

Navigation parity matters because users live in the sidebar. This should remain native GTK.

Scope:

- Multi-workspace switcher.
- Custom sidebar sections.
- Collapsible sections.
- Drag and drop section reordering where local/server support is clear.
- Presence and avatars.
- Muted state and per-conversation notification state.
- Slack Connect indicators and external organization labels.

Out of scope:

- Admin-only Slack Connect management.
- Full organization directory management.
- Enterprise Grid workspace migration tools.

Design file: `docs/workspace-navigation-design.md`.

### 7. Search, Files, And Directories

Assessment: yes, implement focused native surfaces.

Search and files are daily-use surfaces, but Conduit should not become a document-management clone.

Scope:

- Search result tabs for messages, files, people, and channels when API support is available.
- Search filters for channel/person/date/saved/thread where practical.
- File browser for recent files and conversation files.
- People and channel directories.
- Download/open file actions with cache-aware previews.

Out of scope:

- Slack AI answers.
- Enterprise search across external systems.
- Full document lifecycle management.

Design file: `docs/search-files-directories-design.md`.

### 8. Canvases, Lists, Workflows, Huddles, And Other Slack Product Surfaces

Assessment: mostly no for native implementation; use deep links and thin affordances.

These surfaces are large products inside Slack. A lightweight GNOME app should acknowledge them without badly cloning them.

Scope:

- Render references to canvases, lists, workflows, clips, and huddles clearly in messages.
- Open Slack deep links for creation, editing, and advanced interaction.
- Show huddle availability/status if a stable API is available.
- Avoid hiding important Slack content just because Conduit cannot edit it natively.

Out of scope:

- Native canvas editor.
- Native list/project-management app.
- Native workflow builder.
- Native huddle audio/video/screen sharing.
- Slack AI authoring or recap surfaces.

Design file: `docs/slack-product-surfaces-design.md`.

## Commit And Review Policy

For each Big Feature:

1. Reread this plan and the current repo state.
2. Assess whether the feature still fits Conduit's lightweight GNOME direction.
3. Save or update the feature design file.
4. Implement the smallest useful vertical slice.
5. Run focused tests plus relevant full validation.
6. Commit and push that Big Feature.
7. Reread this plan and the current repo state before starting the next Big Feature.

If a feature no longer makes sense, save that decision in its design file and skip implementation for that slice.
