# Priority Conversations Plan

## Phase 1: Starred priority conversations [checkpoint: 11efc64]

- [x] Task: Add explicit conversation-star state and a tested Slack toggle request 924854d
- [x] Task: Add the Priority sidebar projection with VIP DMs before starred channels 161cecd
- [x] Task: Add accessible Star and Unstar actions with persisted runtime updates 8c4b604
- [x] Task: Run full sanitized validation and synchronize product documentation 591e437
- [x] Task: Address final priority consistency review findings 16a774b
- [x] Task: Add a DM Profile context action using the existing main-webview flow 8947521
- [x] Task: Add person @ completion to message and thread composers 7c88040
- [x] Task: Conductor - User Manual Verification 'Starred priority conversations' (Protocol in workflow.md) 11efc64

## Phase 2: Reliable new-message picker

- [x] Task: Add failing coverage for opening New message before people discovery completes dd28cd3
- [x] Task: Open New message immediately and refresh it when people arrive d204494
- [~] Task: Conductor - User Manual Verification 'Reliable new-message picker' (Protocol in workflow.md)

## Phase 3: WYSIWYG rich-text composer

- [x] Task: Add failing rich document, draft, and Slack payload coverage 0c48aa4
- [x] Task: Add formatting toolbars, native rich editing, and emoji insertion to message and reply composers 467cc7d
- [x] Task: Send and restore Slack rich-text blocks with accessible plain-text fallbacks 62a0d7a
- [x] Task: Add regressions for compact overflow, rich response rendering, and logical newlines 0512bd8
- [x] Task: Keep composer controls compact and preserve rich formatting and newlines after Send 6921b56
- [~] Task: Make composer toolbar controls square and recalculate overflow
- [ ] Task: Conductor - User Manual Verification 'WYSIWYG rich-text composer' (Protocol in workflow.md)

## Phase 4: Inline staged media

- [x] Task: Add failing staged-media draft, batch-upload, and terminal-progress coverage 0c8f23f
- [x] Task: Preview removable image and video attachments inside both composers e9ed127
- [x] Task: Send staged media and rich text as one Slack file-share message 2bfa87e
- [x] Task: Fix manual composer verification regressions 0b7213f
- [ ] Task: Conductor - User Manual Verification 'Inline staged media' (Protocol in workflow.md)
