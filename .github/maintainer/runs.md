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

## 2026-06-20 Maintainer Pass

- Scope: manual maintainer pass after image caching, sidebar icon, and conversation unread diagnostics work.
- GitHub backlog: 0 open issues and 0 open pull requests returned by `gh`.
- Recent GitHub Actions: latest visible CI and CodeQL runs on `main` are passing.
- Commits created locally:
  - `af53fa2` `Persist message image caches`: uses persistent WebKit cache/data directories under the app cache root and caches authenticated Slack image preview data URIs.
  - `fec82d8` `Log conversation properties in debug mode`: preserves unknown Slack conversation fields and prints full conversation JSON under `--debug`.
  - `4b7fe94` `Bundle public channel sidebar icon`: adds an app-owned symbolic public-channel icon and updates sidebar docs.
- Local checks:
  - `xmllint --noout src/conduit.gresource.xml src/icons/hicolor/scalable/status/channel-public-symbolic.svg`: pass
  - `cargo fmt --check`: pass
  - `cargo test`: pass, 34 tests
  - `cargo clippy --all-targets -- -D warnings`: pass
  - `meson compile -C _build`: pass
  - `meson test -C _build`: pass, 4 Meson tests
  - `gresource list _build/src/conduit.gresource`: includes `channel-public-symbolic.svg`
- Notes:
  - The installed maintainer skill is still missing its referenced scripts and reference files, so this run used the manual fallback.
  - No public GitHub action was taken. Pushing the local commits requires explicit approval.

## 2026-06-20 Modernization Follow-up

- Scope: manual maintainer pass after the modernization build-out through `50f4c05` (`Render Slack product surface action links`).
- GitHub backlog: 0 open issues, 0 open pull requests, 0 historical issues, and 0 historical pull requests returned by `gh`.
- Recent GitHub Actions: latest visible CI and CodeQL runs on `main` are passing for `50f4c05`.
- Branch state:
  - `main` is aligned with `origin/main`.
  - Worktree was clean before recording this maintainer update.
- Local checks:
  - `cargo fmt --check`: pass
  - `cargo test`: pass, 60 tests
  - `cargo clippy --all-targets -- -D warnings`: pass
  - `meson compile -C _build`: pass
  - `meson test -C _build`: pass, 4 Meson tests
- Recommendations:
  - Run a real-workspace release smoke pass before adding more broad Slack surface area.
  - Document OAuth scope migration expectations for users who authenticated before read markers and file access were added.
  - Keep Flatpak and release packaging as the next preventive support-load reducer.
- Notes:
  - The installed maintainer skill is still missing its referenced scripts and reference files, so this run used the manual fallback.
  - No issue, pull request, label, release, or public comment action was taken.

## 2026-07-03 Maintainer Pass

- Scope: manual maintainer pass after the Slack parity track completed through `9623c82` (`conductor(plan): Complete Slack parity track`).
- GitHub backlog: 0 unread notifications, 0 open issues, 0 open pull requests, 0 Dependabot alerts, 0 code scanning alerts, 0 historical issues, and 0 historical pull requests returned by `gh-helper` and `gh`.
- Recent GitHub Actions: latest visible CI and CodeQL runs on `main` are passing for `9623c82`.
- Branch state:
  - `main` is aligned with `origin/main` at `9623c82`.
  - Worktree had prior uncommitted maintainer-memory updates before this pass; this run preserved them and added release-readiness docs.
- Local changes:
  - Added README guidance for testers who authenticated before new Slack scopes were added: sign out, confirm scopes, and reconnect.
  - Added a release-checklist gate to verify scope documentation and reconnect instructions when requested scopes change.
- Local checks:
  - `cargo fmt --check`: pass
  - `cargo test`: pass, 73 tests
  - `cargo clippy --all-targets -- -D warnings`: pass
  - `meson compile -C _build`: pass
  - `meson test -C _build`: pass, 4 Meson tests
  - `git diff --check`: pass
- Recommendations:
  - Run a real-workspace release smoke pass with OAuth reconnect before adding more broad Slack surface area.
  - Start Flatpak dependency vendoring and sandbox checks next, because packaging is now the highest support-risk area.
  - Keep Slack scope changes paired with explicit migration notes.
- Notes:
  - The installed maintainer skill is still missing its referenced scripts and reference files, so this run used the manual fallback.
  - No issue, pull request, label, release, or public comment action was taken.

## 2026-07-10 CI Follow-up

- Scope: investigate failed CI run `29090777347` and prepare a local fix.
- Cause: stable Clippy rejected the sidebar placeholder enum naming and a manual `Default` implementation on `dee54bc`.
- Local fix: renamed internal placeholder variants and derived `Default` without changing UI behavior.
- Local checks:
  - `cargo fmt --check`: pass
  - `cargo clippy --all-targets -- -D warnings`: pass
  - `cargo test`: pass, 132 tests
  - clean Meson setup and compile: pass
  - `meson test -C _build`: pass, 4 tests
- No public GitHub action was taken. Pushing the local commits requires explicit approval.

## 2026-07-10 Architecture and UX Modernization

- Scope: verify and document the architecture/UX modernization stack before the user-authorized push.
- GitHub backlog: 0 unread notifications, 0 open issues, 0 open pull requests, 0 Dependabot alerts, and 0 code scanning alerts.
- GitHub Actions: failed run `29090777347` was caused by strict Clippy; follow-up run `29091467301` is green on `origin/main` at `227b099`.
- Local checks:
  - `cargo fmt --check`: pass
  - `cargo test`: pass, 194 tests
  - `cargo clippy --all-targets --all-features -- -D warnings`: pass
  - `meson compile -C _build`: pass
  - `meson test -C _build --print-errorlogs`: pass, 4 tests
  - `git diff --check`: pass
- Notes: no public issue/PR actions were taken; the user explicitly requested the branch push.
