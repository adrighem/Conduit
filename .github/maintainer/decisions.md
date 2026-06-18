# Maintainer Decisions

## 2026-06-17

- Initialized maintainer memory for `adrighem/Conduit`.
- Verified there are no open GitHub issues or pull requests at initialization time.
- Verified local CI-equivalent checks pass on the current working tree:
  - `cargo fmt --check`
  - `cargo test`
  - `meson compile -C _build`
  - `meson test -C _build`

## 2026-06-18

- Maintainer workflow continues with manual fallback because the installed skill package does not include its referenced triage script or reference files.
- Sidebar work should proceed as a structured architecture slice using `docs/sidebar-improvement-plan.md` rather than adding more list-rendering logic directly to `src/window.rs`.
- No public GitHub action is needed: there are no open issues or pull requests to triage.
