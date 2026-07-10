# Spec: Desktop Notifications For Notify-Worthy Messages

## Goal

Conduit should send native desktop notifications when Slack reports new messages that deserve the user's attention.

## Requirements

- Send a desktop notification through the existing GTK/GIO application notification path.
- Notify only when a freshly loaded conversation history contains a latest message newer than the last latest message Conduit recorded for that conversation.
- Notify only for conversations that currently have unread activity.
- Do not notify for the currently selected conversation.
- Do not notify for muted conversations.
- Do not notify for messages authored by the current Slack user.
- Do not notify for cached history or older history pagination.
- Keep notification tracking in memory for this slice; do not add a notification database or preference UI.
- Keep notification body text short and derived from the latest message text, with a generic fallback when needed.

## Acceptance Criteria

- Fresh history for an unread, unmuted, non-selected conversation with a newer incoming latest message emits one desktop notification.
- First observation of a conversation establishes the latest timestamp without notifying.
- Cached history, older pages, selected conversations, muted conversations, read conversations, and current-user messages do not notify.
- Existing read-marker, sidebar unread, and Activity behavior continues to work.
- Automated tests cover the notification decision logic.

## Out Of Scope

- Background polling.
- Slack realtime events.
- Per-channel notification preferences.
- Quiet hours.
- Mention/reaction/thread-reply aggregation beyond the unread state already loaded.
- Persistent deduplication across app restarts.
