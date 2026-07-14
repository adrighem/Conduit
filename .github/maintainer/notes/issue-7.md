# ISSUE:7 — GNOME Shell search provider

- Status: core provider shipped in `8b3965d`; keep open for hardening and installed-session validation
- Confidence: high in pure search/activation model; medium in live GNOME integration
- Implemented: SearchProvider2 D-Bus object, installed provider metadata, cached channel/DM/MPIM search, shared matching, workspace-scoped opaque result IDs, metadata, application-action activation
- Remaining acceptance gaps: subsearch ignores the previous-result set and reparses caches; metadata installation and D-Bus behavior lack automated validation; no installed GNOME Shell smoke test yet
- Recommended next step: narrow subsearch to prior IDs, add metadata/activation/archived/MPIM tests, and run an installed-session smoke test
- Public action: none taken
