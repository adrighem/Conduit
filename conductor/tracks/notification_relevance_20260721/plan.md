# Relevant Notifications And Noise-Free Unread State Plan

## Phase 1: Attention domain policy [checkpoint: acd58e1]

- [x] Task: Write the table-driven decision matrix for message kinds, unread actions, notification actions, and relevance reasons b3d14b1
- [x] Task: Normalize lifecycle noise, direct messages, mentions, name/alias matches, keyword/phrase matches, and relevant thread replies b3d14b1
- [x] Task: Implement the pure AttentionPolicy inputs and AttentionDecision outputs with no GTK, GSettings, Slack API, or storage dependency b3d14b1
- [x] Task: Conductor - User Manual Verification 'Attention domain policy' (Protocol in workflow.md) acd58e1

## Phase 2: Canonical pipeline integration [checkpoint: d8dc411]

- [x] Task: Route realtime, snapshot, and local message candidates through AttentionPolicy before coordinator effects fan out 760fe7c
- [x] Task: Apply the unread decision consistently to StoreBatch persistence, sidebar/activity projection, and reconciliation without losing raw Slack counters 760fe7c
- [x] Task: Apply notification effects with display-name resolution, active-target suppression, freshness, and persistent deduplication 760fe7c
- [x] Task: Add integration regressions for join/leave suppression, relevant thread replies, reconnect redelivery, and read-marker behavior 760fe7c
- [x] Task: Conductor - User Manual Verification 'Canonical pipeline integration' (Protocol in workflow.md) d8dc411

## Phase 3: Preferences and live configuration [checkpoint: 96cb4cb]

- [x] Task: Add versioned GSettings keys and defaults for notification enablement, direct messages, mentions/names, thread replies, aliases, and keywords 276dbe7
- [x] Task: Add an accessible Notifications group to Preferences with validation and concise matching guidance 276dbe7
- [x] Task: Feed live preference snapshots into AttentionPolicy without restarting or reconnecting 276dbe7
- [x] Task: Add schema, binding, default-value, and live-update tests 276dbe7
- [x] Task: Conductor - User Manual Verification 'Preferences and live configuration' (Protocol in workflow.md) 96cb4cb

## Phase 4: Hardening, measurement, and documentation

- [ ] Task: Add attention counters and structured reasons for decisions without logging message content or configured keywords
- [ ] Task: Measure a realtime burst for classification cost, queue growth, duplicate delivery, and unread reconciliation behavior
- [ ] Task: Update user and architecture documentation for relevant-only notifications and the raw-unread/attention distinction
- [ ] Task: Run formatting, unit, integration, Meson, GSettings, GTK, and WebKit validation suites
- [ ] Task: Conductor - User Manual Verification 'Hardening, measurement, and documentation' (Protocol in workflow.md)
