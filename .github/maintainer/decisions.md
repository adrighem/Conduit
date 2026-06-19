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

## 2026-06-19

- Continued maintainer workflow with manual fallback because the installed skill package still does not include its referenced triage script or reference files.
- Verified live GitHub backlog remains empty: no open or historical issues or pull requests returned by `gh`.
- Verified recent GitHub Actions are healthy: latest CI and CodeQL runs on `main` completed successfully.
- Sidebar implementation is now the highest-priority local release-readiness slice to finish and commit: it restores native GTK navigation with tested grouping/sorting behavior and currently leaves the worktree dirty.
- No public GitHub action is needed today.

## 2026-06-19 Execution

- Committed the native GTK sidebar slice first as `b97419f`.
- Added preventive intake and contributor guidance in `0230dc5`: GitHub issue forms, PR template, and `CONTRIBUTING.md`.
- Did not add Clippy to CI yet. `cargo-clippy` is installed locally, but the installed `clippy-driver` is version `0.1.87` and behaves as Rust 1.87, while current resolved dependencies require Rust 1.88 or newer. The normal `cargo` and `rustc` are 1.95.
