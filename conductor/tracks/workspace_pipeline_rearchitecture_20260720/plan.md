# Workspace Pipeline Rearchitecture Plan

## Phase 1: Guardrails and persistent storage

- [x] Task: Record the investigation baseline and coordinator/store architecture in Conductor documentation 8c16f8b
- [x] Task: Add schema-v2 freshness metadata and derived-cache recovery coverage 4e306e2
- [x] Task: Introduce StoreHub with bounded writer/readers, commit barriers, and clean shutdown 9e4b31a
- [x] Task: Move bootstrap, conversation, user, history, thread, and freshness access onto focused repository operations 1cd7ebe
- [~] Task: Add write batching, immediate user flushes, unchanged suppression, and store tracing counters
- [ ] Task: Conductor - User Manual Verification 'Guardrails and persistent storage' (Protocol in workflow.md)

## Phase 2: Canonical reducer pipeline

- [ ] Task: Define and test workspace mutations, patches, store batches, revisions, and snapshot envelopes
- [ ] Task: Extract WorkspaceCoordinator and its pure reducer from the runtime
- [ ] Task: Route cache, Web API, local actions, and realtime transports through the reducer adapter
- [ ] Task: Preserve read overlays and deduplicate message/send/echo identities with timeline invariants
- [ ] Task: Conductor - User Manual Verification 'Canonical reducer pipeline' (Protocol in workflow.md)

## Phase 3: Bounded synchronization and backpressure

- [ ] Task: Define and test SyncJob priorities, freshness, coalescing, cancellation, and bounded lanes
- [ ] Task: Move startup, manual refresh, navigation, and membership-event scheduling onto the bounded scheduler
- [ ] Task: Enforce startup enrichment/history limits and lazy 24-hour user-directory loading
- [ ] Task: Add scheduler/API tracing counters and no-realtime stale-check behavior
- [ ] Task: Conductor - User Manual Verification 'Bounded synchronization and backpressure' (Protocol in workflow.md)

## Phase 4: Incremental GTK and WebKit presentation

- [ ] Task: Define SidebarProjection keyed splice/update/reset behavior with 1,430-row regression tests
- [ ] Task: Migrate the sidebar to GtkListView, gio::ListStore, and stable single selection
- [ ] Task: Define TimelinePresenter document/revision/loading/delta behavior with scroll regression tests
- [ ] Task: Route one batched TimelineDelta per frame and restrict full document loads
- [ ] Task: Add the MIME-checked conduit-asset cache-key scheme and remove nested root resize observers
- [ ] Task: Conductor - User Manual Verification 'Incremental GTK and WebKit presentation' (Protocol in workflow.md)

## Phase 5: Expansion and cleanup

- [ ] Task: Migrate remaining workspace surfaces onto coordinator intents and projections
- [ ] Task: Remove whole-state storage, raw realtime UI events, broad invalidations, and routine reload adapters
- [ ] Task: Add settled-idle counters and run full automated acceptance validation
- [ ] Task: Synchronize final architecture documentation
- [ ] Task: Conductor - User Manual Verification 'Expansion and cleanup' (Protocol in workflow.md)
