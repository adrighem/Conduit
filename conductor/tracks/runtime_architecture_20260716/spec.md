# Runtime Architecture Hardening

## Summary

Incrementally harden Conduit's runtime architecture around typed failures, structured observability, explicit workspace lifecycle transitions, and testable application services. Build on the existing request/session identity and `WorkspaceViewState` work instead of replacing it or introducing a broad framework.

## Requirements

1. Slack and persistence boundaries must expose typed errors with stable categories while orchestration code may continue to use `anyhow` for context and aggregation.
2. Runtime failure events must carry a typed category, operation, target, and safe user-facing message so the UI can distinguish authentication, connectivity, storage, validation, and internal failures.
3. Asynchronous runtime work must use `tracing` spans and structured fields for session, request, operation, and target without recording credentials, message contents, OAuth values, or browser-session data.
4. Workspace connection lifecycle must be represented by a small explicit state model covering disconnected, connecting, syncing, ready, degraded, authentication-required, and terminal startup failure behavior.
5. Lifecycle transitions must be pure and unit tested; GTK widgets must render the authoritative lifecycle rather than independently infer it from status strings.
6. Application use cases must be extracted incrementally from `runtime.rs` and `window.rs` behind narrow services. Network and persistence traits should be introduced only where a concrete test seam or alternate implementation needs them.
7. Domain and application modules must remain usable in headless unit tests and must not depend on GTK or WebKit.
8. Existing user-visible behavior, request supersession, stale-event protection, and operation-local recovery must remain intact.

## Acceptance Criteria

- Typed Slack and store errors preserve their source errors and classify representative auth, timeout/connectivity, storage, validation, and unexpected failures in unit tests.
- Runtime failure events expose stable categories and safe display messages; the UI handles authentication-required failures separately from retryable network and local-storage failures.
- Runtime commands and spawned work emit structured spans containing non-sensitive session/request/operation/target fields.
- Tests prove every supported lifecycle transition, including reconnect, sync success, degraded recovery, sign-out, and authentication failure.
- The GTK shell derives connection/loading/error presentation from the lifecycle model rather than ad-hoc status text.
- At least one conversation-oriented use case is moved behind an application service with headless tests and explicit Slack/store ports, establishing the pattern for further extraction.
- `cargo fmt --check`, strict Clippy, all Rust tests, Meson compile, and Meson tests pass.

## Architectural Constraints

- Keep `anyhow` at executable and orchestration boundaries where callers cannot make a meaningful typed recovery decision.
- Do not create a single global state machine for navigation, composer, thread, and connection behavior; lifecycle state stays separate from `WorkspaceViewState`.
- Do not add repository or gateway traits without an immediate consumer and a test that benefits from the seam.
- Do not log secrets or full message bodies.
- Prefer small moves with compatibility adapters over broad module rewrites.

## Out of Scope

- Replacing Tokio, GTK, WebKitGTK, reqwest, or rusqlite.
- Multi-workspace switching.
- A complete rewrite of `runtime.rs` or `window.rs` in one phase.
- A generic dependency-injection framework or event-sourcing architecture.
- Changing Slack API behavior unrelated to error classification or the extracted use case.
