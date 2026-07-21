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

## 2026-07-13 CI Follow-up

- Scope: investigate the latest failing CI action and implement the minimal local fix.
- GitHub Actions: runs `29235054733`, `29232864322`, and `29232284948` failed at strict Clippy on a constant runtime assertion.
- Local fix: converted the About-logo size invariant to a const assertion compatible with Clippy 1.97.
- Local checks:
  - `cargo fmt --check`: pass
  - `cargo test --all-targets`: pass, 284 tests
  - `meson compile -C _build`: pass
  - `meson test -C _build --print-errorlogs`: pass, 4 tests
  - `git diff --check`: pass
- Exact Clippy validation is unavailable locally because this system has no Clippy component; the raw GitHub job log supplies the lint and prescribed fix.
- No issue, pull request, label, release, or comment action was taken; the focused CI-fix commit is authorized for push by this request.

## 2026-07-14 Maintainer Pass

- GitHub: 1 open issue (ISSUE:1), 0 open pull requests, 0 unread notifications, 0 Dependabot alerts, and 0 code-scanning alerts.
- Latest remote CI and CodeQL passed on `f03ed8b`.
- ISSUE:1 is actionable: pointer focus keeps one quick-action toolbar open while hover reveals another; the fix should replace `:focus-within` visibility with keyboard-specific focus behavior.
- Current local work: DM synchronization fix in README/runtime/store/workspace state; 290 Rust tests pass.
- Local checks:
  - `cargo fmt --check`: pass
  - `cargo test --all-targets`: pass, 290 tests
  - `meson compile -C _build`: pass
  - `meson test -C _build --print-errorlogs`: pass, 4 tests
  - `git diff --check`: pass
  - strict Clippy: unavailable locally
- The installed maintainer package contains no referenced triage script or reference guides, so this run used the documented manual fallback.
- No public issue/PR or repository-setting action was taken.

## 2026-07-14 Post-Feature Audit

- GitHub backlog: 7 open issues (ISSUE:1–7), 0 open pull requests, 0 unread notifications, 0 Dependabot alerts, and 0 code-scanning alerts.
- Commit `8b3965d` implements the core behavior across all seven issues, but acceptance review recommends closing only ISSUE:1, ISSUE:4, and ISSUE:6 after green CI.
- Keep ISSUE:2 open for bounded attachment-cache eviction, ISSUE:3 for conversation-pane-wide paste, ISSUE:5 for a genuinely shared picker boundary, and ISSUE:7 for subsearch/metadata hardening and installed GNOME validation.
- CodeQL run `29325605218` passed. CI run `29325605695` failed at strict Clippy before tests and Meson ran; the minimal fix is validated in a local commit awaiting push approval.
- Local baseline before the lint fix: 327 Rust tests and all 4 Meson tests passed.
- The installed maintainer package still lacks its referenced scripts and reference guides, so this run used `gh-helper`, `gh`, and manual intent/decision analysis.
- No public issue, label, closure, release, or repository-setting action was taken.

## 2026-07-14 Acceptance-Gap Follow-up

- Current public backlog: ISSUE:2, ISSUE:3, ISSUE:5, and ISSUE:7 remain open; ISSUE:1, ISSUE:4, and ISSUE:6 were closed after CI and CodeQL passed on `855bd8b`.
- Implemented locally: bounded attachment cache eviction and UTF-8-safe filenames; conversation-pane screenshot paste; shared emoji picker model plus reaction keyboard navigation; lightweight active-workspace GNOME index, real subsearch, stale filtering, metadata validation, and live D-Bus activation smoke coverage.
- Local checks: formatting and all-target checks pass; 341 Rust tests pass; Meson compile and all 6 available Meson tests pass, including search-provider metadata and live D-Bus smoke tests.
- Exact stable Clippy remains unavailable locally and must be verified by GitHub Actions before closure.
- No public issue, closure, comment, label, or push action was taken in this follow-up.

## 2026-07-21 Maintainer Pass

- Scope: manual maintainer pass for the first release PR and the new architecture/performance backlog.
- GitHub backlog: 6 open issues (ISSUE:9–14), 1 open pull request (PR:8), 0 unread Conduit notifications, 0 Dependabot alerts, 0 code-scanning alerts, 0 secret-scanning alerts, and 0 repository security advisories.
- PR:8 review:
  - one verified same-repository `github-actions[bot]` commit based directly on `b93e743`
  - complete diff is limited to the release manifest, generated changelog, and AppStream release date
  - CodeQL passes, but CI run `29817695695` is `action_required` with zero jobs
  - the PR head fails `tests/test_release_automation.py` because the test rejects the expected post-bootstrap manifest
  - a minimal test adjustment accepting an empty or synchronized manifest passes all 5 release metadata checks
  - generated notes incorrectly associate browser-session Socket Mode with closing ISSUE:7
- Current-tree checks under a sanitized environment:
  - `cargo fmt --check`: pass
  - `cargo test --locked`: pass, 543 tests
  - `git diff --check`: pass
- Latest remote main checks: CI run `29817583831` and CodeQL pass on `b93e743`.
- Branch state: local `main` matches `origin/main`; existing user work remains in `eu.vanadrighem.conduit.json` plus two untracked files.
- Backlog order: ISSUE:11 → ISSUE:12 with ISSUE:13 extractions; ISSUE:9 in Phase 4; ISSUE:10 as a separate measured optimization; ISSUE:14 gates PR:8.
- Notes: the installed maintainer package still lacks its referenced scripts and reference guides, so this run used `gh-helper`, `gh`, and manual analysis.
- No public GitHub action was taken.

## 2026-07-21 First-release Stabilization

- Scope: implement the initial-sync affordance and profile emoji fix, resolve ISSUE:14 locally, and harden the first package-publication gates.
- Local product changes:
  - the workspace is insensitive and visibly dimmed until the first successful sync, while later reconnects stay interactive
  - profile structured metadata remains literal text; only the Slack status field retains emoji rendering, and expiration timezones are de-duplicated
  - one connected Slack workspace is recorded as intentional product scope
- Local release changes:
  - Release Please metadata accepts both valid bootstrap manifest states
  - Debian, RPM, and Flatpak release builds explicitly disable unavailable native huddle media and screen sharing
  - three unused direct dependencies and their transitive lock/Flatpak source entries were removed
  - Flatpak installation validation now gates asset publication
- Checks, all run with an allowlisted sanitized environment where inherited state could be recorded:
  - `cargo fmt --all -- --check`: pass
  - `cargo test --locked`: pass, 546 tests
  - release automation: pass, 7 tests, including synthetic manifest acceptance/rejection
  - workflow YAML and JSON parsing: pass
  - Flatpak manifest expansion: pass
  - Meson configure and compile: pass with native media and screen sharing disabled
  - `meson test -C _build`: pass, 14 tests
  - `git diff --check`: pass
- Remaining validation: the opt-in media feature build lacks local `gstreamer-webrtc-1.0` development metadata; real Debian/RPM/Flatpak jobs and manual release smoke checks remain remote/manual gates.
- Generated Meson text and JSON test logs were removed after recording the pass counts.
- Existing user-owned changes in the root development Flatpak manifest and untracked research/backup files were preserved.
- No public GitHub action was taken.

## 2026-07-21 Approved Stabilization Push

- Pushed four focused commits from `b93e743` through `a242401` to `main`; user-owned changes in the root development Flatpak manifest and untracked research/backup files remained unstaged.
- Remote `main` checks on `a242401`:
  - CI `29824946777`: pass, including formatting, strict Clippy, 546 default tests, optional native-huddle lint/tests, and both Meson builds
  - CodeQL `29824945608`: pass for Rust, Actions, JavaScript/TypeScript, and Python
  - Release `29824946738`: pass; Release Please updated PR:8 and correctly did not create a release
- PR:8 actions:
  - approved the required CI run after Release Please refreshed the branch
  - generated head `b230059` passed CI `29825087286` and CodeQL `29825084839`
  - removed the false ISSUE:7 closure attribution from `CHANGELOG.md` and the PR body in commit `517a45e`
  - corrected head `517a45e` passed CI runs `29825962522` and `29825965362` plus CodeQL `29825963617`
- PR:8 remains open, clean, mergeable, and unmerged. ISSUE:14 remains open. No release, issue comment, label, closure, or merge was performed.
- Remaining release gates: complete the manual first-release checklist, then explicitly approve merging PR:8 and monitor the first real Debian/RPM/Flatpak build-install-publication workflow.
- Non-blocking annotations: GitHub forced the Node.js 20 implementations of `actions/checkout@v4` and `release-please-action@v4` onto Node.js 24.

## 2026-07-21 First-release Publication

- Scope: merge PR:8, recover the first draft release safely, fix package-pipeline failures, and verify the public artifacts.
- PR:8 was squash-merged as `49e9203`; ISSUE:14 remained open and the merge did not create a closing reference.
- Release recovery fixes:
  - `bad40b8` installed Git for RPM source-archive creation.
  - `dbcf786` added a stable-tag-validated recovery path for an existing draft.
  - `8003f95` installed Git before Fedora checkout.
  - `8c16452` trusted the checked-out RPM workspace and removed an RPM-lint spelling false positive.
- Final remote validation on `8c16452`:
  - CI `29835081152`: pass for formatting, both strict-Clippy/test configurations, and both Meson builds.
  - CodeQL `29835080623`: pass for Actions, JavaScript/TypeScript, Python, and Rust.
  - guarded Release `29835203473`: pass for draft selection; Debian, RPM, and Flatpak build/install validation; checksums; and publication.
- Published `v0.1.0` at exact tag target `8c16452` with `conduit_0.1.0-1_amd64.deb`, `conduit-0.1.0-1.fc44.x86_64.rpm`, `conduit-0.1.0-x86_64.flatpak`, and `SHA256SUMS`; an independent download check verified all three package hashes.
- Credential rotation was treated as complete per operator instruction; no credential or secret values were inspected or recorded.
- Final audit: 6 open issues, PR:15 is the only open pull request, no unread notifications, and no Dependabot or code-scanning alerts. ISSUE:14 remains open for dependency-count and clean-build timing evidence.
- Existing user-owned changes in the root development Flatpak manifest and untracked research/backup files remained unstaged and preserved.
- The installed maintainer package still lacks its referenced checklist, so this run used the documented manual fallback.
