# Maintainer Runs

## 2026-06-17

- Scope: initial manual maintainer pass for `adrighem/Conduit`.
- GitHub backlog: 0 open issues, 0 open pull requests, 0 historical issues or pull requests returned by search.
- Local checks:
  - `cargo fmt --check`: pass
  - `cargo test`: pass, 3 tests
  - `meson compile -C _build`: pass
  - `meson test -C _build`: pass, 4 Meson tests
- Extra checks:
  - `cargo clippy --all-targets -- -D warnings`: not run, `clippy` is unavailable and `rustup` is not installed locally
- Notes: the installed maintainer skill is missing its referenced scripts and reference files, so this run used a manual fallback.

## 2026-06-18

- Scope: manual maintainer pass for `adrighem/Conduit` after thread rendering fixes and sidebar planning.
- GitHub backlog: 0 open issues, 0 open pull requests, 0 historical issues or pull requests reported by `gh repo view`.
- Recent GitHub Actions: latest CI and CodeQL runs on `main` are passing.
- Local checks:
  - `cargo fmt --check`: pass
  - `cargo test`: pass, 19 tests
  - `meson setup _build --reconfigure`: pass
  - `meson compile -C _build`: pass
  - `meson test -C _build`: pass, 4 Meson tests
- Worktree notes:
  - `docs/sidebar-improvement-plan.md` is uncommitted documentation work.
- Notes: the installed maintainer skill is still missing its referenced scripts and reference files, so this run used the manual fallback again.

## 2026-06-19

- Scope: manual maintainer pass for `adrighem/Conduit` after implementing the native GTK sidebar slice.
- GitHub backlog: 0 open issues, 0 open pull requests, 0 historical issues, and 0 historical pull requests returned by `gh`.
- Recent GitHub Actions: latest CI and CodeQL runs on `main` are passing.
- Local checks:
  - `cargo fmt --check`: pass
  - `cargo test`: pass, 24 tests
  - `xmllint --noout src/window.ui`: pass
  - `meson compile -C _build`: pass
  - `meson test -C _build`: pass, 4 Meson tests
  - runtime smoke launch under `xvfb-run`: pass, no template binding failure observed before timeout
- Extra checks:
  - `gtk4-builder-tool validate src/window.ui`: not applicable directly, fails because the tool does not load `AdwApplicationWindow` as a template parent.
  - `cargo clippy --all-targets -- -D warnings`: failed before linting project code because installed `clippy-driver` is 0.1.87/Rust 1.87 while dependencies require Rust 1.88 or newer.
- Worktree notes:
  - Sidebar implementation committed as `b97419f`.
  - Contributor intake files committed as `0230dc5`.
- Notes: the installed maintainer skill is still missing its referenced scripts and reference files, so this run used the manual fallback again.

## 2026-06-19 Clippy Gate

- Scope: enable Clippy after local upgrade to matching Rust 1.95 tooling.
- Local checks:
  - `cargo clippy --version`: `clippy 0.1.95`
  - `clippy-driver --version`: `clippy 0.1.95`
  - `cargo clippy --all-targets -- -D warnings`: pass after mechanical cleanup
- Code cleanup:
  - Removed needless generic borrow in About dialog translator credits.
  - Replaced redundant OAuth expiry closure with function item.
  - Replaced lazy reaction fallback with eager `Option::or`.
  - Replaced `filter(...).next()` with `find(...)`.
  - Elided needless test helper lifetimes.
- Policy update:
  - CI now installs the `clippy` component and runs the warning-deny lint gate.
  - Contributor guide and PR template include the lint command.

## 2026-06-19 Maintainer Activation

- Scope: manual maintainer check after invoking the open-source maintainer workflow.
- GitHub backlog: 0 open issues and 0 open pull requests returned by `gh`.
- Recent GitHub Actions: latest visible CI and CodeQL runs on `origin/main` are passing, but they predate the local six-commit stack.
- Branch state:
  - `main` is ahead of `origin/main` by 6 commits.
  - Worktree has uncommitted sidebar follow-up changes in `README.md`, `src/sidebar.rs`, `src/window.rs`, and `src/window.ui`.
  - Worktree has untracked sidebar docs: `docs/sidebar-1.0-update-plan.md` and `docs/sidebar-design.md`.
- Local checks on the current dirty worktree:
  - `cargo fmt --check`: pass
  - `cargo test`: pass, 30 tests
  - `cargo clippy --all-targets -- -D warnings`: pass
  - `meson compile -C _build`: pass
  - `meson test -C _build`: pass, 4 Meson tests
  - `xmllint --noout src/window.ui`: pass
- Notes: the installed maintainer skill is still missing its referenced scripts and reference files, so this run used the manual fallback again.

## 2026-06-19 Recommendation Implementation

- Scope: implement maintainer recommendations from the activation pass.
- Commits created:
  - `3cebef2` `Polish sidebar row activation`: resolved the sidebar follow-up as one reviewable code/docs change set.
  - `0ab0606` `Add security reporting policy`: added `SECURITY.md` and routed security-sensitive issue-template users to the policy.
- Local checks before committing:
  - `cargo fmt --check`: pass
  - `cargo test`: pass, 30 tests
  - `cargo clippy --all-targets -- -D warnings`: pass
  - `meson compile -C _build`: pass
  - `meson test -C _build`: pass, 4 Meson tests
  - `xmllint --noout src/window.ui`: pass
  - `git diff --check`: pass
  - YAML parse for `.github/ISSUE_TEMPLATE/config.yml` and `.github/ISSUE_TEMPLATE/bug_report.yml`: pass
- Notes: maintainer memory is recorded in this follow-up commit before pushing the local stack.
