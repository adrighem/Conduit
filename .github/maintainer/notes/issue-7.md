# ISSUE:7 — GNOME Shell search provider

- Status: hardening and live D-Bus coverage implemented locally; closure-ready after remote CI
- Confidence: high
- Implemented: SearchProvider2 D-Bus object, installed provider metadata, active-workspace lightweight index, channel/DM/MPIM search, shared matching, version/kind/stale filtering, prior-result subsearch, workspace-scoped opaque IDs, metadata, and exact conversation activation
- Validation: seven pure provider tests, lightweight-index persistence test, packaging metadata validation, and a live isolated-session D-Bus smoke test that verifies the selected target
- Public action: none taken
