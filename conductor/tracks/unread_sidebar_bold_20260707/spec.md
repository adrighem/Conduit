# Track Spec: Unread Sidebar Bold Text

## Goal
Ensure sidebar channels, DMs, and group DMs with unread messages render their conversation title in bold, while conversations without unread messages render with normal-weight title text.

## Context
The sidebar model already tracks unread counts and unread conversations are duplicated into the `Unreads` section. The existing row renderer only applies the libadwaita `heading` CSS class to unread titles, which is not explicit enough to guarantee bold text in the navigation list.

## Requirements
- Unread sidebar conversation titles must use explicit bold font weight.
- Read sidebar conversation titles must use explicit normal font weight.
- Unread count badges may remain visually emphasized.
- The behavior must apply to normal sidebar sections and duplicate rows in the `Unreads` section.
- Add focused unit coverage for the unread/read font-weight decision.

## Acceptance Criteria
- A conversation row with `unread_count > 0` produces bold title styling.
- A conversation row with `unread_count == 0` produces normal title styling.
- Existing sidebar grouping and unread badge behavior are unchanged.
- Existing tests pass, and a compile command is run after code changes.

## Out of Scope
- Full Slack unread synchronization semantics.
- Changing unread badge display.
- Reordering sidebar conversations.
