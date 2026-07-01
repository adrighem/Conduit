# Track Plan

## Phase 1: Member-Scoped Sidebar Loading [checkpoint: d797e7c]
- [x] Task: Use member-scoped conversations and resilient sidebar refresh 0daa8e9
  - [x] Sub-task: Add tests for conversation rate-limit policy and debug logging limits
  - [x] Sub-task: Use `users.conversations` for sidebar conversation loading
  - [x] Sub-task: Keep conversation refresh failures scoped to sidebar state
  - [x] Sub-task: Keep debug logs compact by default
- [x] Task: Conductor - User Manual Verification 'Member-Scoped Sidebar Loading' (Protocol in workflow.md) d797e7c

## Phase 2: Slack-Like Visible Conversation Set [checkpoint: ff4ca7e]
- [x] Task: Add active-only sidebar filtering with all-conversations override 87b007c
  - [x] Sub-task: Write tests for visible conversation classification
  - [x] Sub-task: Add the sidebar override control
  - [x] Sub-task: Preserve search and unread filtering behavior
- [x] Task: Conductor - User Manual Verification 'Slack-Like Visible Conversation Set' (Protocol in workflow.md) ff4ca7e

## Phase 3: Ctrl-K Conversation Switcher
- [~] Task: Add Ctrl-K conversation switcher
  - [ ] Sub-task: Write tests for switcher filtering and activation helpers
  - [ ] Sub-task: Add modal dialog UI and shortcut wiring
  - [ ] Sub-task: Select conversations from all loaded conversations
- [ ] Task: Conductor - User Manual Verification 'Ctrl-K Conversation Switcher' (Protocol in workflow.md)

## Phase 4: Quick History Switching
- [ ] Task: Reuse in-memory history before fresh network loading
  - [ ] Sub-task: Write tests for history load decision helpers
  - [ ] Sub-task: Avoid duplicate `LoadHistory` commands for current cached conversation history
  - [ ] Sub-task: Keep explicit refresh behavior available
- [ ] Task: Conductor - User Manual Verification 'Quick History Switching' (Protocol in workflow.md)
