# Code Review Findings Remediation

## Summary

A full-codebase senior-engineer review (174k lines of Rust) was performed on 2026-09-08 by seven parallel review agents, each covering a subsystem: auth/network core (`auth.rs`, `socket_mode.rs`, `slack.rs`, `slack_link.rs`), the data/sync layer (`store.rs`, `workspace_pipeline.rs`, `sync_scheduler.rs`), HTML/message rendering (`message_html.rs` and friends), the recently-decomposed `window.rs`/`runtime.rs` controllers, sidebar/composer/state (`sidebar.rs`, `workspace_state.rs`, `composer.rs`, `models.rs`), the huddles (voice/video) subsystem, and the remaining misc source files. Each finding was rated Critical/High/Medium/Low across five categories: correctness, performance, readability, error handling, and security.

No Critical findings were found. This track exists to work through the High, Medium, and Low findings so a future agent (or the same one, later) can pick up individual tasks, investigate the specific claim against current code (code may have moved since the review), and land a fix.

## Requirements

1. For each task below, first re-verify the finding against the current state of the file/line — code may have shifted since the review snapshot (2026-09-08, commit `18cc5db`). If the finding no longer applies, mark the task done with a note explaining why, rather than force a fix.
2. Fixes must be minimal and scoped to the specific defect — no unrelated refactors bundled into a finding's fix.
3. Any fix touching `store.rs`, `workspace_pipeline.rs`, or `workspace_state.rs` hot paths (the per-event full-clone/full-sort performance findings) must include a benchmark or before/after reasoning showing the change actually reduces the cost, not just a claim.
4. Any fix touching security-relevant surfaces (huddles fd handling, D-Bus search provider, log injection, URL scheme allowlists) must explain the exact attack/failure this closes, in the commit message or task summary.
5. Existing tests must continue to pass; new tests should be added for any correctness bug fix (regression test that fails before the fix, passes after) per `workflow.md`'s TDD task workflow.
6. Do not delete the dead-code duplicate HTML rendering pipeline (`message_html.rs`) or the dead `services/workspace_service.rs` module without explicit user confirmation first, even though the review recommends it — confirm intent before removing code that might be a deliberate future seam.

## Acceptance Criteria

- Every task in `plan.md` is either fixed with a passing regression test, or explicitly marked as "verified not applicable" with reasoning.
- `cargo fmt --check`, `cargo clippy`, `cargo test`, and `meson compile -C _build` / `meson test -C _build` all pass after each phase.
- No new Critical/High findings are introduced by the fixes themselves (the fixer should sanity-check adjacent code for the same class of bug when fixing one instance).

## Out of Scope

- Re-running the full review from scratch. This track is remediation-only, based on the 2026-09-08 findings.
- The huddles GStreamer/WebRTC media pipeline redesign — only the specific fd-ownership finding is in scope, not a broader audit.
- Wiring `services/workspace_service.rs` into production use, or deleting it — needs a separate product decision (flag with the user).
- Any UX/product decisions implied by findings (e.g. whether `msteams:`/`zoommtg:` links should ever be clickable in message bodies at all) — fixes should preserve existing product behavior while closing the specific gap described.
