# Sidebar Design

> **Status: Historical design record.** This document is preserved as implementation history and may not describe the current repository. See [README.md](../README.md) for current capabilities and [conductor/tech-stack.md](../conductor/tech-stack.md) for current architecture.

This document records an earlier implementation of the Conduit workspace sidebar and its channel/user list. The sidebar is native GTK4 UI; it does not use a web embed.

`docs/sidebar-improvement-plan.md` is related historical planning context; both documents are preserved for reference.

## Source Files

- `src/window.ui`: Defines the workspace sidebar shell and static controls.
- `src/window.rs`: Renders the pure sidebar model into GTK rows, owns row activation, and keeps the row-action map.
- `src/sidebar.rs`: Builds the pure sidebar list model, including filtering, placeholder state, grouping, sorting, row state, accessibility labels, unread badges, switcher items, and conversation type classification.
- `src/models.rs`: Defines `SlackConversation`, including the Slack fields used by the sidebar.
- `src/runtime.rs`: Prioritizes and rotates bounded unread-state enrichment work.
- `src/store.rs`: Persists the unread-refresh queue across runs.

## 1.0 Scope

Included for 1.0:

- Native GTK workspace sidebar.
- Workspace identity header.
- Home and Later primary navigation.
- Conversation refresh.
- Workspace/account menu with sign out.
- Conversation filtering by display title and Slack conversation ID.
- Unread-only conversation filtering.
- An all-conversations override for browsing the complete loaded catalog.
- Sections for unreads, channels, direct messages, and unknown conversations.
- Native selectable and activatable conversation rows.
- Unread counts, selected state, type icons, and accessible row labels.
- Empty, loading, filtered-empty, and load-error states.

Excluded from 1.0 unless explicitly revisited:

- Slack custom sections.
- Collapsible sections.
- Drag and drop reordering.
- Muted state.
- Slack Connect or external organization indicators.
- Avatars and presence badges.
- Full unread synchronization beyond conversation list and lightweight history hints.

Supporting more than one connected Slack workspace is outside Conduit's product scope, not a deferred post-1.0 sidebar feature.

## Sidebar Shell

The sidebar is the start child of the workspace `GtkPaned`. It is a vertical `GtkBox` with a 280 pixel width request and these elements:

- Workspace title label named `workspace_title_label`.
  - Shows the authenticated Slack team name when available.
  - Falls back to `Workspace` before workspace metadata is available.
- Refresh icon button using `view-refresh-symbolic`.
- Workspace menu button using `open-menu-symbolic`.
  - Contains the sign-out action.
  - Sign out is intentionally not a top-level sidebar button because it is account/session management rather than primary navigation.
- Primary navigation buttons:
  - `Home`, using `go-home-symbolic`.
  - `Later`, using `starred-symbolic`.
- Conversation filter entry with placeholder text `Filter conversations`.
- Unread-only toggle button using `mail-unread-symbolic`.
- All-conversations toggle button using `view-list-symbolic`.
- Scrollable `GtkListBox` named `conversation_list`, styled with `navigation-sidebar`.
- Status footer label named `workspace_status_label`, used for transient progress and errors.

The conversation list is a native GTK `ListBox`; every section header and conversation row is created in Rust.

## Sidebar States

Sidebar list states are resolved in this order:

| State | Trigger | List Content | Footer |
| --- | --- | --- | --- |
| Loading | Conversations are being fetched and no cached conversations are available | `Loading conversations` | Current runtime status |
| Load error | Conversation loading failed and no cached conversations are available | `Could not load conversations` | Error text |
| Empty workspace | Conversation loading succeeded with zero conversations | `No conversations` | Current runtime status |
| Filtered empty | Conversations exist, but the filter matches none | `No matching conversations` | Current runtime status |
| Populated | Sections contain at least one row | Section headers and conversation rows | Current runtime status |

If conversation refresh fails while existing conversations are still available, the list remains populated and the error appears in the status footer. The sidebar should not replace useful cached navigation with a full error state unless there is nothing else to show.

## Primary Navigation

`Home` returns to the currently selected conversation when one exists. If no conversation is selected, it shows the normal `Select a conversation` placeholder.

`Later` opens saved items and clears visual conversation selection while the main pane is not displaying a conversation.

Conversation row selected state is shown only when `MainMessageView::Conversation` is active. Search and Later do not keep a conversation visually selected in the sidebar.

## Conversation Sections

`src/sidebar.rs` converts Slack conversations into sections. Empty sections are omitted.

Sections are rendered in this order:

1. `Unreads` when enabled in Preferences
2. `Channels`
3. `Direct messages`
4. `Other`

One-to-one and group direct messages share the `Direct messages` section and are sorted together. Their distinct conversation kinds are retained for row icons, accessibility labels, and Slack-specific behavior.

Unread conversations can be duplicated: when the Preferences setting `Show Unreads section` is enabled, they appear in `Unreads` and also in their normal section. The currently selected conversation also remains in `Unreads` after opening it clears its unread state. This keeps `Unreads` as a shortcut section rather than moving the conversation out of its normal location. The setting is disabled by default, so unread conversations stay in their normal sections unless the shortcut section is explicitly enabled.

The unread-only filter likewise retains the currently selected conversation after it becomes read, so opening an item does not immediately remove it from the filtered sidebar.

The `Unreads` section header includes its total row count, for example `Unreads (3)`. Other section headers use their plain title.

Section headers are non-selectable disclosure controls. Clicking or keyboard-activating a header collapses or expands that section, and the chevron shows its current state. Collapse state is kept for the current application session and survives sidebar refreshes and filter changes. Collapsing `Unreads` hides only its shortcut rows; copies in their regular sections remain available.

## Conversation Types

Each `SlackConversation` is classified as one of:

- `PublicChannel`
- `PrivateChannel`
- `DirectMessage`
- `GroupDirectMessage`
- `Unknown`

Classification uses Slack conversation flags:

- `is_im` maps to `DirectMessage`.
- `is_mpim` maps to `GroupDirectMessage`.
- `is_private` or `is_group` maps to `PrivateChannel`.
- `is_channel` maps to `PublicChannel`.
- Anything else maps to `Unknown`.

Archived conversations are filtered out before section construction.

Channels remain visible in the sidebar. Direct messages, including group direct messages, are shown when selected, unread, or marked active by Slack. Up to 20 additional read DMs are retained from Slack's latest-message and read-cursor activity hints. Deleted-user and dormant DMs are excluded from those active/history sets unless selected or unread. **Show All Conversations**, switchers, and pickers retain the complete non-archived loaded catalog.

## Row Model

Each rendered conversation row is based on `SidebarRowModel`:

```rust
pub struct SidebarRowModel {
    pub id: String,
    pub title: String,
    pub kind: ConversationKind,
    pub unread_count: u64,
    pub selected: bool,
    pub private: bool,
}
```

The `title` is resolved with `SlackConversation::display_name_with_users`, so direct messages use loaded user display names when available. Channels retain their Slack-style `#name` display title. Missing direct-message user names degrade to `DM <user_id>`.

## Sorting

Regular sections are sorted alphabetically by display title, with a normalized sort key that strips a leading `#` before comparison.

`Unreads` is sorted by:

1. Unread count descending.
2. Normalized title ascending.
3. Conversation ID ascending.

The ID tie-breaker keeps ordering deterministic when titles match.

## Filtering

The sidebar filter entry rerenders the list on search changes. The unread-only and all-conversations toggles rerender the list on state changes. **Show All Conversations** expands the source set before text and unread-only filters are applied, so those filters continue to compose normally.

Text filtering matches:

- Resolved display title, case-insensitive.
- Slack conversation ID, case-insensitive.

Unread-only filtering keeps conversations that have unread activity according to either numeric unread counts or boolean unread hints such as `has_unreads`. Numeric display counts and boolean unread state are intentionally separate so a channel can render as unread without showing a count badge.

Slack's aggregate unread fields remain the raw synchronization model. Once Conduit has observed
message-level attention for a conversation, the sidebar uses that local projection instead:
ordinary non-self messages increment it, while membership lifecycle noise is recorded as observed
without becoming unread. Later increases in a coarse raw count cannot recreate filtered noise or
inflate the local count. An authoritative raw read state can still clear the local projection.

The local count is persisted across restarts until a read action or authoritative raw read state
clears it. Each conversation retains only its 512 most recently recorded message identities for
replay and history-reconciliation checks. A retained repeat is
`already_observed`; a message whose timestamp is not newer than the durable local read cursor is
`at_or_before_read_cursor`. Neither outcome increments the local projection. Older identities can
age out of the bounded replay window.

On startup, browser-credential sessions first attempt Slack Web's private `client.userBoot`/`client.counts` sequence to establish a bulk unread baseline. The integration is restricted to the authenticated workspace origin, validates the observed response shape, and fails closed because Slack does not publish a stable contract for these methods. Missing records remain unknown rather than being treated as read.

OAuth sessions, private-endpoint failures, and conversations omitted from that snapshot use the bounded fallback: the runtime refreshes at most 30 distinct conversations using conversation info and lightweight latest-message checks instead of crawling the entire workspace. Unknown active DMs and other DMs whose unread state is unknown are prioritized so hidden unread conversations can be discovered. The full candidate order is persisted and rotated after each batch; newly discovered priority candidates enter at the front, while deferred candidates retain their order and cannot be permanently starved.

Filtered results preserve normal section grouping and hide empty sections. When unread-only filtering is active or the `Show Unreads section` preference is disabled, the `Unreads` shortcut section is omitted so the same unread conversations do not appear twice in the list. Fuzzy search and matched-text highlighting are not part of the 1.0 sidebar.

If no conversations exist, the list shows `No conversations`. If the filter removes every conversation, it shows `No matching conversations`.

## Row Rendering

Each conversation row is a selectable and activatable `GtkListBoxRow`. The row contains a horizontal `GtkBox` with:

- A conversation type icon.
- An ellipsized title label.
- An unread count label only when Slack provides a non-zero display count.

Icons:

- Public channel: `channel-public-symbolic`
- Private channel: `channel-secure-symbolic`
- Direct message: `avatar-default-symbolic`
- Group direct message: `system-users-symbolic`
- Unknown conversation: `dialog-question-symbolic`

Rows with unread messages apply explicit bold Pango weight and the native `heading` CSS class to the title, including badge-less unread channels. Read rows apply explicit normal title weight. Unread counts remain visually emphasized with the `heading` CSS class when Slack provides a display count. Selected conversations use the native `GtkListBox` selected row state, not `suggested-action`.

Unread badge labels are capped at `99+` for counts above 99. Row titles ellipsize and the sidebar width remains stable at the shell level.

If the same selected conversation appears in both `Unreads` and its normal section, the first matching visible row is selected. This preserves native single-selection semantics while keeping unread duplication.

## Row Activation

Rows are activated through the `GtkListBox::row-activated` signal on `conversation_list`; the row is the interactive object. There is no nested button inside a conversation row.

The list sets `activate-on-single-click` to `True`, so a normal mouse click activates the channel or DM. Keyboard activation still uses native list behavior.

`src/window.rs` registers a `SidebarRowAction` for each conversation row and a section toggle action for each header, keyed by the rendered `GtkListBoxRow` index. Placeholder rows are not registered. Activating a header toggles its collapsed state and rerenders the keyed list without navigating away from the current conversation.

Activating a conversation row calls `select_conversation(channel_id, title)`.

That function:

- Stores the selected channel ID.
- Clears the selected thread.
- Switches the main view state to `Conversation`.
- Updates the message title.
- Clears current thread UI state.
- Rerenders the sidebar so native selection is updated.
- Shows a loading placeholder in the message web view while preserving `Conversation` as the active main view.
- Sends `RuntimeCommand::LoadHistory`.

When history loads, `populate_history` sets the selected channel again, updates the message title using the latest resolved display name, rerenders the sidebar, and loads the message HTML.

## Accessibility And Input

Rows expose accessible labels in this form:

```text
Public channel: #general, 3 unread, selected
```

The type label comes from `ConversationKind::accessible_name`. The unread and selected suffixes are included only when relevant.

Icon-only controls have tooltips:

- Refresh: `Refresh Conversations`
- Workspace menu: `Workspace Menu`
- Unread-only filter: `Show Unread Conversations`
- All-conversations filter: `Show All Conversations`

Expected keyboard order:

1. Sidebar header/menu controls.
2. Home and Later primary navigation.
3. Conversation filter, unread-only toggle, and all-conversations toggle.
4. Conversation list.
5. Main message area.

Section headers are focusable and expose action-oriented labels such as `Collapse Channels` or `Expand Channels`. Conversation rows remain selectable and activatable through native list row behavior.

## Test Coverage

Sidebar model coverage lives in `src/sidebar.rs` and verifies conversation classification, list placeholders, filtering, section grouping and collapsing, sorting, unread duplication, unread badge labels, selected state, active/history DM bounds, the all-conversations override, switcher items, section display titles, and accessible labels.

Window/controller coverage in `src/window.rs` verifies that rendered row actions preserve the conversation ID and resolved title, unregistered rows do not activate, section state toggles independently with appropriate accessibility labels, and unread rows map to bold title weight.

Slack client, runtime, and store coverage verifies the guarded browser bootstrap request shape, schema-drift rejection, badge-less unread snapshots, monotonic cursor persistence, DM-aware fallback priority, the 30-conversation hard limit, fair queue rotation, and ordered persistence across restarts.

## Native UI Boundary

The sidebar is fully GTK4/libadwaita-native. WebKit is still used for message and thread rendering elsewhere in the app, but not for sidebar navigation, filtering, section headers, row icons, unread badges, row activation, or selection state.

## Current Limits

The sidebar currently uses member-scoped conversation data from `users.conversations`, a browser-session-only private unread baseline when available, bounded conversation-detail enrichment, lightweight activity hints, and the user-name cache. By default it hides dormant, deleted-user, and inactive direct-message entries unless they are unread or currently selected. Slack's activity fields are hints rather than a canonical recent-conversation list, so the history fallback is deliberately capped. The `Show All Conversations` toggle exposes the full non-archived loaded set for older or low-activity conversations. Future features should be added only when the data model and Slack API usage support them cleanly.

Known limits:

- No Slack custom section sync.
- No drag and drop reordering.
- No muted state.
- No Slack Connect or external organization indicators.
- No avatars or presence badges.
- One connected Slack workspace by design.
- No full unread synchronization beyond conversation list and lightweight history hints.
