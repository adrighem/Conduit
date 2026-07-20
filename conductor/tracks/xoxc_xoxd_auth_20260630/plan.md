# Track Plan

## Phase 1: XOXC/XOXD Browser Session Authentication [checkpoint: b3c064d]
- [x] Task: Add browser-session token import and authenticated Slack requests 3ff62dc
  - [x] Sub-task: Write tests for token import and request headers
  - [x] Sub-task: Implement token model, config, runtime, and Slack request changes
  - [x] Sub-task: Document the environment-variable flow
- [x] Task: Add XOXC/XOXD authentication option to the connect UI ca8c1f1
  - [x] Sub-task: Write tests for browser-session token construction from UI input
  - [x] Sub-task: Add UI controls and mode toggle
  - [x] Sub-task: Wire UI input through runtime validation and keyring storage
- [x] Task: Conductor - User Manual Verification 'XOXC/XOXD Browser Session Authentication' (Protocol in workflow.md) b3c064d

## Phase 2: Browser-session import compatibility
- [x] Task: Align browser-session requests and recovery guidance with the upstream workaround 98d41a8
  - [x] Sub-task: Add request-level coverage for XOXC form authentication and browser cookies
  - [x] Sub-task: Stop substituting an unrelated browser User-Agent
  - [x] Sub-task: Surface actionable browser-session import failures without exposing credentials
  - [x] Sub-task: Document Enterprise User-Agent and TLS-fingerprint requirements
- [ ] Task: Conductor - User Manual Verification 'Browser-session import compatibility' (Protocol in workflow.md)
