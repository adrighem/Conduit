# Lazy WebView and Emoji Materialization Plan

## Phase 1: Bounded on-demand emoji picker [checkpoint: cc481cf]

- [x] Task: Record release-build baselines and add failing bounded-picker document tests dbc7969
- [x] Task: Define a generation-scoped native picker query and result protocol d9382e8
- [x] Task: Replace eager emoji markup with a lightweight picker shell and bounded materialization c115324
- [x] Task: Preserve search, categories, custom emoji, keyboard, cancellation, focus, and reactions c115324
- [x] Task: Align the native status emoji picker with the bounded reaction-picker layout 47ee7eb
- [x] Task: Conductor - User Manual Verification 'Bounded on-demand emoji picker' (Protocol in workflow.md) cc481cf

## Phase 2: Lazy thread WebView lifecycle

- [x] Task: Add failing lifecycle coverage proving no thread WebView exists at startup 466b6f1
- [ ] Task: Make ThreadPane create its WebView on the first thread open
- [ ] Task: Implement and document the measured close and reopen lifecycle
- [ ] Task: Record release-build HTML, latency, scroll, and process-tree PSS results
- [ ] Task: Conductor - User Manual Verification 'Lazy thread WebView lifecycle' (Protocol in workflow.md)
