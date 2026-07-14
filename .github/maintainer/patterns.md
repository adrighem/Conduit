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

## 2026-06-19 Clippy Gate

- Clippy is now part of the standard maintenance gate. Keep local docs, PR templates, and CI aligned on the exact command: `cargo clippy --all-targets -- -D warnings`.
- Treat new Clippy warnings as code review signals before changing lint policy; suppressions should be local and justified.

## 2026-06-19 Maintainer Activation

- Empty public backlog means release readiness remains the highest-leverage maintenance track: land coherent local slices, keep CI green, and reduce future support load before outside reports arrive.
- Local commits ahead of `origin/main` should be pushed only after the dirty worktree is intentionally resolved, because current remote CI results do not cover the local six-commit stack plus sidebar follow-up edits.
- Sidebar UX changes are easiest to maintain when behavior, tests, and docs move together; avoid splitting row activation semantics from the design contract unless there is a clear review reason.

## 2026-06-19 Recommendation Implementation

- Keep security-sensitive intake guidance close to issue templates, not only in contributor docs, because issue forms are where accidental disclosure is most likely.
- For early projects with empty public backlogs, small preventive policy files can be higher leverage than opening internal tracking issues.

## 2026-06-20 Maintainer Pass

- Theme-dependent icon names are brittle for core navigation. Prefer app-owned symbolic icons when the desired metaphor is not available in stock Adwaita.
- Diagnostic logging that prints Slack conversation objects should preserve unknown API fields, because unread state often depends on fields the UI does not yet model.
- Keep cache files under the XDG cache root. WebKit persistent storage and app-managed image preview caches should not write to data directories unless the data is user-owned state.

## 2026-06-20 Modernization Follow-up

- After a broad modernization stack lands with green CI and an empty public backlog, maintenance risk shifts from implementation throughput to real-workspace integration, setup clarity, and release packaging.
- New Slack API scopes should be documented as migration-sensitive changes because existing keyring tokens may not have the permissions expected by newer UI paths.
- Keep advanced Slack product surfaces as link-and-handoff experiences unless a stable lightweight API makes native support cheap to test and maintain.

## 2026-07-03 Maintainer Pass

- Scope changes need user-facing reconnect guidance, not just a current scope list, because Slack may keep older OAuth grants for existing testers.
- Release readiness checks should explicitly verify scope migration notes whenever requested Slack permissions change.
- With GitHub backlog and security alerts empty, packaging and real-workspace smoke tests are higher leverage than more feature breadth.

## 2026-07-10 CI Follow-up

- CI tracks the stable Rust toolchain with `-D warnings`, so new Clippy releases can fail an unchanged codebase. Run the exact strict Clippy command immediately before pushing.
- Prefer small idiomatic fixes for newly enforced lints over broad lint suppression; in this case, concise enum variants and a derived `Default` preserve behavior while reducing maintenance code.

## 2026-07-10 Architecture and UX Modernization

- Historical search context must be modeled separately from normal history; treating a bounded context page as history silently corrupts navigation and pagination semantics.
- Async composer/upload completion should identify one in-flight submission and compare against current text before clearing persistent drafts.
- Desktop notification targets and persisted user content need both workspace and user identity, even before multi-workspace switching exists.

## 2026-07-14 Maintainer Pass

- Realtime delivery and unread metadata are insufficient unless unopened conversation bodies are also cached and survive stale in-flight history responses.
- CSS `:focus-within` does not distinguish keyboard focus from pointer clicks. For hover-only affordances, preserve keyboard access with `:focus-visible` or explicit input-modality state.
- Keep product fixes separate from repository-policy changes so branch protection and workflow upgrades remain deliberate and reversible.

## 2026-07-14 Post-Feature Audit

- Issue footers are traceability, not proof of acceptance. Audit each acceptance list before closure, especially for lifecycle cleanup, interaction scope, and claims of shared architecture.
- Stable-Clippy drift continues to make local parity important; broad feature batches should run the exact CI lint before push or pin a reviewed Rust toolchain.
- Desktop integrations need an installed-session smoke test in addition to pure model tests because metadata discovery and D-Bus activation can fail outside unit-test coverage.

## 2026-07-14 Acceptance-Gap Follow-up

- Desktop search providers should read a deliberately small index, not deserialize the application's full offline state on the UI/D-Bus thread.
- Cross-toolkit reuse is best expressed as a shared model and behavior contract; native GTK and WebView frontends still need thin platform-specific adapters.
- Window-level keyboard routing can extend actions across a conversation surface, but must explicitly exclude editable/search/outside controls to preserve normal input behavior.
