# Track Plan

## Phase 1: Stable Sidebar During Conversation Refresh [checkpoint: 956004c]
- [x] Task: Keep populated sidebar interactive during refresh 6eac9a8
  - [x] Sub-task: Write tests for sidebar render policy when loading with existing conversations
  - [x] Sub-task: Avoid list replacement for loading/error states when conversations are already available
  - [x] Sub-task: Preserve selection, filters, toggles, and list scroll during refresh
  - [x] Sub-task: Reduce avoidable full sidebar rebuilds from status-only updates
- [x] Task: Suppress sidebar rebuilds from user-name refresh churn db93e6b
  - [x] Sub-task: Write tests for user-name sidebar render policy during refresh
  - [x] Sub-task: Render sidebar only when a loaded user changes a visible DM row
  - [x] Sub-task: Defer user-name sidebar rerenders while conversation refresh is active
- [x] Task: Conductor - User Manual Verification 'Stable Sidebar During Conversation Refresh' (Protocol in workflow.md) 956004c

## Phase 2: Fast Channel Opening And Bounded History Cache [checkpoint: 06d4863]
- [x] Task: Render cached channel history immediately while refreshing latest history 033b3f6
  - [x] Sub-task: Write tests for cached-vs-fresh history event handling
  - [x] Sub-task: Keep cached loads from clearing fresh pagination/read-state metadata
  - [x] Sub-task: Continue fresh latest-page loading after cached history is shown
  - [x] Sub-task: Avoid duplicate in-flight history requests for the same channel
- [x] Task: Cache merged paged history without building an unbounded archive c60ae35
  - [x] Sub-task: Write tests for merged history storage and pruning behavior
  - [x] Sub-task: Store merged history after older-page loads
  - [x] Sub-task: Keep the cache focused on recent and explicitly loaded messages
- [x] Task: Conductor - User Manual Verification 'Fast Channel Opening And Bounded History Cache' (Protocol in workflow.md) 06d4863

## Phase 3: Bottom Anchoring And Auto-Scroll
- [x] Task: Default channel timelines to the latest messages cb80c3c
  - [x] Sub-task: Write tests for scroll-intent decisions
  - [x] Sub-task: Scroll to the bottom after initial channel render and sent-message render
  - [x] Sub-task: Keep bottom anchoring when the user was already at the bottom
  - [x] Sub-task: Avoid stealing scroll when the user is reading older messages
- [x] Task: Preserve reading position while loading older messages 66910c5
  - [x] Sub-task: Write tests or a small harness for prepend scroll behavior
  - [x] Sub-task: Maintain visible position when older messages are prepended above the current viewport
  - [x] Sub-task: Keep the top **Load older messages** affordance reachable
- [ ] Task: Conductor - User Manual Verification 'Bottom Anchoring And Auto-Scroll' (Protocol in workflow.md)
