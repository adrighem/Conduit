# Lazy WebView and Emoji Materialization Plan

## Phase 1: Bounded on-demand emoji picker

- [~] Task: Record release-build baselines and add failing bounded-picker document tests
- [ ] Task: Define a generation-scoped native picker query and result protocol
- [ ] Task: Replace eager emoji markup with a lightweight picker shell and bounded materialization
- [ ] Task: Preserve search, categories, custom emoji, keyboard, cancellation, focus, and reactions
- [ ] Task: Conductor - User Manual Verification 'Bounded on-demand emoji picker' (Protocol in workflow.md)

## Phase 2: Lazy thread WebView lifecycle

- [ ] Task: Add failing lifecycle coverage proving no thread WebView exists at startup
- [ ] Task: Make ThreadPane create its WebView on the first thread open
- [ ] Task: Implement and document the measured close and reopen lifecycle
- [ ] Task: Record release-build HTML, latency, scroll, and process-tree PSS results
- [ ] Task: Conductor - User Manual Verification 'Lazy thread WebView lifecycle' (Protocol in workflow.md)
