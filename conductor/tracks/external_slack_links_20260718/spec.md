# External Slack URI Integration

## Summary

Allow `slack://` links opened by the desktop or a web browser to activate Conduit and navigate to the matching location in the connected workspace. Keep URI parsing centralized, validate every external target, and integrate through the standard XDG custom-scheme handler without a browser extension or general HTTPS association.

## Requirements

1. Parse Slack's documented `slack://` URI forms into typed navigation targets without performing UI work.
2. Support `open`, `channel`, `user`, `file`, `share-file`, and `app` actions, including their documented team, target ID, and App Home tab parameters.
3. Follow Slack's documented fallback for unknown `slack://` actions by opening Conduit normally.
4. Reject malformed links, credentials in URLs, unexpected paths or fragments, invalid identifier types, duplicated sensitive parameters, control characters, and oversized inputs.
5. Match workspace-scoped links against the connected Slack team ID. Queue valid links across cold start and workspace loading, and report a concise error for links targeting another workspace.
6. Route supported targets to existing Conduit behavior for app activation, channels, direct messages, and files. Unsupported native App Home and file-sharing behavior must fall back safely or show a clear status without recursively reopening `slack://`.
7. Support both D-Bus `Open` activation and direct command-line URI delivery while preserving existing command-line options and single-instance behavior.
8. Register Conduit as an available handler for `slack://` without silently replacing the user's current default handler.
9. Document desktop-handler selection, supported links, HTTPS limitations, fallbacks, and troubleshooting.

## Acceptance Criteria

- Pure Rust tests cover every documented `slack://` family and representative malicious or malformed inputs.
- A cold or already-running Conduit instance receives external URIs through one application-level routing path.
- Channel and user links open the correct conversation; file links load the addressed file; open links present the application.
- Wrong-workspace links never open an identically shaped target in the active workspace.
- The desktop entry contains a URI field code and `x-scheme-handler/slack`, and metadata tests prevent regressions.
- Ordinary Slack HTTPS links remain owned by the browser unless the Slack site itself invokes a `slack://` link.
- `cargo fmt --check`, strict Clippy, all Rust tests, Meson compile, desktop validation, AppStream validation, and Meson tests pass.

## Security and UX Constraints

- Never register Conduit as the general HTTP or HTTPS handler.
- Never expose the app-internal `conduit://` message-action scheme as a desktop handler.
- Do not log complete externally supplied URIs.
- Do not silently change the user's existing `slack://` default application.
- Keep wrong-workspace and unsupported-link feedback short and actionable.

## Out of Scope

- A browser extension or native-messaging bridge.
- Direct interception of ordinary `https://*.slack.com` navigation.
- Native implementations of Slack App Home, canvases, lists, workflows, huddles, or file-sharing dialogs.
- Silent browser or desktop-handler configuration.
- Multi-workspace switching or adding a second authenticated workspace.
