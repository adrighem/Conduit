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

## Releases and Distribution
- Release Please v4 maintains conventional-commit release pull requests, the changelog, synchronized Cargo/Meson/AppStream versions, `v<semver>` tags, and GitHub Releases.
- Release jobs build the complete Meson installation in Debian 13 and Fedora 44 containers, then create architecture-native `.deb` and `.rpm` assets with explicit runtime dependencies.
- The official Flatpak GitHub Actions builder produces an offline Cargo build against the GNOME 50 runtime and attaches an installable single-file bundle to each GitHub Release.
- GitHub Release bundles are the supported Flatpak distribution path. Flathub onboarding remains a separate human-owned process subject to Flathub policy and review.

## Local State
- XDG cache paths under the application ID for WebKit data, image assets, and Slack state caches.
- A workspace-scoped `StoreHub` owns one persistent SQLite writer and two query-only readers behind bounded channels; revisioned batches, commit barriers, and clean shutdown replace per-operation connections.
- Schema-v2 freshness and retry metadata augment keyed derived-cache payloads. Cache migration/corruption recovery may recreate derived Slack data without touching keyring credentials or GSettings drafts.
- GSettings stores workspace/user/conversation/thread-scoped composer drafts.

## Workspace Pipeline
- A headless `WorkspaceCoordinator` maintains a revisioned canonical view for the inputs migrated so far, alongside the existing runtime, persistence, and UI compatibility paths.
- Cached hydration, Web API responses, local actions, and realtime transports normalize into pure `WorkspaceMutation` values. Each changed logical mutation emits one revisioned `WorkspacePatch` and, when persistence changes, one ordered `StoreBatch`; compatibility paths still deliver the current UI events and store writes.
- Snapshot envelopes carry the revision at which network work began so reducer merges cannot roll back newer realtime or local overlays.
- Runtime work is bounded by separate semaphores for navigation, interactive, background, image, and upload lanes. Typed `SyncJob` coalescing and cancellation remain part of the active workspace-pipeline track.

## Presentation
- libadwaita split views and breakpoints adapt the workspace and thread shell to narrow windows.
- Generated message documents use semantic HTML, logical responsive CSS, locale-aware timestamps, RTL direction, and keyboard-focusable message targets.
- A keyed `SidebarModelDiff` reconciles stable rows in the current `GtkListBox`; migration to a virtualized `GtkListView` remains planned.
- Message timelines load generated HTML documents for navigation and apply typed DOM patches for realtime messages, response regions, user details, and loaded media. A revision-aware batching presenter remains planned.
- Cached message media is size- and MIME-checked before it is rendered through bounded data URLs. A dedicated cache-key URI scheme remains planned.
- Desktop notifications use stable workspace/user/channel IDs and typed application actions so activation can survive a cold start.

## External Slack URI Integration
- GIO `Application::open`, command-line forwarding, and the XDG desktop scheme handler deliver `slack://` URIs to the existing single-instance GTK application.
- A pure Rust parser validates official Slack custom-scheme links before the GTK layer resolves them against the active workspace.
- Conduit does not claim HTTP or HTTPS and does not install a browser extension; normal Slack web links remain in the browser unless Slack explicitly invokes its custom scheme.

## Native Huddles
- Pure huddle models, coordinator transitions, media intents, signalling capabilities, and fake adapters are always compiled and tested without Slack credentials or capture devices.
- Native media is isolated behind the Cargo `native-media` feature and a Meson feature option so builds without WebRTC development headers retain official discovery, UI state, and external Slack fallback.
- The native media stack uses `gstreamer` 0.23.7, `gstreamer-sdp` 0.23.5, and `gstreamer-webrtc` 0.23.5 with their GStreamer 1.24 API features. This generation shares Conduit's existing GLib 0.20 type universe; the newer 0.25 bindings require a different GLib generation.
- `ashpd` 0.11.1 with GTK4 integration owns user-initiated ScreenCast portal sessions. The portal-provided restricted PipeWire file descriptor and selected stream node remain ephemeral and are released when sharing stops.
- One session-owned huddle actor serializes signalling and media commands and exclusively owns GStreamer pipelines, portal sessions, ephemeral negotiation state, and teardown.
- GStreamer `webrtcbin` provides the generic native WebRTC transport used by the deterministic harness; `webrtcdsp` and `webrtcechoprobe` provide the optional stack's echo-cancellation path; PipeWire/GStreamer plugins provide local audio, camera, and screen-share streams.
- Slack huddles are Amazon Chime meetings. Generic SDP/ICE exchange through `webrtcbin` is not by itself compatible with Chime's signalling contract and must never be presented as a production Slack join path.
- Slack-supported conversation metadata and `user_huddle_changed` events provide discovery and presence. First-party `rooms.join` bootstrap and a Chime-compatible media bridge remain behind replaceable, independently capability-checked adapters because Slack does not publish a huddle join API. Enabling `native-media` alone never enables private Slack joining.
- A future production Chime bridge may wrap Amazon's Apache-licensed C++ signalling SDK, but it must remain disabled until Conduit has a verified, redacted Slack bootstrap contract and a tested media integration for the packaged platform.
- A deterministic synthetic signalling/media harness exercises negotiation, controls, reconnect, statistics, and teardown. Production protocol drift degrades to an explicit external Slack handoff.

## Native Huddle Packaging
- Native compilation requires GStreamer core, base, and bad-plugin development metadata at version 1.24 or newer. Runtime packages must provide `webrtcbin`, ICE/libnice, Opus, camera/video codecs, PipeWire, and audio source/sink plugins.
- CI validates both the default build and `--features native-media,screen-share,huddle-harness`; synthetic sources and sinks replace real devices and portals in automated tests.
- General Debian, RPM, and Flatpak releases explicitly disable `native-media` and `screen-share` until a production Slack/Chime join path is verified. They retain huddle discovery and the external Slack fallback without media dependencies or capture permissions.
- Experimental developer builds may enable the Meson native-media options. Screen sharing then uses the portal's restricted PipeWire remote without broad device, filesystem, or session-bus permissions.
