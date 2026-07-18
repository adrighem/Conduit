# External Slack Link Integration

## Summary

Allow Slack links opened by the desktop or a web browser to activate Conduit and navigate to the matching location in the connected workspace. Keep URI parsing centralized, validate every external target, and use a narrowly scoped browser extension for Slack HTTPS handoff instead of claiming all HTTPS links.

## Requirements

1. Parse supported Slack HTTPS, `slack://`, and dedicated `conduit-slack://` browser-handoff URIs into typed navigation targets without performing UI work.
2. Accept Slack message permalinks, channel routes, direct-message user routes, file routes, `app_redirect` channel routes, and supported `app.slack.com/client` routes.
3. Recognize official Slack deep-link actions and safely leave unsupported product surfaces in Slack's web experience rather than inventing native behavior.
4. Reject malformed links, credentials in URLs, non-Slack nested handoff URLs, misleading host suffixes, invalid identifiers, recursive handoffs, and oversized inputs.
5. Match workspace-scoped links against the connected Slack team ID or workspace host. Queue valid links across cold start and workspace loading, and report a concise error for links targeting another workspace.
6. Route supported targets to existing Conduit behavior for app activation, conversations, direct messages, message/thread context, user profiles, and files.
7. Support both D-Bus `Open` activation and direct command-line URI delivery while preserving existing command-line options and single-instance behavior.
8. Register Conduit as an available handler for `slack://` and `conduit-slack://` without exposing the app-internal `conduit://` action scheme or silently replacing the user's current default handler.
9. Package one Manifest V3 WebExtension for current Firefox and Chromium-family browsers. It must observe only top-level navigation, hand off only supported Slack HTTPS routes, request no page-content access, and contain no remote code.
10. Document browser installation, desktop-handler selection, supported links, fallbacks, and troubleshooting.

## Acceptance Criteria

- Pure Rust tests cover every supported link family, workspace scope, wrapper validation, and representative malicious or malformed inputs.
- A cold or already-running Conduit instance receives external URIs through one application-level routing path.
- Message permalinks open the exact message or thread context; channel and user links open the correct conversation; file links load the addressed file.
- The desktop entry contains URI field codes and both custom-scheme MIME registrations, and metadata tests prevent regressions.
- Firefox and Chromium can load the packaged extension manifest, and JavaScript tests prove that only supported top-level Slack navigations produce `conduit-slack://` handoff URLs.
- Unsupported Slack web surfaces continue in the browser, avoiding redirect loops.
- `cargo fmt --check`, strict Clippy, all Rust tests, Node extension tests, Meson compile, desktop validation, AppStream validation, and Meson tests pass.

## Security and UX Constraints

- Never register Conduit as the general HTTP or HTTPS handler.
- Do not read page contents, cookies, credentials, message bodies, or browser history from the extension.
- Do not log complete externally supplied URLs when they may contain sensitive query data.
- Do not silently change the user's existing `slack://` default application.
- Keep wrong-workspace and unsupported-link feedback short and actionable.

## Out of Scope

- Native implementations of Slack App Home, canvases, lists, workflows, huddles, or file-sharing dialogs.
- Silent browser-extension installation, which browsers intentionally prohibit.
- Multi-workspace switching or adding a second authenticated workspace.
- Taking ownership of all HTTP or HTTPS links.
