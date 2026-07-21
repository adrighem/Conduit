# Spec: Relevant Notifications And Noise-Free Unread State

## Goal

Conduit should keep unread state meaningful and send desktop notifications only for messages that are relevant to the current user.

## Requirements

- Introduce one pure attention policy that classifies a normalized message before unread persistence, UI projection, or notification delivery diverge.
- Return separate decisions for recording unread activity and sending a desktop notification, together with testable relevance reasons.
- Never create Conduit unread state or desktop notifications for membership lifecycle noise, including channel/group join and leave messages.
- Continue recording ordinary, non-self messages as unread even when they do not meet the desktop-notification relevance threshold.
- Treat direct messages as relevant by default.
- Treat a message as notification-relevant when it contains a direct Slack user mention, a configured name/alias, a configured keyword or phrase, or is a reply in a thread the user started, replied to, or subscribed to.
- Match configured names, aliases, and keywords case-insensitively without matching inside unrelated words; document the behavior for phrases and punctuation.
- Preserve existing suppression for self-authored messages, muted conversations, stale or duplicate delivery, and the target currently being actively read.
- Resolve Slack user mentions to display names before delivering notification text; defer delivery while required identity data is loading.
- Keep authoritative Slack unread counters distinct from Conduit's locally derived attention/unread classification so a coarse server snapshot cannot reintroduce filtered lifecycle noise.
- Add a Notifications group to Preferences with controls for desktop notifications, direct messages, mentions/name aliases, thread replies, and a keyword/phrase list.
- Apply preference changes without restarting or reconnecting.
- Store preferences in GSettings with safe defaults that preserve direct-message and explicit-mention notifications while suppressing broad channel noise.
- Build the policy as a domain module consumed by the canonical workspace coordinator. Do not add another window-owned policy or state pipeline.

## Acceptance Criteria

- Table-driven tests cover direct messages, ordinary channel messages, direct mentions, configured names, keywords/phrases, participated and subscribed thread replies, self messages, muted conversations, active targets, duplicates, and every supported join/leave subtype.
- Join/leave messages create neither persisted Conduit unread state nor sidebar/activity unread presentation, including after realtime delivery and reconciliation.
- An ordinary channel message still becomes unread but does not produce a desktop notification under the default relevant-only policy.
- Each enabled relevance trigger produces one notification; disabling that trigger in Preferences suppresses future notifications immediately.
- Notification title and body contain resolved display names instead of raw Slack user IDs.
- Raw Slack aggregate unread data remains available for reconciliation without overriding the locally classified attention result.
- Restart and realtime-redelivery tests demonstrate that notification deduplication does not produce duplicate alerts.
- Existing conversation read markers, thread read markers, muted-channel behavior, and notification activation continue to work.

## Integration Constraints

- Coordinate implementation with `workspace_pipeline_rearchitecture_20260720`: classification belongs before coordinator effects fan out to storage and presentation.
- Reuse normalized message/thread data and `ThreadRecord` subscription/participation state rather than scanning rendered HTML.
- GSettings is a preferences adapter only; `ConduitApplication` remains responsible only for native notification delivery.

## Assumptions

- Direct messages are inherently relevant unless the user disables that preference.
- “Replied in the threads” means a reply to a thread the user started, previously replied to, or explicitly subscribed to.
- Unread state and desktop notifications are related but not identical: non-noise channel messages may be unread without notifying.

## Out Of Scope

- Quiet hours and schedules.
- Per-conversation keyword or notification overrides beyond Slack mute state.
- Reaction notifications.
- Notification digests or aggregation.
- Replacing Slack's server-side unread model.
