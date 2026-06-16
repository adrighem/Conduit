# Autonomous Implementation Plan

This plan turns `implementation_plan.text` into buildable vertical slices for Conduit, a native GNOME Slack client.

## Operating Approach

- **Maintainer hat:** protect repo buildability, keep public API/dependency choices conservative, and document deviations from the research plan.
- **Architect hat:** isolate Slack, OAuth, secure storage, async runtime, and GTK UI so each phase can advance without freezing the main thread.
- **Implementer hat:** ship one compiling slice at a time, starting with login and read-only Slack data before write actions.
- **Reviewer hat:** run `cargo check`, `meson compile`, and Meson validation after each slice that changes behavior.
- **Release hat:** only after core flows work, tighten metadata, Flatpak permissions, README, and release notes.

## Subagent Usage

- **Research subagent:** verify current Slack OAuth/PKCE, file upload, Socket Mode, and crate assumptions while implementation begins locally.
- **Build-system subagent:** review Meson, generated resources, Flatpak, and AppStream risks in parallel with feature work.
- **Review subagent:** after each large slice, inspect changed modules for UI-thread blocking, token leakage, and missing error paths.
- **Feature workers:** only use workers for disjoint slices, such as `src/rendering.rs` or docs/metadata, once the shared architecture is stable.

## Execution Slices

1. **Foundation:** add dependencies, module boundaries, an async runtime bridge, and a native UI shell with explicit unauthenticated/authenticated states.
2. **Authentication:** implement Slack PKCE OAuth for desktop/user-token installation, local callback handling, browser launch, and secure token storage.
3. **Read-only Slack:** fetch auth identity, conversations, history, thread replies, saved items, and search through a service layer.
4. **Messaging:** post channel messages and thread replies, then add native notifications for incoming refreshes.
5. **File upload:** implement the modern external upload flow using `files.getUploadURLExternal` and `files.completeUploadExternal`.
6. **Rich rendering:** parse Slack mrkdwn into safe Pango markup, resolve mentions through a user cache, and map Block Kit into GTK widgets.
7. **Realtime:** add Socket Mode as an opt-in advanced path requiring an app-level token, with event acknowledgements on a background runtime.
8. **Packaging:** finalize README, AppStream data, Flatpak permissions, screenshots, and release checks.

## Current Scope

The first autonomous pass implements slices 1-3 as a compiling product skeleton with real OAuth/token storage and Slack Web API service methods. Later slices are wired into the UI as visible placeholders where credentials or broader Slack app configuration are required.
