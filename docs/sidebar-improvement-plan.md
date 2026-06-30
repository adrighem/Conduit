# Sidebar Improvement Plan

This plan describes how to evolve Conduit's workspace sidebar from a flat conversation list into a Slack-like, native GNOME navigation surface.

The goal is not a pixel clone of Slack. The goal is to preserve the information architecture users expect from Slack while keeping Conduit simpler, faster, and maintainable as an open source desktop client.

## Reference Behavior

Slack's current desktop/web sidebar is organized around these concepts:

- A navigation area for primary destinations such as Home, DMs, Activity, Files, Later, Tools, and More.
- A Home sidebar containing channels and direct messages divided into sections.
- Sidebar filtering and sorting controls.
- Conversation state indicators such as unread count, selected conversation, muted state, private channel state, and external connection state.
- Collapsible and reorderable sections, plus custom sections on paid plans.
- Keyboard navigation for moving between conversations and sections.

Primary references:

- https://slack.com/help/articles/212596808-Adjust-your-sidebar-preferences
- https://slack.com/help/articles/44134792609555-A-consolidated-set-of-tabs-for-Slack-on-desktop
- https://slack.com/help/articles/360043207674-Organize-your-sidebar-with-custom-sections
- https://slack.com/help/articles/201374536-Slack-keyboard-shortcuts

## Current State

The current sidebar is implemented mainly in `src/window.ui` and `src/window.rs`.

Current behavior:

- The workspace layout uses a horizontal `GtkPaned` with a fixed initial sidebar width of 280px.
- The sidebar contains a message search entry, a search button, refresh/saved/sign-out buttons, and one `GtkListBox`.
- All Slack conversations are sorted alphabetically and rendered into one flat list.
- Conversation rows are plain flat buttons with only a text label.
- DMs are resolved through `display_name_with_users`, but no row type, unread state, private state, or selected state is rendered.
- The current selected conversation is tracked in state, but the sidebar row is not visually marked.

Data already available:

- `SlackConversation.id`
- `name`
- `user`
- `is_channel`
- `is_group`
- `is_im`
- `is_mpim`
- `is_private`
- `is_archived`
- `unread_count`

Important constraint:

- Conduit currently obtains sidebar conversation data from `users.conversations`. The first implementation should use only data already available there unless a step explicitly calls for new API work.

## Product Goals

1. Make the sidebar scannable.
   Users should immediately distinguish channels, private channels, DMs, group DMs, unread conversations, and the selected conversation.

2. Match Slack's navigation model where it matters.
   The sidebar should have obvious primary destinations and grouped conversation sections.

3. Stay native.
   Use GTK widgets, Adwaita styling, keyboard focus behavior, accessible labels, and predictable GNOME layout conventions.

4. Keep `window.rs` maintainable.
   Sidebar grouping and row rendering should move out of the main window controller before adding more sidebar behavior.

5. Defer expensive or account-specific Slack features.
   Custom sections, full Slack Connect state, muted state, and profile avatars should be later phases unless the API/model data is already present.

## Non-Goals For The First Pass

- Pixel-perfect Slack theme replication.
- Custom sidebar sections synced with Slack.
- Drag and drop reordering.
- Workspace switcher for multiple Slack workspaces.
- Activity, Files, Tools, Canvases, Lists, or enterprise-specific views.
- Full unread synchronization semantics beyond fields already returned by Slack.

## Proposed Architecture

Add a sidebar module:

- `src/sidebar.rs`

Responsibilities:

- Convert `Vec<SlackConversation>` plus user-name cache plus selected channel into a sidebar view model.
- Group conversations into sections.
- Sort conversations consistently.
- Render GTK rows and section headers.
- Expose callbacks for selecting conversations.
- Keep row state updates localized.

Suggested view model:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversationKind {
    PublicChannel,
    PrivateChannel,
    DirectMessage,
    GroupDirectMessage,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarSectionKind {
    Unreads,
    Channels,
    DirectMessages,
    GroupDirectMessages,
}

#[derive(Debug, Clone)]
struct SidebarRowModel {
    id: String,
    title: String,
    kind: ConversationKind,
    unread_count: u64,
    selected: bool,
    private: bool,
}

#[derive(Debug, Clone)]
struct SidebarSectionModel {
    kind: SidebarSectionKind,
    title: String,
    rows: Vec<SidebarRowModel>,
    collapsed: bool,
}
```

Initial grouping rules:

- `Unreads`: conversations with `unread_count > 0`.
- `Channels`: public and private non-DM channels.
- `Direct messages`: one-to-one IMs.
- `Group direct messages`: MPIM conversations.

Duplication rule:

- For the first pass, unread conversations should appear in both `Unreads` and their normal section. This matches user expectations that unread is a quick access section, not the only location for that conversation.

Sorting rule:

- Sort unread rows by unread count descending, then title.
- Sort regular section rows alphabetically by resolved display title.

## Implementation Steps

### Step 1: Extract Sidebar View Models

Goal:

- Separate grouping/sorting from GTK widget construction.

Changes:

- Add `src/sidebar.rs`.
- Add `ConversationKind`, `SidebarRowModel`, and `SidebarSectionModel`.
- Add a pure function like `build_sidebar_sections(conversations, user_names, selected_channel)`.
- Register the module in `src/main.rs`.

Testing:

- Unit test channel, private channel, DM, and MPIM classification.
- Unit test alphabetical sorting.
- Unit test selected row marking.
- Unit test unread conversations appearing in `Unreads`.

Verification:

- `cargo fmt --check`
- `cargo test sidebar`
- Full `cargo test`

### Step 2: Render Sectioned Sidebar

Goal:

- Replace the flat list with section headers and richer rows.

Changes:

- Update `render_conversations` in `src/window.rs` to call the sidebar builder.
- Render section headers as lightweight rows or labels.
- Render row content as a horizontal `GtkBox`:
  - type icon or textual marker
  - conversation title
  - unread badge when `unread_count > 0`
- Use existing GTK icon names where possible.
- Keep the existing click behavior that calls `select_conversation`.

Testing:

- Unit tests remain in `src/sidebar.rs`.
- Add focused tests only for pure behavior, not GTK widget internals.

Manual verification:

- Launch Conduit.
- Confirm conversations are grouped into sections.
- Confirm DMs no longer visually blend with channels.
- Confirm clicking every row type opens the correct conversation.
- Confirm empty sections are hidden.
- Confirm a workspace with no conversations still shows the existing placeholder.

### Step 3: Add Selected Row State

Goal:

- Make the current conversation visible in the sidebar.

Changes:

- Add a CSS class or row state for selected sidebar rows.
- Ensure `render_conversations` marks the selected channel when user names are loaded and the list rerenders.
- Do not rely only on `message_title`.

Testing:

- Unit test selected row state in the sidebar view model.
- Manual test selection persists visually after DM display names resolve.
- Manual test selection persists after refresh.

Verification:

- Select a channel.
- Trigger refresh.
- Confirm the selected row remains highlighted.
- Select a DM before the display name is loaded.
- Confirm the highlight remains after the DM name changes from user ID to display name.

### Step 4: Rework Sidebar Top Navigation

Goal:

- Separate primary navigation from account/workspace actions.

Changes:

- Replace the current row of refresh/saved/sign-out buttons with a clearer top area:
  - workspace label/menu
  - `Home`
  - `Saved`
  - optional `Search`
  - refresh as a secondary action
  - sign out in menu, not as a primary sidebar button
- Keep full message search available, but do not make it look like conversation filtering unless it filters conversations.

Testing:

- Existing saved/search/sign-out callbacks still work.
- Manual check that keyboard focus order is sensible.

Manual verification:

- Click `Saved`; saved items load.
- Run a message search; search results load.
- Refresh conversations; rows update.
- Sign out from the new menu location.
- Confirm no action regressed from the old button row.

### Step 5: Add Sidebar Conversation Filter

Goal:

- Make it quick to find a channel or DM without running global Slack message search.

Changes:

- Repurpose the sidebar `GtkSearchEntry` for local conversation filtering, or add a separate conversation filter field.
- Filter all section rows by resolved title.
- Show a placeholder when no conversation matches.
- Keep global message search as a separate view/action.

Testing:

- Unit test filtering by channel name.
- Unit test filtering by resolved DM display name.
- Unit test filtering preserves selected state.
- Unit test filtering hides empty sections.

Manual verification:

- Type a public channel name.
- Type a private channel name.
- Type a DM display name.
- Clear the filter.
- Confirm no network request is made for local sidebar filtering.

### Step 6: Collapsible Sections

Goal:

- Reduce sidebar noise in large workspaces.

Changes:

- Add section expand/collapse controls.
- Store collapsed state in memory first.
- Optionally persist collapsed state later through GSettings.

Testing:

- Unit test collapsed sections omit rows from render input.
- Manual test collapse/expand with keyboard and mouse.
- Manual test collapsed state survives conversation rerender in the same session.

Verification:

- Collapse `Channels`.
- Refresh conversations.
- Confirm `Channels` stays collapsed.
- Expand `Channels`.
- Confirm row click still works.

### Step 7: Persist Sidebar Preferences

Goal:

- Preserve user choices without introducing Slack account coupling.

Candidate preferences:

- collapsed sections
- sidebar width
- show/hide empty sections
- sort mode

Changes:

- Add keys to `data/eu.vanadrighem.conduit.gschema.xml`.
- Read/write preferences through the existing GTK/GSettings patterns.

Testing:

- Unit test default preference interpretation where practical.
- Manual test preferences persist across app restart.
- Build verification for schema changes.

Verification:

- `meson setup _build --reconfigure`
- `meson compile -C _build`
- `meson test -C _build`
- Restart the app and confirm sidebar state persists.

## Visual Direction

Conduit should feel like a native GNOME app, not Slack in a WebView.

Recommended visual treatment:

- Keep the sidebar calm and dense.
- Use compact rows with stable height.
- Use subtle badges for unread count.
- Use a clear selected background.
- Use section headers with small, muted labels and disclosure affordances.
- Avoid excessive color, gradients, avatars, or Slack-specific branding.

Suggested row layout:

```text
[icon] # channel-name                         [3]
[icon] person name
[icon] private-channel                       [12]
```

Icon meaning:

- public channel: hash-style marker or channel icon
- private channel: lock
- DM: person/status marker
- group DM: group marker

## Accessibility Requirements

- Rows must be keyboard reachable.
- Section collapse controls must have accessible labels.
- Unread badges must be represented in the accessible label, not only visually.
- Selected row state must be exposed through GTK row selection or an equivalent accessible state.
- Focus order should move from workspace navigation to sections to rows, then to message content.

Manual accessibility verification:

- Navigate the sidebar with keyboard only.
- Confirm visible focus is never lost.
- Confirm selected row and unread counts are understandable without color alone.
- Confirm row labels are not truncated into ambiguity at the default 280px width.

## Test Plan Summary

Run after every sidebar phase:

- `cargo fmt --check`
- `cargo test`

Run after UI or resource changes:

- `meson setup _build --reconfigure`
- `meson compile -C _build`
- `meson test -C _build`

Manual smoke test after each user-visible phase:

- Connect to a real Slack workspace.
- Confirm conversations load.
- Open a public channel.
- Open a private channel if available.
- Open a DM.
- Open a group DM if available.
- Confirm unread badges render for conversations with unread counts.
- Confirm selected row state is visible.
- Confirm refresh does not lose selection.
- Confirm saved items still load.
- Confirm message search still works.
- Confirm sign out still works.

Regression checks:

- Thread opening from the message view still works.
- Message posting still targets the selected conversation.
- File upload still targets the selected conversation.
- DM user-name loading still rerenders without losing selection.
- Notifications still use the correct conversation title.

## Acceptance Criteria

The first sidebar improvement release is complete when:

- Conversations are grouped into at least `Unreads`, `Channels`, `Direct messages`, and `Group direct messages`.
- Rows show type, title, unread count, and selected state.
- Local sidebar filtering works without invoking Slack message search.
- Existing saved/search/refresh/sign-out workflows continue to work.
- `cargo test` passes.
- Meson build and tests pass.
- A real Slack workspace smoke test passes.

## Risks And Mitigations

Risk: `window.rs` grows harder to maintain.

- Mitigation: move pure sidebar modeling and row construction into `src/sidebar.rs` before adding filter/collapse behavior.

Risk: Slack API fields are incomplete for exact web-client parity.

- Mitigation: implement only states backed by current model data first. Add API fields deliberately when needed.

Risk: unread semantics differ from Slack web.

- Mitigation: label the first pass as best-effort unread rendering from `users.conversations`; verify against a real workspace before expanding behavior.

Risk: GTK row rendering becomes hard to unit test.

- Mitigation: unit test pure sidebar models and keep manual verification for widget rendering.

Risk: sidebar search conflicts with global message search.

- Mitigation: name and place controls clearly. Local conversation filtering should not look like global Slack search.
