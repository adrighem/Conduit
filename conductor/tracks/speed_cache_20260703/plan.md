# Track Plan

## Phase 1: Stable Sidebar During Conversation Refresh
- [x] Task: Keep populated sidebar interactive during refresh 6eac9a8
  - [x] Sub-task: Write tests for sidebar render policy when loading with existing conversations
  - [x] Sub-task: Avoid list replacement for loading/error states when conversations are already available
  - [x] Sub-task: Preserve selection, filters, toggles, and list scroll during refresh
  - [x] Sub-task: Reduce avoidable full sidebar rebuilds from status-only updates
- [ ] Task: Conductor - User Manual Verification 'Stable Sidebar During Conversation Refresh' (Protocol in workflow.md)

## Phase 2: Fast Channel Opening And Bounded History Cache
- [ ] Task: Render cached channel history immediately while refreshing latest history
  - [ ] Sub-task: Write tests for cached-vs-fresh history event handling
  - [ ] Sub-task: Keep cached loads from clearing fresh pagination/read-state metadata
  - [ ] Sub-task: Continue fresh latest-page loading after cached history is shown
  - [ ] Sub-task: Avoid duplicate in-flight history requests for the same channel
- [ ] Task: Cache merged paged history without building an unbounded archive
  - [ ] Sub-task: Write tests for merged history storage and pruning behavior
  - [ ] Sub-task: Store merged history after older-page loads
  - [ ] Sub-task: Keep the cache focused on recent and explicitly loaded messages
- [ ] Task: Conductor - User Manual Verification 'Fast Channel Opening And Bounded History Cache' (Protocol in workflow.md)

## Phase 3: Bottom Anchoring And Auto-Scroll
- [ ] Task: Default channel timelines to the latest messages
  - [ ] Sub-task: Write tests for scroll-intent decisions
  - [ ] Sub-task: Scroll to the bottom after initial channel render and sent-message render
  - [ ] Sub-task: Keep bottom anchoring when the user was already at the bottom
  - [ ] Sub-task: Avoid stealing scroll when the user is reading older messages
- [ ] Task: Preserve reading position while loading older messages
  - [ ] Sub-task: Write tests or a small harness for prepend scroll behavior
  - [ ] Sub-task: Maintain visible position when older messages are prepended above the current viewport
  - [ ] Sub-task: Keep the top **Load older messages** affordance reachable
- [ ] Task: Conductor - User Manual Verification 'Bottom Anchoring And Auto-Scroll' (Protocol in workflow.md)
