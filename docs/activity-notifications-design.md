# Activity Design

Activity is a rail destination that answers "what needs my attention?" from three sources:
mentions of the current user, reactions to the current user's messages, and the threads the
user takes part in. Desktop notification policy is separate; see
[Attention And Notifications](attention-and-notifications.md).

## UI

- A rail button named **Activity** sits directly below Messages (`emblem-important-symbolic`)
  with a badge: the number of unread mention items plus unread reaction items, hidden at 0.
- Selecting it replaces the sidebar column (a `GtkStack` with the pages `conversations` and
  `activity`) with the Activity list. The main pane is untouched, so opening an item shows it
  in the main pane while the list stays up.
- `SidebarMode::{Conversations, Activity}` lives next to `MainMessageView` in
  `WorkspaceViewState`. Selecting Messages, Threads, Files or Later returns the sidebar to
  Conversations; one rail toggle is active at a time and `sync_workspace_chrome` gives the
  Activity toggle precedence while the mode is Activity.
- The header has two toggles that are exclusive or both off, and a switch:
  - **@ N** shows mentions only; N is the number of unread mentions.
  - **thread icon N** shows followed threads only; N is the number of unread threads.
  - **Unread messages** shows only unread items. Neither toggle active shows everything.
- Rows show the sender avatar, a bold author name with "+N" for other participants (latest
  reactor for reactions), a relative time, a two-line snippet with mentions resolved, and a
  context line: `Thread in #channel`, `Thread in Direct message`, `Direct message` or
  `#channel` (with the private-channel lock). A reaction row reads `Reacted :emoji: to:
  <snippet>` and carries a chip with the emoji and its count. The trailing widget is a red
  count pill (`mention-badge-pill`) when unread, else a check mark.
- Activating a mention or reaction opens the conversation at that message
  (`SecondaryAction::OpenMessage`); a thread opens in the thread pane
  (`SecondaryAction::OpenThread`).

## Model

`src/activity.rs` holds the GTK-free model; `src/secondary_views/activity.rs` the widget and
`src/secondary_views/model.rs` the row view models.

- `ActivityItem` is keyed by `(channel_id, ts, kind)`. Kinds: `Mention { broadcast, group }`,
  `Reaction { name, user_ids }` (one item per message and emoji, aggregated) and `ThreadReply`
  (derived, never stored).
- `ActivityFeed` keeps stored items newest first, deduplicated by key, capped at 200. Reaction
  adds aggregate reactors; removing the last reactor drops the item.
- Unread state is derived per render and never persisted. Mentions and reactions are unread
  when their timestamp is after the conversation's `last_read` in the unread ledger (a
  reaction's timestamp is when the reaction arrived, since the reacted message is the user's
  own and always older than their read marker). Threads come from `ThreadRecord`
  (`has_unread_replies`, `unread_reply_count`, `participant_user_ids`) in the thread catalog:
  threads the user started, took part in or follows, at most the newest 100.

## Sources

Nothing in Activity runs at startup against Slack.

| Source | How | Notes |
| --- | --- | --- |
| Live mentions | The realtime path forwards posted messages from others that contain a direct, broadcast or user-group mention. The window classifies them: direct `<@me>`, a user group the user belongs to (`<!subteam^ID>`, membership from the cached `usergroups.list`), or `@channel`/`@here`/`@everyone`. | Own messages are excluded. |
| Backfill | One `search.messages` for `<@me>` (`sort=timestamp`, `sort_dir=desc`, `count=50`). | Sent when Activity is opened and the persisted `sync_metadata` stamp (`activity_backfill`) is missing or older than 15 minutes. |
| Reactions | `reaction_added` and `reaction_removed` carry `item_user`; only reactions by others on the user's own messages are kept. | Live only. |
| Threads | Derived from the thread catalog at render time. | No extra calls. |

Mentions and reactions persist as `activity_item` rows in `workspace_items` (written two
seconds after a change, replacing the set) and are read from the local store at startup
without network.

## Performance

- `UiInvalidations::ACTIVITY` coalesces events into one rebuild per frame. While the
  conversation sidebar is showing, a change only marks the list stale (`PickerRefreshGate`)
  and it is rebuilt when shown.
- Rows are keyed. A refresh keeps the widgets of rows whose data and avatar did not change and
  splices only the rest (`splices_around`), so scroll position and selection survive. Sort
  keys are parsed once per item.
- The list is a virtualized `GtkListView`.
- `perf_guard_activity_*` tests cover the closed view, event bursts and unchanged rows.

## Gaps

- Reactions are live only: reactions that happened while Conduit was not running are not
  recovered, and Slack offers no cheap query for them.
- No `@channel`/`@here` backfill: `search.messages` for the user's id finds direct mentions
  only. User-group mentions are live only too.
- No invitations, keywords, app notifications or saved-item reminders.
- A reaction that arrives after the newest message of a fully read conversation stays unread
  until a later message in that conversation is read.
- The 200-item cap drops the oldest stored mentions and reactions.
