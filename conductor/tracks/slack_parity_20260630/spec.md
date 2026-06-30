# Track Spec

## Track Description

Implement practical Slack browser/app navigation parity for the current single-workspace Conduit experience.

## Problem

Conduit should feel closer to Slack's default browser/app workflow. The sidebar should not behave like a raw workspace dump, users need a fast Ctrl-K style conversation switcher, and switching between conversations should favor cached local history instead of repeatedly blocking on fresh network history.

## Requirements

- Load sidebar conversations from a member-scoped Slack API path, not a broad workspace enumeration.
- Keep cached conversations visible while fresh conversation refresh runs or fails.
- Default the sidebar to conversations that are likely to be visible in Slack's app: joined/member conversations, unread conversations, active channels, active group DMs, and active/recent DMs.
- Provide an obvious local override to show all loaded conversations when the user needs to find older/dormant items.
- Add a Ctrl-K shortcut that opens a conversation switcher.
- The switcher must search across all loaded conversations, not only currently visible sidebar rows.
- Selecting a switcher result must open the conversation and close the switcher.
- Prefer cached history immediately when switching conversations.
- Avoid sending duplicate fresh history requests when the selected conversation already has cached in-memory messages.
- Keep explicit refresh/manual load paths available for getting fresh data.

## Acceptance Criteria

- Startup debug logs show `users.conversations returned ... conversations` instead of `conversations.list`.
- A rate-limited conversation refresh reports a sidebar/status error without blocking the runtime command loop.
- The default sidebar hides dormant/closed low-value entries, while a toggle exposes all loaded conversations.
- Ctrl-K opens a modal conversation switcher; Escape or close dismisses it.
- Typing in the switcher filters conversations by resolved title or ID.
- Activating a switcher row opens the selected conversation.
- Re-selecting a conversation with in-memory history renders immediately and does not enqueue another `LoadHistory`.
- `cargo test`, `cargo check`, and `meson compile -C _build` pass.

## Out Of Scope

- Slack-side custom section sync.
- Slack-side sidebar order preference sync.
- Multi-workspace switching.
- Realtime event/socket-mode synchronization.
- Full Slack browser visual parity.
