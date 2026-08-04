# Rich Bot Messages Plan

## Phase 1: Lossless message model and cache [checkpoint: 32e7dfa]

- [x] Task: Add failing model and cache round-trip tests for attachments and bot identity 15509ae
- [x] Task: Retain typed legacy attachments, controls, and bot/app identity 15509ae
- [x] Task: Centralize visible/accessibility fallback and notification eligibility 15509ae
- [x] Task: Add failing decoder, canonical-document, sensitive-value, cache-version, and fresh-over-old replacement tests e41bb50
- [x] Task: Introduce a bounded Slack wire decoder and canonical message author/document/control model e41bb50
- [x] Task: Derive fallback, accessibility, notifications, and mentions from the canonical document e41bb50
- [x] Task: Persist versioned canonical content while preserving compatibility with existing cache rows e41bb50
- [x] Task: Conductor - User Manual Verification 'Lossless message model and cache' (Protocol in workflow.md) 32e7dfa

## Phase 2: Rich presentation and honest controls [checkpoint: 2fb9ce8]

- [x] Task: Add failing renderer tests for Bob-like attachments and Jira-like Block Kit 15509ae
- [x] Task: Render common legacy attachments, rich text, headers, fields, selects, and overflow 15509ae
- [x] Task: Add safe control capability resolution and opaque callback-free action URLs 15509ae
- [x] Task: Resolve bot/app author names and avatars with stable grouping 15509ae
- [x] Task: Add failing render-plan, author-capability, attachment-color, malformed-sibling, and DOM/accessibility tests e41bb50
- [x] Task: Introduce a pure capability resolver and renderer-neutral message render plan e41bb50
- [x] Task: Render canonical nodes without raw Slack JSON and make bot/app identity behavior consistent e41bb50
- [x] Task: Extract owned rich-message CSS and complete structured attachment and image-accessory presentation e41bb50
- [x] Task: Conductor - User Manual Verification 'Rich presentation and honest controls' (Protocol in workflow.md) 2fb9ce8

## Phase 3: Exact Slack handoff and integration [checkpoint: 92438ba]

- [x] Task: Add failing permalink API and message-control routing tests 15509ae
- [x] Task: Implement chat.getPermalink and external exact-message handoff 15509ae
- [x] Task: Route control activation through typed runtime commands and refresh-safe state 15509ae
- [x] Task: Verify Web API, realtime, coordinator, and store replacement behavior 15509ae
- [x] Task: Add failing opaque-handle lifecycle, handoff policy, safe-URL, and external-opener tests e41bb50
- [x] Task: Implement presenter-owned opaque control handles scoped to session, generation, message revision, and capability e41bb50
- [x] Task: Extract exact-message handoff behind typed provider, validation, provenance, cache, and opener ports e41bb50
- [x] Task: Reject stale, replayed, cross-session, and forged activations and suppress duplicate in-flight handoffs e41bb50
- [x] Task: Conductor - User Manual Verification 'Exact Slack handoff and integration' (Protocol in workflow.md) 92438ba

## Phase 4: Acceptance and documentation [checkpoint: c17fdea]

- [x] Task: Run formatting, strict Clippy, Rust tests, Meson compilation, and Meson tests cd28961
- [x] Task: Synchronize architecture and product documentation 15509ae
- [x] Task: Add shared synthetic Bob/Jira fixtures and contributor documentation for extending rich messages e41bb50
- [x] Task: Run formatting, strict Clippy, Rust tests, Meson builds, DOM/headless tests, Release, and CodeQL 2628864
- [x] Task: Conductor - User Manual Verification 'Acceptance and documentation' (Protocol in workflow.md) c17fdea

## Phase 5: URL preview images [checkpoint: 07fa1e0]

- [x] Task: Add failing policy and canonical-discovery coverage for URL preview images b20288a
- [x] Task: Preserve public direct preview images and discover canonical private images c3a7eb1
- [x] Task: Harden repeated headless window activation during CI validation 60a64b7
- [x] Task: Conductor - User Manual Verification 'URL preview images' (Protocol in workflow.md) 07fa1e0

## Phase 6: Animated GIF shares

- [x] Task: Add failing Block Kit, file-share, asset-request, and render coverage for animated GIFs 7b9ab05
- [x] Task: Preserve Slack file images and animated GIF thumbnails through normalization and rendering 8129099
- [~] Task: Conductor - User Manual Verification 'Animated GIF shares' (Protocol in workflow.md)
