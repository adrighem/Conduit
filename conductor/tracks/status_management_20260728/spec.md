# Current User Status Management

## Summary

Let the connected user view and change their Slack custom status from Conduit.
The workspace sidebar header presents the user's display name and active status,
and the workspace menu opens a native status editor.

Slack's public API exposes custom status text, emoji, and expiration, but it
does not expose workspace-managed status suggestions. Conduit therefore
provides a free-form native editor without claiming to mirror private Slack
client presets.

## Requirements

1. Add `users.profile:write` to the default OAuth user scopes.
2. Set or clear the authenticated user's status with `users.profile.set`.
3. Send `status_text`, `status_emoji`, and `status_expiration` together, using
   an absolute Unix timestamp or `0` for no expiration.
4. Clear a status by sending empty text and emoji with expiration `0`.
5. Route status writes through the runtime mutation architecture and update UI
   state only after Slack confirms success.
6. Keep existing realtime `user_change` handling authoritative for later
   profile and status changes.
7. Replace the workspace header title with the current user's display name once
   it is known, falling back to the workspace name while identity loads.
8. Show the current active status in the workspace header subtitle, resolving
   Unicode emoji where possible and exposing complete accessible text.
9. Add a `Set status...` action to the workspace menu above New Message and New
   Channel.
10. Present a native adaptive status dialog that preloads the active status,
    accepts text-only or emoji-only statuses, enforces Slack's 100-character
    text limit, and supports explicit clearing.
11. Provide a searchable emoji chooser backed by the existing Unicode and
    workspace custom emoji catalog, including a No Emoji choice.
12. Offer predictable expiration choices: do not clear, 30 minutes, 1 hour,
    4 hours, end of today, and end of this week. Preserve an existing custom
    future expiration when the dialog opens.
13. Restore the dialog and its actionable draft when Slack rejects the update,
    with a concise accessible error.
14. Preserve Conduit's single-workspace boundary and existing connection status
    presentation.
15. Use the existing browser-session request transport when configured, while
    surfacing Slack authentication or permission failures without promising
    undocumented browser-session API support.

## Acceptance Criteria

- Slack request tests prove the exact `users.profile.set` payload for set and
  clear operations and confirm no other profile fields are sent.
- OAuth scope tests and setup documentation include `users.profile:write`.
- Runtime descriptor and event tests cover a current-user status mutation.
- Pure tests cover status input validation, expiration calculation, preserved
  custom expiration, and workspace header presentation.
- The workspace menu exposes the status action through the correct window
  action.
- The dialog preloads active state, disables Save for an empty status, permits
  text-only and emoji-only statuses, and exposes Clear Status only when useful.
- Emoji filtering and keyboard navigation reuse the existing catalog and
  selection behavior.
- Successful updates refresh the header and current-user status everywhere;
  failures do not present an unconfirmed status.
- A headless GTK test activates the window action and covers empty and preloaded
  dialog and header state in wide and narrow layouts. API, runtime, and pure
  tests cover save, clear, failure recovery, and confirmed header refresh.
- `cargo fmt --check`, Rust tests, `cargo check`, Meson compile, and Meson tests
  pass in a sanitized allowlisted environment.

## Architectural Constraints

- Keep Slack network requests in `SlackApi` and asynchronous orchestration in
  the runtime.
- Reuse `SlackUserStatus`, existing identity/status caches, `EmojiCatalog`, and
  Adwaita dialog patterns.
- Do not introduce a new dependency or a second workspace-session model.
- Do not log credentials, complete environments, or private status text.

## Out of Scope

- Reading Slack's private workspace status suggestions or automatic calendar
  status integrations.
- Changing presence, Do Not Disturb, notification schedules, profile names, or
  other profile fields.
- Admin changes to another user's status.
- Replacing the workspace connection-status row.
