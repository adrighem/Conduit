# Rich Bot Messages Plan

## Phase 1: Lossless message model and cache

- [~] Task: Add failing model and cache round-trip tests for attachments and bot identity
- [ ] Task: Retain typed legacy attachments, controls, and bot/app identity
- [ ] Task: Centralize visible/accessibility fallback and notification eligibility
- [ ] Task: Conductor - User Manual Verification 'Lossless message model and cache' (Protocol in workflow.md)

## Phase 2: Rich presentation and honest controls

- [ ] Task: Add failing renderer tests for Bob-like attachments and Jira-like Block Kit
- [ ] Task: Render common legacy attachments, rich text, headers, fields, selects, and overflow
- [ ] Task: Add safe control capability resolution and opaque callback-free action URLs
- [ ] Task: Resolve bot/app author names and avatars with stable grouping
- [ ] Task: Conductor - User Manual Verification 'Rich presentation and honest controls' (Protocol in workflow.md)

## Phase 3: Exact Slack handoff and integration

- [ ] Task: Add failing permalink API and message-control routing tests
- [ ] Task: Implement chat.getPermalink and external exact-message handoff
- [ ] Task: Route control activation through typed runtime commands and refresh-safe state
- [ ] Task: Verify Web API, realtime, coordinator, and store replacement behavior
- [ ] Task: Conductor - User Manual Verification 'Exact Slack handoff and integration' (Protocol in workflow.md)

## Phase 4: Acceptance and documentation

- [ ] Task: Run formatting, strict Clippy, Rust tests, Meson compilation, and Meson tests
- [ ] Task: Synchronize architecture and product documentation
- [ ] Task: Conductor - User Manual Verification 'Acceptance and documentation' (Protocol in workflow.md)
