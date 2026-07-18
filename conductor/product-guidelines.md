# Product Guidelines

## User Experience
- Keep the app focused on the current workspace and conversation task.
- Prefer native GTK/libadwaita controls and platform conventions.
- Use short, direct labels and errors.
- Avoid exposing implementation details unless they help a user fix setup.

## Security
- Do not log Slack tokens, OAuth codes, cookies, or other authentication secrets.
- Store reusable Slack credentials in the system keyring.
- Treat browser-session credentials as sensitive and optional.
- Validate imported credentials before saving them.
- Require an explicit user action before joining a huddle or activating microphone, camera, or screen sharing.
- Keep camera and screen sharing off by default and show persistent, accessible indicators for every active capture source.
- Tear down capture, portal sessions, media graphs, SDP/ICE state, and ephemeral TURN credentials immediately on leave, sign-out, session replacement, shutdown, or unrecoverable failure.
- Never log or persist raw huddle payloads, SDP, ICE candidates, TURN credentials, media, or device-capture data.
- Treat Slack bootstrap responses and Amazon Chime meeting/attendee credentials, join tokens, signalling URLs, and TURN data as ephemeral secrets even when private huddle support is explicitly enabled.
- Fall back to an explicit external Slack action when private huddle bootstrap is unavailable; never hide repeated retries or resume camera/screen sharing after reconnect without renewed user intent.

## Documentation
- Document authentication setup paths in `README.md`.
- Call out when a flow is intended for development or advanced users.
- Include exact environment variable names for token-based setup.
- Distinguish verified Slack huddle discovery from experimental first-party Slack/Chime joining, and document the native-media build/runtime requirements and external fallback. Never imply that the generic GStreamer WebRTC harness implements Chime signalling.
