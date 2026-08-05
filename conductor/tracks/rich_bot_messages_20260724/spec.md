# Rich Bot Messages

## Summary

Implement the approved rich bot message architecture so Conduit preserves and renders Slack Block
Kit, legacy attachments, bot/app identity, and controls consistently across Web API, realtime,
cache, and presentation paths.

## Requirements

1. `SlackMessage` retains legacy attachments, bot/app identity, bot profile, and icon metadata
   through serde and cache round-trips.
2. Common legacy attachment content renders safely: pretext, author, title/link, text, fallback,
   fields, images, thumbnails, color, and actions.
3. Common Block Kit content renders safely: section fields/accessories, header, rich text, context,
   divider, image, actions, static selects, and overflow menus.
4. Supported content and fallback policy never leaves a genuine message blank.
5. Bot/app display names and avatars resolve before the generic Slack fallback without acquiring
   user presence or person-profile behavior.
6. Safe URL controls remain navigation affordances. Supported Slack callback buttons execute
   through the authenticated browser-session transport; unsupported controls open the exact
   originating message in Slack.
7. Exact-message handoff prefers `chat.getPermalink` and uses a strictly validated constructed
   workspace permalink only when the API result is unavailable.
8. Callback identifiers, action/option values, raw payloads, response URLs, and credentials never
   enter HTML, custom URLs, logs, or test artifacts.
9. Web API, realtime, coordinator, and store paths use the same retained message representation.
10. Existing cached rows that already lost data recover through normal fresh history/realtime
    replacement without duplicate messages.
11. Callback execution uses Slack's private `chat.attachmentAction` and `blocks.actions` methods
    only when XOXC/XOXD browser-session credentials are available.
12. Each callback button receives its own opaque, revision-bound handle. Sensitive callback and
    action values remain outside HTML, custom URLs, debug output, and diagnostics.
13. Slack confirmation metadata is honored before dispatch. Successful actions reload the visible
    message context; failures log complete sanitized diagnostics and show a short status message.

## Acceptance Criteria

- Synthetic attachment-only Bob-like messages render content, author identity, actions, reactions,
  and thread metadata after serialization and deserialization.
- Synthetic Jira-like messages render rich text, buttons, static selects, and overflow menus.
- URL controls allow only safe HTTP(S) destinations.
- Executable callback buttons activate the matching private Slack action exactly once; controls
  without retained action metadata or browser-session authentication keep exact-message handoff.
- Bob-style Block Kit buttons and Outlook-style legacy attachment buttons have synthetic request
  shape coverage.
- Explicit Slack confirmations are shown before dispatch.
- Success reloads the visible conversation or thread. Failure permits retry without duplicate
  in-flight dispatch and never logs action values.
- Unknown or malformed nodes preserve valid siblings and accessible fallback.
- Cache round-trip and fresh-over-old merge tests cover the formerly discarded fields.
- Generated HTML and action URLs contain no callback/action values.
- Rust formatting, strict Clippy, Rust tests, Meson compilation, and Meson tests pass.

## Out of Scope

- Calling another app's Request URL directly or fabricating Slack-signed app requests.
- Interactive selects, overflow choices, modals, and external suggestion providers.
- Block Kit authoring, workflow building, modal hosting, and external option-provider hosting.
- Multiple connected Slack workspaces.

## Design

The approved architecture is recorded in
[`docs/rich-bot-messages-design.md`](../../../docs/rich-bot-messages-design.md).

## Architectural Improvement Requirements

The acceptance review found that the first implementation retained and rendered the required data,
but left transport, domain, presentation, and action policy coupled. Before this track can be
accepted:

1. Slack message wire shapes must be decoded through a bounded, tolerant transport boundary and
   normalized into canonical author, document, and control types.
2. Persisted and rendered message types must not expose raw callback values through `Debug`,
   `Display`, HTML, custom URLs, or diagnostics.
3. Visible text, accessibility text, notifications, mention extraction, and HTML must derive from
   the same ordered canonical document and fallback policy.
4. HTML generation must consume a pure render plan rather than inspect raw Slack JSON or decide
   control capabilities inline.
5. Bot/app identity must consistently control labels, avatars, grouping, presence, and person
   actions.
6. Rich message presentation must have owned CSS and browser-level DOM/accessibility coverage.
7. Callback controls must use opaque, session- and revision-scoped handles that are resolved
   against current coordinator state and rejected when stale, replayed, or forged.
8. Exact-message Slack handoff must live behind typed safe-URL, permalink-provider, and
   external-opener boundaries with explicit authoritative/fallback provenance.
9. Cache content must be versioned, and richer fresh/realtime messages must replace older lossy
   rows without duplication.
10. The resulting module layout and contributor documentation must make adding a supported message
    node a local, compiler-guided change.
