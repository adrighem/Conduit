# Lazy WebView and Emoji Materialization Plan

## Phase 1: Bounded on-demand emoji picker [checkpoint: cc481cf]

- [x] Task: Record release-build baselines and add failing bounded-picker document tests dbc7969
- [x] Task: Define a generation-scoped native picker query and result protocol d9382e8
- [x] Task: Replace eager emoji markup with a lightweight picker shell and bounded materialization c115324
- [x] Task: Preserve search, categories, custom emoji, keyboard, cancellation, focus, and reactions c115324
- [x] Task: Align the native status emoji picker with the bounded reaction-picker layout 47ee7eb
- [x] Task: Conductor - User Manual Verification 'Bounded on-demand emoji picker' (Protocol in workflow.md) cc481cf

## Phase 2: Lazy thread WebView lifecycle [checkpoint: d2292ea]

- [x] Task: Add failing lifecycle coverage proving no thread WebView exists at startup 466b6f1
- [x] Task: Make ThreadPane create its WebView on the first thread open 9e415cf
- [x] Task: Implement and document the measured close and reopen lifecycle 929dbf8
- [x] Task: Record release-build HTML, latency, scroll, and process-tree PSS results 8dee011
- [x] Task: Conductor - User Manual Verification 'Lazy thread WebView lifecycle' (Protocol in workflow.md) d2292ea

## Phase 3: Animated status emoji [checkpoint: 378a585]

- [x] Task: Add failing status-picker coverage for animated custom emoji 520dab2
- [x] Task: Preserve animation when rendering custom emoji in the status picker a4fcab4
- [x] Task: Conductor - User Manual Verification 'Animated status emoji' (Protocol in workflow.md) 378a585

## Phase 4: Selected status emoji preview [checkpoint: 085e93a]

- [x] Task: Add failing coverage for the selected custom emoji preview a01e496
- [x] Task: Render selected custom emoji images and animation beside status text 51522cd
- [x] Task: Conductor - User Manual Verification 'Selected status emoji preview' (Protocol in workflow.md) 085e93a

## Phase 5: Usage-ranked quick reactions [checkpoint: e359afa]

- [x] Task: Add failing ranking, bounded-history, four-button, and hover-close coverage bf96cc2
- [x] Task: Rank four quick reactions by frequency across the latest 20 additions and close overflow on hover end 8528d0a
- [x] Task: Conductor - User Manual Verification 'Usage-ranked quick reactions' (Protocol in workflow.md) e359afa

## Phase 6: Slack skin-tone emoji modifiers [checkpoint: 125ccff]

- [x] Task: Add failing shared-catalog and surface coverage for adjacent Slack skin-tone modifiers c686215
- [x] Task: Resolve compatible Slack skin-tone modifiers through every catalog-backed renderer 206a712
- [x] Task: Conductor - User Manual Verification 'Slack skin-tone emoji modifiers' (Protocol in workflow.md) 125ccff
