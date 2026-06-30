# Sidebar 1.0 Update Plan

This plan covers near-term sidebar UI improvements and broader 1.0 readiness work that affects the sidebar experience. Every implementation step includes a documentation update so `docs/sidebar-design.md` stays aligned with the app.

Status: implemented in the initial 1.0 sidebar pass. Keep this file as the implementation checklist and use `docs/sidebar-design.md` as the current design contract.

Follow-up on 2026-06-19:

- Conversation row activation now uses `GtkListBox::row-activated` with `activate-on-single-click=True`.
- Conversation rows register explicit row actions by list row index; headers and placeholders remain inert.
- Selecting a channel or DM keeps `MainMessageView::Conversation` active while the message web view shows the loading placeholder.
- Added controller tests for row-action lookup and sidebar selected-state policy.
- Updated `docs/sidebar-design.md` with the activation path and test coverage.

## Goals

- Make the sidebar feel like native GNOME navigation, not a stack of command buttons.
- Keep the Slack information architecture recognizable without copying Slack pixel-for-pixel.
- Preserve the current pure sidebar model and tests as the place for grouping and sorting logic.
- Reduce 1.0 support risk around navigation, account state, accessibility, empty states, and Slack setup expectations.

## Non-Goals

- Slack custom sections synced from Slack.
- Drag and drop sidebar reordering.
- Multi-workspace switching.
- Full Slack presence or avatar support unless the data model and API usage are expanded deliberately.

## Step 1: Clarify Sidebar Header And Account Actions

Problem:

- The sidebar header is labeled `Home` while there is also a `Home` nav button.
- Sign out is exposed as a top-level sidebar header button, which makes a session-management action feel like primary navigation.

Changes:

- Replace the static `Home` header with workspace/team identity when available.
- Use a fallback such as `Workspace` before auth/team data is loaded.
- Move sign out into a workspace/account menu or the existing app menu.
- Keep refresh available, but evaluate whether it belongs in the header or near the conversation list.

Design documentation update:

- Update `docs/sidebar-design.md` to describe the header as workspace identity rather than Home.
- Document where sign out lives and why it is not primary navigation.
- Record any fallback labels used before workspace metadata is available.

Validation:

- Confirm the header and primary nav no longer duplicate `Home`.
- Confirm sign out remains discoverable but is not visually competing with navigation.

## Step 2: Use Native Row Selection Semantics

Problem:

- Conversation rows are non-selectable `GtkListBoxRow`s containing flat `GtkButton`s.
- Selected conversations use `suggested-action`, which reads like a primary command rather than navigation state.

Changes:

- Make conversation rows activatable/selectable `GtkListBoxRow`s where practical.
- Move activation handling to row activation instead of a nested button click.
- Connect activation at the `GtkListBox` level and enable single-click activation for pointer users.
- Replace `suggested-action` selected styling with native list selection or a dedicated navigation selected class.
- Preserve accessible labels/tooltips for conversation type and title.

Design documentation update:

- Update `docs/sidebar-design.md` row rendering and row activation sections.
- Document the chosen selection style and keyboard behavior.
- Document whether unread duplicate rows both reflect selected state.

Validation:

- Keyboard navigation can move through conversation rows predictably.
- Pressing Enter/Space opens the focused row.
- Clicking a conversation row opens that conversation in the message pane.
- Selected state is visually distinct without looking like a call-to-action button.

## Step 3: Define Empty, Loading, And Error States

Problem:

- The current design documents `No conversations` and `No matching conversations`, but not loading or refresh failure states.
- 1.0 needs predictable behavior when Slack data is slow, unavailable, or partially loaded.

Changes:

- Add a `Loading conversations` state while conversations are being fetched.
- Keep `No conversations` for valid empty results.
- Keep `No matching conversations` for filters.
- Add a compact sidebar error state or footer message for refresh/auth failures.
- Avoid replacing the whole sidebar with an error page when only conversation refresh fails.

Design documentation update:

- Add a sidebar state table to `docs/sidebar-design.md`.
- Document state precedence: loading, error, empty, filtered empty, populated.
- Document which states appear in the list and which appear in the status footer.

Validation:

- Simulate or force loading/error paths where possible.
- Confirm filtering an empty result differs from a truly empty workspace.

## Step 4: Improve Section Header Behavior

Problem:

- Section headers are static labels inside list rows.
- There is no documented behavior for collapse, keyboard skipping, section counts, or section visibility beyond omitting empty sections.

Changes:

- Keep sections non-collapsible for the first 1.0 pass unless a clear need appears.
- Make section headers visually distinct but lightweight.
- Consider showing unread counts on the `Unreads` section header if it improves scan speed.
- Ensure headers are not focus traps and do not behave like selectable conversations.

Design documentation update:

- Document section header role, focus behavior, and styling.
- Document whether section counts are shown.
- Keep the future collapsible-section behavior in `Current Limits` unless implemented.

Validation:

- Keyboard focus should skip non-interactive headers or handle them consistently.
- Empty sections remain hidden.

## Step 5: Tighten Conversation Row Information Design

Problem:

- The row currently has type icon, title, and unread count.
- It does not document how long names, private state, unread prominence, or unknown conversation types should feel visually.

Changes:

- Keep row height compact and stable.
- Ensure titles ellipsize and never resize the sidebar.
- Decide whether private channels need a separate lock-style treatment beyond `channel-secure-symbolic`.
- Keep unread counts visually secondary to the selected state.
- Define a maximum unread label strategy if Slack returns large counts.

Design documentation update:

- Add row layout constraints to `docs/sidebar-design.md`.
- Document overflow behavior for long titles and large unread counts.
- Document private/public visual distinction.

Validation:

- Test long channel names, long DM names, unread counts, and selected rows.
- Confirm rows still scan well at 280px width.

## Step 6: Align Filtering With User Expectations

Problem:

- Filtering currently matches title and Slack ID.
- It is not documented whether sections should remain, counts should update, or matched text should be highlighted.

Changes:

- Keep filtering simple for 1.0: case-insensitive title and ID matching.
- Keep section grouping in filtered results.
- Consider clearing the filter when switching workspaces in future multi-workspace support.
- Do not add match highlighting unless it can be done cleanly with native GTK labels.

Design documentation update:

- Expand the filtering section in `docs/sidebar-design.md`.
- Document that filtered results preserve sections and hide empty sections.
- Document non-goals such as fuzzy search and match highlighting.

Validation:

- Verify channel, DM, and ID matches.
- Verify filtered `Unreads` behavior is understandable.

## Step 7: Accessibility And Input Readiness

Problem:

- The design references tooltips and accessible type labels but does not yet define the full keyboard and screen-reader behavior.

Changes:

- Ensure rows expose meaningful accessible names such as `Public channel, #general, 3 unread`.
- Ensure icon-only controls have clear tooltips and accessible labels.
- Confirm tab order: header/menu controls, primary nav, filter, conversation list, message area.
- Define expected keyboard behavior for row activation and filter focus.

Design documentation update:

- Add an accessibility section to `docs/sidebar-design.md`.
- Document accessible naming rules for rows and icon buttons.
- Document keyboard traversal expectations.

Validation:

- Manual keyboard-only pass.
- Inspect GTK accessible names where practical.

## Step 8: 1.0 Release Readiness For Sidebar Data

Problem:

- The sidebar depends on `users.conversations`, cached user names, and `unread_count`.
- 1.0 should clearly communicate which Slack-like features are intentionally not present.

Changes:

- Keep custom sections, presence, avatars, Slack Connect badges, and full unread sync out of 1.0 unless they become cheap and reliable.
- Make sure missing user names degrade gracefully to `DM <user_id>`.
- Keep the pure sidebar grouping tests as release-blocking coverage.
- Add tests if new state behavior or row model fields are introduced.

Design documentation update:

- Update `Current Limits` in `docs/sidebar-design.md`.
- Add a short `1.0 Scope` subsection listing included and excluded sidebar features.
- Link back to `docs/sidebar-improvement-plan.md` only for historical planning context.

Validation:

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test sidebar`
- Full `cargo test`
- `meson compile -C _build`
- `meson test -C _build`

## Proposed Implementation Order

1. Header/account action cleanup.
2. Native row activation and selected-state styling.
3. Empty/loading/error state definitions.
4. Section header and row information polish.
5. Filtering documentation and behavior cleanup.
6. Accessibility pass.
7. Final 1.0 scope and limits update.

Each step should be merged with its corresponding `docs/sidebar-design.md` update in the same change.
