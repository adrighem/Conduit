# Priority Conversations

## Summary

Add a Priority section to the conversation sidebar using Slack's documented
conversation-star state. Starred direct messages are presented as VIP
conversations before starred channels, while every conversation remains in its
normal sidebar section.

Slack's separate paid-plan VIP preference has no documented public API. Conduit
therefore treats starred DMs as its VIP projection and does not claim to read or
change Slack's private VIP list.

## Requirements

1. Read a conversation's user-relative `is_starred` value from Slack conversation objects.
2. Add a Priority section above Unreads, Channels, Direct messages, and Other.
3. List starred direct and group direct messages before starred public and private channels.
4. Sort conversations consistently by title and stable ID within each Priority category.
5. Keep Priority conversations in their existing Unreads and type-specific sections.
6. Provide Star or Unstar actions for supported channel and DM rows.
7. Toggle the Slack state with `stars.add` or `stars.remove` using a bare conversation ID.
8. Persist a successful toggle through the existing workspace conversation pipeline and update every duplicate sidebar row.
9. Preserve the current flat search and unread-filter behavior.
10. Keep single-workspace behavior unchanged.
11. Provide a Profile action in the sidebar context menu for one-to-one DMs with
    a known Slack user ID.
12. Open that Profile action through the existing main-webview profile flow.
13. Open person completion when `@` is typed at a mention boundary in either
    the message or thread composer.
14. Search the full available workspace person catalog by display name, full
    name, normalized aliases, and Slack username rather than limiting results
    to existing direct messages.
15. Reuse the emoji completion popup's mouse and keyboard interactions,
    including Up, Down, Enter, Tab, and Escape.
16. Present the selected person's readable name in the composer while sending
    Slack's canonical `<@USER_ID>` mention form.
17. Preserve completed mentions through draft save and restore without
    notifying a stale person after the visible mention text is edited.

## Acceptance Criteria

- Model tests cover parsing, mutating, and merging explicit starred state.
- Slack request tests prove conversation stars omit a message timestamp.
- Sidebar tests prove Priority is first, DMs precede channels, title and ID tie-breaks are deterministic, and normal sections retain their rows.
- Empty Priority sections are omitted.
- Keyed sidebar tests prove duplicate rows remain section-scoped and independently collapsible.
- UI helper tests cover Star and Unstar labels and supported conversation kinds.
- UI helper tests cover Profile eligibility for one-to-one DMs and reject
  channels, group DMs, and malformed DMs without a user ID.
- Activating Profile loads the selected person's profile in the main webview
  through the same path used by message author profile links.
- Composer tests cover mention boundaries, filtering, deterministic ranking,
  Unicode character offsets, and canonical mention serialization.
- Both message and thread composers expose the same person completion behavior
  without competing with emoji completion.
- Person suggestions exclude deleted users and bots, deduplicate by Slack user
  ID, and remain searchable from the full loaded catalog.
- Draft round trips preserve canonical mention IDs while restoring readable
  names where identity data is available.
- A successful toggle updates persisted conversation state and rerenders the sidebar.
- `cargo fmt --check`, Rust tests, `cargo check`, Meson compile, and Meson tests pass in a sanitized allowlisted environment.

## Architectural Constraints

- Reuse `SlackConversation`, `ConversationCatalog`, `WorkspaceCoordinator`, and the keyed sidebar reconciliation path.
- Keep Slack network calls in `SlackApi` and runtime orchestration out of GTK widget helpers.
- Use stable conversation IDs for toggle targets.
- Do not log credentials, browser-session data, message bodies, or complete environments.
- Do not add a new dependency.

## Out of Scope

- Reading or changing Slack's undocumented paid-plan VIP preference.
- Creating Conduit-local priority state that diverges from Slack conversation stars.
- Custom Slack sidebar sections or ordering.
- Multi-workspace support.
- Replacing the existing conversation list widget.
