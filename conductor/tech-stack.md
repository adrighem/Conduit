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
- GStreamer `webrtcbin` provides the generic native WebRTC transport used by the deterministic harness; `webrtcdsp` and `webrtcechoprobe` provide the packaged echo-cancellation path; PipeWire/GStreamer plugins provide local audio, camera, and screen-share streams.
- Slack huddles are Amazon Chime meetings. Generic SDP/ICE exchange through `webrtcbin` is not by itself compatible with Chime's signalling contract and must never be presented as a production Slack join path.
- Slack-supported conversation metadata and `user_huddle_changed` events provide discovery and presence. First-party `rooms.join` bootstrap and a Chime-compatible media bridge remain behind replaceable, independently capability-checked adapters because Slack does not publish a huddle join API. Enabling `native-media` alone never enables private Slack joining.
- A future production Chime bridge may wrap Amazon's Apache-licensed C++ signalling SDK, but it must remain disabled until Conduit has a verified, redacted Slack bootstrap contract and a tested media integration for the packaged platform.
- A deterministic synthetic signalling/media harness exercises negotiation, controls, reconnect, statistics, and teardown. Production protocol drift degrades to an explicit external Slack handoff.

## Native Huddle Packaging
- Native compilation requires GStreamer core, base, and bad-plugin development metadata at version 1.24 or newer. Runtime packages must provide `webrtcbin`, ICE/libnice, Opus, camera/video codecs, PipeWire, and audio source/sink plugins.
- CI validates both the default build and `--features native-media`; synthetic sources and sinks replace real devices and portals in automated tests.
- Flatpak enables the Meson native-media option, uses the GNOME Platform media stack, and grants only the standard PulseAudio socket for microphone/speaker access in addition to existing network, display, and DRI access. Screen sharing uses the portal's restricted PipeWire remote without broad device, filesystem, or session-bus permissions.
