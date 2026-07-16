# Runtime Architecture Hardening Plan

## Phase 1: Typed failure boundaries [checkpoint: 44964d4]

- [x] Task: Document the typed-error and tracing stack choices before implementation 015ca08
- [x] Task: Introduce typed Slack boundary errors with category coverage e26d6de
- [x] Task: Introduce typed store boundary errors with category coverage 8c0279d
- [x] Task: Carry structured runtime failures into operation-local UI recovery 792d61d
- [x] Task: Conductor - User Manual Verification 'Typed failure boundaries' (Protocol in workflow.md) 44964d4

## Phase 2: Structured observability

- [x] Task: Initialize a safe tracing subscriber and bridge existing diagnostics ca201ae
- [x] Task: Instrument runtime commands and asynchronous work with structured spans 9d0f4c5
- [x] Task: Add observability regression tests and secret-redaction coverage 5d391d8
- [x] Task: Preserve requested diagnostics across GTK activation f1494e5
- [~] Task: Conductor - User Manual Verification 'Structured observability' (Protocol in workflow.md)

## Phase 3: Workspace lifecycle state

- [ ] Task: Define and test pure workspace lifecycle transitions
- [ ] Task: Drive runtime lifecycle events from authentication, sync, disconnect, and recovery paths
- [ ] Task: Render lifecycle-driven GTK status and recovery behavior
- [ ] Task: Conductor - User Manual Verification 'Workspace lifecycle state' (Protocol in workflow.md)

## Phase 4: Application service boundary

- [ ] Task: Define narrow Slack and store ports for one conversation use case
- [ ] Task: Extract and test the conversation use case in a headless application service
- [ ] Task: Route the runtime and GTK shell through the extracted service
- [ ] Task: Run full regression validation and synchronize approved architecture documentation
- [ ] Task: Conductor - User Manual Verification 'Application service boundary' (Protocol in workflow.md)
