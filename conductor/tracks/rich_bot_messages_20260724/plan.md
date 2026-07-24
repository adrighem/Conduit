# Rich Bot Messages Plan

## Phase 1: Lossless message model and cache

- [x] Task: Add failing model and cache round-trip tests for attachments and bot identity 15509ae
- [x] Task: Retain typed legacy attachments, controls, and bot/app identity 15509ae
- [x] Task: Centralize visible/accessibility fallback and notification eligibility 15509ae
- [ ] Task: Conductor - User Manual Verification 'Lossless message model and cache' (Protocol in workflow.md)

## Phase 2: Rich presentation and honest controls

- [x] Task: Add failing renderer tests for Bob-like attachments and Jira-like Block Kit 15509ae
- [x] Task: Render common legacy attachments, rich text, headers, fields, selects, and overflow 15509ae
- [x] Task: Add safe control capability resolution and opaque callback-free action URLs 15509ae
- [x] Task: Resolve bot/app author names and avatars with stable grouping 15509ae
- [ ] Task: Conductor - User Manual Verification 'Rich presentation and honest controls' (Protocol in workflow.md)

## Phase 3: Exact Slack handoff and integration

- [x] Task: Add failing permalink API and message-control routing tests 15509ae
- [x] Task: Implement chat.getPermalink and external exact-message handoff 15509ae
- [x] Task: Route control activation through typed runtime commands and refresh-safe state 15509ae
- [x] Task: Verify Web API, realtime, coordinator, and store replacement behavior 15509ae
- [ ] Task: Conductor - User Manual Verification 'Exact Slack handoff and integration' (Protocol in workflow.md)

## Phase 4: Acceptance and documentation

- [~] Task: Run formatting, strict Clippy, Rust tests, Meson compilation, and Meson tests
- [x] Task: Synchronize architecture and product documentation 15509ae
- [ ] Task: Conductor - User Manual Verification 'Acceptance and documentation' (Protocol in workflow.md)
