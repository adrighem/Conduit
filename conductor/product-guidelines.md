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

## Documentation
- Document authentication setup paths in `README.md`.
- Call out when a flow is intended for development or advanced users.
- Include exact environment variable names for token-based setup.
