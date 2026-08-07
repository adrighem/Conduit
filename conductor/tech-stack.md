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
- `services::conversation_history` owns cached and fresh history retrieval behind focused Slack and store ports. Runtime results enter the workspace coordinator; state-free completion events carry only request metadata such as pagination cursors.

## Observability
- `tracing` provides structured asynchronous spans for runtime sessions, requests, operations, and non-sensitive targets.
- `tracing-subscriber` initializes human-readable diagnostics at the executable boundary and respects environment filtering.
- Debug mode samples saturating, process-wide pipeline counters every 60 seconds. Samples report deltas and totals for admitted jobs, outbound Slack requests, SQLite connections/transactions/changed/skipped rows, sidebar operations, document loads, and timeline deltas.
- Credentials, OAuth values, browser-session data, and message bodies are excluded from fields and events.

## Slack Integration
- `reqwest` with rustls for Slack Web API requests.
- Slack OAuth PKCE user-token flow through `oauth.v2.user.access`.
- Slack API calls validated with `auth.test`.
- Keyring-backed token storage through Secret Service.

## Build and Test
- Cargo for Rust dependency management and unit tests.
- Meson/Ninja for GNOME build integration and resources.
- `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked --all-targets`, `meson compile -C _build`, and `meson test -C _build` form the default acceptance gate.

## Releases and Distribution
- Release Please v4 maintains conventional-commit release pull requests, the changelog, synchronized Cargo/Meson/AppStream versions, `v<semver>` tags, and GitHub Releases.
- Release jobs build the complete Meson installation in Debian 13 and Fedora 44 containers, then create architecture-native `.deb` and `.rpm` assets with explicit runtime dependencies.
- The official Flatpak GitHub Actions builder produces an offline Cargo build against the GNOME 50 runtime and attaches an installable single-file bundle to each GitHub Release.
- GitHub Release bundles are the supported Flatpak distribution path. Flathub onboarding remains a separate human-owned process subject to Flathub policy and review.

## Local State
- XDG cache paths under the application ID for WebKit data, image assets, and Slack state caches.
- A workspace-scoped `StoreHub` owns one persistent SQLite writer and two query-only readers behind bounded channels. Revisioned atomic `StoreBatch` writes, unchanged suppression, commit barriers, and clean shutdown replace routine per-operation write connections.
- Schema-v2 freshness and retry metadata augment keyed derived-cache payloads. Cache migration/corruption recovery may recreate derived Slack data without touching keyring credentials or GSettings drafts.
- GSettings stores workspace/user/conversation/thread-scoped composer drafts.

## Workspace Pipeline
- A headless `WorkspaceCoordinator` is the sole canonical owner of conversations, users, channel and thread timelines, thread records, read/attention overlays, message identities, and monotonically increasing workspace revisions for the one connected workspace.
- Cached hydration, Web API responses, local actions, and both realtime transports normalize into typed `WorkspaceMutation` values. One accepted logical mutation emits at most one `WorkspacePatch` and one ordered atomic `StoreBatch`; identical input emits neither.
- Persistence is attempted before its matching patch is published; persistence failure does not suppress the current canonical projection and is handled by ordered recovery. GTK receives coordinator-owned workspace state only through revisioned patches, while companion completion events for those paths carry request or operation metadata rather than parallel state payloads.
- Snapshot envelopes carry the revision at which network work began. Coordinator merges preserve newer local/realtime values, read cursors, tombstones, and message identity decisions when stale results arrive.
- A bounded `SyncScheduler` coalesces typed jobs across interactive, foreground, and yielding maintenance priorities, reserves capacity before spawning, applies freshness and retry policy, and supports generation-scoped cancellation and clean shutdown. Image and upload work retain specialized bounded execution paths.
- Startup hydrates one cached projection, starts realtime promptly, conditionally refreshes membership, enriches at most 30 priority conversations, and prefetches at most 12 histories. User-directory refresh remains lazy after a warm cache.

## Presentation
- libadwaita split views and breakpoints adapt the workspace and thread shell to narrow windows.
- Generated message documents use semantic HTML, logical responsive CSS, locale-aware timestamps, RTL direction, and keyboard-focusable message targets.
- A tolerant Slack wire decoder normalizes Block Kit, legacy attachments, and bot/app identity into
  one versioned canonical message document. Rendering, accessibility, notifications, mentions, and
  cache projections share that document; raw callback values are neither rendered nor persisted.
- Safe URL controls navigate directly. Slack-owned callback controls use one-shot opaque,
  session/revision-scoped handles and a typed, validated exact-message external handoff with
  explicit authoritative or constructed-fallback provenance.
- A keyed `SidebarProjection` applies splice, update, and reset operations to a `gio::ListStore` backing a virtualized `GtkListView`, preserving stable selection without whole-catalog rebuilds.
- Conversation navigation creates a generation-scoped opening session with an immutable semantic target. One WebKit viewport controller owns initial geometry, reveals the timeline only after positioning, cancels on user interaction, and arms read observation after commit.
- `WorkspaceViewState` consumes workspace patches to update cached projections and derives the narrow sidebar, title, picker, main-view, and thread presentation changes needed for each patch.
- Each message WebView owns a revision-aware timeline presenter. Navigation loads a generated document, while cached-to-fresh snapshots, realtime messages, response regions, user details, and loaded media are coalesced into one anchor-preserving typed DOM delta per GTK frame; full reloads are reserved for initial navigation, revision mismatch, and unrecoverable presentation recovery.
- Cached message media uses an exact raster/video MIME allowlist, content-signature validation, and 8/16 MiB per-file bounds. Raw payloads live under workspace-scoped SHA-256 keys in a deterministic 512 MiB/16,384-entry/30-day disk cache; failed enforcement rolls back the new file. The UI retains only descriptors for a 64 MiB/2,048-entry logical ready set, with bounded source and request state. A private `conduit-asset` WebKit scheme revalidates each file, serves only registered keys with bounded single-range video responses, and invalidates broken DOM sources before one recovery attempt.
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
