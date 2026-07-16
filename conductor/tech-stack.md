# Tech Stack

## Application
- Rust 2021.
- GTK4, libadwaita, and WebKitGTK 6 for the desktop UI.
- Tokio's multi-threaded runtime for concurrent background I/O, with asynchronous channels delivering events to GTK's main loop.
- Request/session identities and operation targets prevent stale asynchronous work from changing the active workspace surface.
- `WorkspaceViewState` owns navigation, loading, transient search context, and render-state transitions independently from GTK widgets.

## Architecture and Errors
- `thiserror` defines typed errors at Slack, persistence, authentication, and other boundaries where callers can make recovery decisions.
- `anyhow` remains available at executable and orchestration boundaries for contextual aggregation when no typed recovery decision is needed.
- A small workspace lifecycle model describes connection and synchronization state independently from navigation-oriented `WorkspaceViewState`.
- Application services are extracted incrementally behind narrow ports only when a concrete use case and headless test require the seam.
- `services::conversation_history` owns cached-preview and fresh-history orchestration behind focused Slack and store ports; the runtime adapts its progress into GTK-facing events.

## Observability
- `tracing` provides structured asynchronous spans for runtime sessions, requests, operations, and non-sensitive targets.
- `tracing-subscriber` initializes human-readable diagnostics at the executable boundary and respects environment filtering.
- Credentials, OAuth values, browser-session data, and message bodies are excluded from fields and events.

## Slack Integration
- `reqwest` with rustls for Slack Web API requests.
- Slack OAuth PKCE user-token flow through `oauth.v2.user.access`.
- Slack API calls validated with `auth.test`.
- Keyring-backed token storage through Secret Service.

## Build and Test
- Cargo for Rust dependency management and unit tests.
- Meson/Ninja for GNOME build integration and resources.
- `cargo test` and `meson test -C _build` are supported validation commands.

## Local State
- XDG cache paths under the application ID for WebKit data, image assets, and Slack state caches.
- Workspace cache mutations are serialized across store clones before read-modify-write updates.
- GSettings stores workspace/user/conversation/thread-scoped composer drafts.

## Presentation
- libadwaita split views and breakpoints adapt the workspace and thread shell to narrow windows.
- Generated message documents use semantic HTML, logical responsive CSS, locale-aware timestamps, RTL direction, and keyboard-focusable message targets.
- Desktop notifications use stable workspace/user/channel IDs and typed application actions so activation can survive a cold start.
