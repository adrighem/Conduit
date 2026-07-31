# Workspace Pipeline Rearchitecture Plan

## Phase 1: Guardrails and persistent storage [checkpoint: 7c8428e]

- [x] Task: Record the investigation baseline and coordinator/store architecture in Conductor documentation 8c16f8b
- [x] Task: Add schema-v2 freshness metadata and derived-cache recovery coverage 4e306e2
- [x] Task: Introduce StoreHub with bounded writer/readers, commit barriers, and clean shutdown 9e4b31a
- [x] Task: Move bootstrap, conversation, user, history, thread, and freshness access onto focused repository operations 1cd7ebe
- [x] Task: Add write batching, immediate user flushes, unchanged suppression, and store tracing counters 2b255e6
- [x] Task: Conductor - User Manual Verification 'Guardrails and persistent storage' (Protocol in workflow.md) 7c8428e

## Phase 2: Canonical reducer pipeline [checkpoint: 854789b]

- [x] Task: Define and test workspace mutations, patches, store batches, revisions, and snapshot envelopes 918ae95
- [x] Task: Extract WorkspaceCoordinator and its pure reducer from the runtime 5ab1767
- [x] Task: Route cache, Web API, local actions, and realtime transports through the reducer adapter 67c3a5a
- [x] Task: Preserve read overlays and deduplicate message/send/echo identities with timeline invariants a7e44a2
- [x] Task: Conductor - User Manual Verification 'Canonical reducer pipeline' (Protocol in workflow.md) 854789b

## Phase 3: Authoritative conversation adoption [checkpoint: da64ac5]

- [x] Task: Define failing end-to-end tests for authoritative conversation membership and metadata e113608
- [x] Task: Execute each coordinator StoreBatch atomically and in revision order 640658d
- [x] Task: Deliver revisioned conversation WorkspacePatch values to presentation 0e39acd
- [x] Task: Route cache, API, and realtime conversation changes through the coordinator 52741ca
- [x] Task: Remove the replaced legacy persistence, runtime events, and GTK catalog mutations 2a3c560
- [x] Task: Prevent native reload of internal message WebViews from navigating away 03717c1
- [x] Task: Conductor - User Manual Verification 'Authoritative conversation adoption' (Protocol in workflow.md) da64ac5

## Phase 4: Bounded synchronization and backpressure [checkpoint: 34ebd6c]

- [x] Task: Integrate the validated bounded SyncJob scheduler after authoritative conversation adoption 4c43f9f
- [x] Task: Move startup, manual refresh, navigation, and membership-event scheduling onto the bounded scheduler 0e0804a
- [x] Task: Enforce startup enrichment/history limits and lazy 24-hour user-directory loading 5a044a0
- [x] Task: Add scheduler/API tracing counters and no-realtime stale-check behavior 6e9b437
- [x] Task: Conductor - User Manual Verification 'Bounded synchronization and backpressure' (Protocol in workflow.md) 34ebd6c

## Phase 5: Incremental GTK and WebKit presentation

- [ ] Task: Define SidebarProjection keyed splice/update/reset behavior with 1,430-row regression tests
- [ ] Task: Migrate the sidebar to GtkListView, gio::ListStore, and stable single selection
- [ ] Task: Define TimelinePresenter document/revision/loading/delta behavior with scroll regression tests
- [ ] Task: Route one batched TimelineDelta per frame and restrict full document loads
- [ ] Task: Add the MIME-checked conduit-asset cache-key scheme and remove nested root resize observers
- [ ] Task: Conductor - User Manual Verification 'Incremental GTK and WebKit presentation' (Protocol in workflow.md)

## Phase 6: Expansion and cleanup

- [ ] Task: Migrate remaining workspace surfaces onto coordinator intents and projections
- [ ] Task: Remove whole-state storage, raw realtime UI events, broad invalidations, and routine reload adapters
- [ ] Task: Add settled-idle counters and run full automated acceptance validation
- [ ] Task: Synchronize final architecture documentation
- [ ] Task: Conductor - User Manual Verification 'Expansion and cleanup' (Protocol in workflow.md)
