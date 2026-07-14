# ISSUE:6 — Show Slack status for direct-message users

- Status: implemented in `8b3965d`; closure-ready after CI is green
- Confidence: high
- Implemented: cached profile statuses, expiration scheduling, realtime `user_change`, DM-only sidebar/switcher/title/pane rendering, tooltip and accessible text, search/sort isolation
- Residual risk: custom status emoji in the constrained native title falls back to a dot; sidebar and message pane retain custom emoji rendering
- Public action: none taken
