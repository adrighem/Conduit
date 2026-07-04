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

## 2026-06-19 Clippy Gate

- After `cargo-clippy` was upgraded to match Rust 1.95, enabled `cargo clippy --all-targets -- -D warnings` in CI.
- Fixed the current warning set before adding the gate, so the lint check is immediately actionable rather than aspirational.
- Updated contributor and pull request checklists to include the same Clippy command.

## 2026-06-19 Maintainer Activation

- Continued maintainer workflow with manual fallback because the installed skill package still lacks its referenced triage script and reference files.
- No public GitHub action is needed: the open issue and pull request backlog is empty.
- Treat the current local branch state as the immediate maintenance priority. The repo is six commits ahead of `origin/main` and has uncommitted sidebar follow-up work; remote CI has not validated that combined state yet.
- Keep the sidebar follow-up grouped as one reviewable local change set because the code, row behavior tests, README wording, and sidebar design docs describe the same UX contract.

## 2026-06-19 Recommendation Implementation

- Implemented the activation recommendations locally instead of opening public backlog items, because the requested work was directly actionable in the repository.
- Kept sidebar row activation polish, tests, README updates, and sidebar design docs in a single commit to preserve review context.
- Added a lightweight security reporting policy and issue-template redirect to reduce the chance of Slack tokens, OAuth codes, or private workspace data being posted publicly.
- Prepared the local stack for push after validation; no issue, pull request, label, release, or public comment action was taken.

## 2026-06-20 Maintainer Pass

- Continued maintainer workflow with manual fallback because the installed skill package still lacks its referenced triage script and reference files.
- No issue or PR triage action is needed: the open GitHub backlog is empty.
- Kept the session work split into focused local commits: cache behavior, debug diagnostics, and sidebar icon resources.
- Treated `--debug` conversation dumps as sensitive diagnostics and documented that they may include private workspace metadata while still avoiding access tokens and authorization codes.
- Did not push the local commits because pushing is a public repository action and needs explicit approval.

## 2026-06-20 Modernization Follow-up

- Continued maintainer workflow with manual fallback because the installed skill package still lacks its referenced triage script and reference files.
- No issue or PR triage action is needed: the open and fetched historical GitHub backlog is empty.
- Treat the post-modernization feature set as ready for release-readiness validation rather than more breadth-first feature work.
- Prioritize real-workspace smoke testing, OAuth scope migration notes, and Flatpak/release packaging because those reduce likely user setup and support failures.
- No issue, pull request, label, release, or public comment action was taken.

## 2026-07-03 Maintainer Pass

- Continued maintainer workflow with manual fallback because the installed skill package still lacks its referenced triage script and reference files.
- No public triage action is needed: inbox notifications, issues, pull requests, Dependabot alerts, code scanning alerts, historical issues, and historical pull requests are all empty.
- Implemented the low-risk OAuth scope migration documentation locally instead of opening a tracking issue.
- Treat real-workspace OAuth reconnect smoke testing and Flatpak packaging validation as the next maintenance priorities.
- No issue, pull request, label, release, or public comment action was taken.
