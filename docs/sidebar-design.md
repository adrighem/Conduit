# Sidebar Design

This document describes the implemented Conduit workspace sidebar and its channel/user list. The sidebar is native GTK4 UI; it does not use a web embed.

`docs/sidebar-improvement-plan.md` is historical planning context. This document is the current design contract.

## Source Files

- `src/window.ui`: Defines the workspace sidebar shell and static controls.
- `src/window.rs`: Renders sidebar states, sections, and rows; filters conversations; owns the GTK row-activation signal and row-action map.
- `src/sidebar.rs`: Builds the pure sidebar view model for grouping, sorting, row state, accessibility labels, unread badges, and conversation type classification.
- `src/models.rs`: Defines `SlackConversation`, including the Slack fields used by the sidebar.

## 1.0 Scope

Included for 1.0:

- Native GTK workspace sidebar.
- Workspace identity header.
- Home and Later primary navigation.
- Conversation refresh.
- Workspace/account menu with sign out.
- Conversation filtering by display title and Slack conversation ID.
- Unread-only conversation filtering.
- Sections for unreads, channels, direct messages, group direct messages, and unknown conversations.
- Native selectable and activatable conversation rows.
- Unread counts, selected state, type icons, and accessible row labels.
- Empty, loading, filtered-empty, and load-error states.

Excluded from 1.0 unless explicitly revisited:

- Slack custom sections.
- Collapsible sections.
- Drag and drop reordering.
- Multi-workspace switching.
- Muted state.
- Slack Connect or external organization indicators.
- Avatars and presence badges.
- Full unread synchronization beyond conversation-level unread fields returned by `users.conversations`.

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
- Scrollable `GtkListBox` named `conversation_list`, styled with `navigation-sidebar`.
- Status footer label named `workspace_status_label`.

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

1. `Unreads`
2. `Channels`
3. `Direct messages`
4. `Group direct messages`
5. `Other`

Unread conversations are duplicated: they appear in `Unreads` and also in their normal section. This keeps `Unreads` as a shortcut section rather than moving the conversation out of its normal location.

The `Unreads` section header includes its row count, for example `Unreads (3)`. Other section headers use their plain title.

Section headers are non-selectable, non-activatable, and non-focusable. They are static grouping labels, not controls.

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

The sidebar filter entry rerenders the list on search changes. The unread-only toggle rerenders the list on state changes.

Text filtering matches:

- Resolved display title, case-insensitive.
- Slack conversation ID, case-insensitive.

Unread-only filtering keeps conversations that have unread activity according to the modeled `unread_count` field or extra Slack conversation fields whose names contain `unread`, such as `unread_count_display` or `has_unreads`. This catches unread state variants while the app is still validating Slack's conversation payloads.

Filtered results preserve normal section grouping and hide empty sections. When unread-only filtering is active, the `Unreads` shortcut section is omitted so the same unread conversations do not appear twice in an already-filtered list. Fuzzy search and matched-text highlighting are not part of the 1.0 sidebar.

If no conversations exist, the list shows `No conversations`. If the filter removes every conversation, it shows `No matching conversations`.

## Row Rendering

Each conversation row is a selectable and activatable `GtkListBoxRow`. The row contains a horizontal `GtkBox` with:

- A conversation type icon.
- An ellipsized title label.
- An unread count label when `unread_count > 0`.

Icons:

- Public channel: `channel-public-symbolic`
- Private channel: `channel-secure-symbolic`
- Direct message: `avatar-default-symbolic`
- Group direct message: `system-users-symbolic`
- Unknown conversation: `dialog-question-symbolic`

Rows with unread messages apply the `heading` CSS class to the title and unread count. Selected conversations use the native `GtkListBox` selected row state, not `suggested-action`.

Unread badge labels are capped at `99+` for counts above 99. Row titles ellipsize and the sidebar width remains stable at the shell level.

If the same selected conversation appears in both `Unreads` and its normal section, the first matching visible row is selected. This preserves native single-selection semantics while keeping unread duplication.

## Row Activation

Rows are activated through the `GtkListBox::row-activated` signal on `conversation_list`; the row is the interactive object. There is no nested button inside a conversation row.

The list sets `activate-on-single-click` to `True`, so a normal mouse click activates the channel or DM. Keyboard activation still uses native list behavior.

`src/window.rs` registers a `SidebarRowAction` for each conversation row, keyed by the rendered `GtkListBoxRow` index. Section headers and placeholder rows are not registered, so activating them has no navigation effect.

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

Expected keyboard order:

1. Sidebar header/menu controls.
2. Home and Later primary navigation.
3. Conversation filter and unread-only toggle.
4. Conversation list.
5. Main message area.

Section headers are skipped because they are not focusable. Conversation rows are selectable and activatable through native list row behavior.

## Test Coverage

Sidebar model coverage lives in `src/sidebar.rs` and verifies conversation classification, section grouping, sorting, unread duplication, unread badge labels, selected state, section display titles, and accessible labels.

Window/controller coverage in `src/window.rs` verifies that rendered row actions preserve the conversation ID and resolved title, unregistered rows do not activate, and the sidebar only shows a selected conversation while the main pane is actually in `Conversation` mode.

## Native UI Boundary

The sidebar is fully GTK4/libadwaita-native. WebKit is still used for message and thread rendering elsewhere in the app, but not for sidebar navigation, filtering, section headers, row icons, unread badges, row activation, or selection state.

## Current Limits

The sidebar currently uses member-scoped conversation data from `users.conversations` and the user-name cache. Future features should be added only when the data model and Slack API usage support them cleanly.

Known limits:

- No Slack custom section sync.
- No collapsible sections.
- No drag and drop reordering.
- No muted state.
- No Slack Connect or external organization indicators.
- No avatars or presence badges.
- No multiple workspace switching.
- No full unread synchronization beyond conversation-level unread fields returned by `users.conversations`.
