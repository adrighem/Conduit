# Maintenance Patterns

## 2026-06-17

- No public backlog exists yet, so maintenance attention should focus on release readiness, onboarding clarity, issue templates, and CI coverage.
- The repository currently relies on README guidance for Slack app setup; future user reports may cluster around OAuth redirect setup, Socket Mode app-level tokens, and GNOME dependency installation.

## 2026-06-18

- Local repo health and GitHub Actions are green, so the highest-leverage maintenance work is planned UX architecture and documentation rather than reactive triage.
- Sidebar improvements should keep pure grouping/sorting behavior testable outside GTK widget construction to avoid growing `src/window.rs` into a maintenance hotspot.

## 2026-06-19

- Public backlog is still empty, so maintainer leverage remains preventive: complete local UX slices, improve intake templates, and tighten release-readiness docs before outside reports arrive.
- The sidebar slice validates the useful pattern from the plan: keep Slack conversation classification/grouping in a pure module with unit tests, and let `window.rs` only render and wire GTK controls.
- The current repository still has no issue template, PR template, contributing guide, code of conduct, or security policy; those are the next support-load reducers once the sidebar worktree is clean.

## 2026-06-19 Execution

- Contributor intake should bias toward reproducibility and secret hygiene: Slack tokens, OAuth codes, workspace secrets, and private messages are the support data most likely to leak accidentally.
- CI Clippy should wait until the available Clippy driver matches the active Rust toolchain; a mismatched `cargo-clippy` binary can fail before linting project code.
