# Workspace Pipeline Rearchitecture

## Summary

Incrementally replace Conduit's competing cache, runtime, and UI update paths with one revisioned workspace coordinator. GTK intents, Slack responses, and realtime events enter a bounded scheduler and coordinator; the coordinator emits revisioned `WorkspacePatch` values for presentation and ordered `StoreBatch` values for a persistent SQLite `StoreHub`.

GTK and WebKitGTK remain the presentation stack. Credentials in Secret Service and drafts in GSettings must survive migration. The derived Slack cache may be recreated when migration or corruption recovery requires it.

## Baseline

- `runtime.rs` currently combines supervision, network scheduling, cache orchestration, and workspace business rules.
- `WorkspaceStore` serializes read-modify-write operations but opens SQLite connections per operation and retains whole-state compatibility APIs.
- cached hydration, Web API responses, local actions, and realtime events can follow different mutation and UI-notification paths.
- GTK maintains conversation/thread catalogs and broadly invalidates sidebar or WebKit presentation.
- the sidebar is a widget-heavy `GtkListBox`; message documents still have full-document reload paths and embedded data assets.

## Requirements

1. `WorkspaceCoordinator` is the sole owner of conversations, users, messages, threads, unread overlays, deduplication identities, and monotonically increasing revisions.
2. Cached hydration, Web API results, local sends/actions, and both realtime transports normalize into `WorkspaceMutation`. GTK receives workspace-domain changes only as `WorkspacePatch { revision, changes }` after compatibility adapters are retired.
3. Network work uses bounded, coalescing `SyncJob { key, priority, freshness }` lanes. Capacity is acquired before spawning, maintenance yields to interactive work, and no recurring full-workspace polling loop is introduced.
4. Snapshot results carry `SnapshotEnvelope { base_revision, data }`; stale snapshots merge without rolling back newer realtime/local state, read cursors, or unread overlays.
5. Every logical mutation produces at most one revisioned patch and one atomic `StoreBatch`. Identical data produces neither a persistence write nor a UI patch.
6. Message identity deduplicates local send responses and realtime echoes. Normal thread replies stay in threads; broadcasts may also appear in channel history; all replies update root reply metadata.
7. `StoreHub` owns one long-lived SQLite writer, two query-only readers, bounded queues, commit barriers, and clean shutdown. Maintenance writes batch up to 50 mutations or 50 ms; user work flushes after absorbing queued work.
8. Schema v2 retains keyed JSON payload storage and adds freshness/retry metadata. Failed v1 migration or corrupt derived data recreates only the cache.
9. Startup shows one cached bootstrap projection, starts realtime immediately, conditionally refreshes membership, enriches at most 30 priority conversations, and prefetches at most 12 histories.
10. Bulk users are fetched once on an empty cache; otherwise the directory loads lazily when opened or after 24 hours. Bulk discovery is never followed by per-user refreshes.
11. `SidebarProjection` emits keyed splice/update/reset operations over a virtualized `GtkListView`. Local row changes do not rebuild the full model.
12. Each WebView has a revision-aware `TimelinePresenter` that queues while loading and sends one `TimelineDelta` per frame. Full document loads are limited to initial navigation, revision mismatch, or unrecoverable corruption.
13. Cached assets use a restricted `conduit-asset://<cache-key>` scheme with known-key resolution, bounded reads, and MIME validation.
14. Auth, attachment handling, and the huddle actor remain specialized services supervised by the runtime composition root.
15. Add structured counters for jobs, API requests, SQLite connections/transactions/changed/skipped rows, sidebar operations, document loads, and timeline deltas.

## Acceptance Criteria

- A deterministic 1,430-conversation startup performs membership pagination, no more than 30 enrichments, and no more than 12 history prefetches.
- Queue-capacity, maintenance-preemption, coalescing, cancellation, freshness, retry, and rate-limit behavior are headlessly tested.
- Realtime and local mutations before, during, and after snapshots cannot be rolled back.
- One realtime message creates one patch and transaction; a local send plus realtime echo creates one message and one notification.
- Thread replies never enter channel history unless `thread_broadcast`.
- Store tests cover atomic rollback, unchanged suppression, reader visibility after barriers, shutdown flush, malformed rows, cache recovery, and concurrent search reads.
- One unread update does not reset or rebuild a 1,430-row sidebar model.
- Timeline tests cover revision mismatch, loading queues, batched insert/edit/delete/enrichment, pinned-bottom behavior, non-bottom anchoring, delayed media, and user-scroll cancellation.
- At settled idle, the median of three 60-second samples is below 2% native CPU and below 2% WebKit CPU, with zero store commits, sidebar updates, or document reloads.
- `cargo fmt --check`, strict Clippy, Rust tests, Meson compilation, and Meson tests pass at phase boundaries.

## Out of Scope

- Replacing GTK, WebKitGTK, Tokio, rusqlite, auth/keyring, drafts/GSettings, attachment handling, or the huddle actor.
- Supporting multiple simultaneously active workspaces.
- Changing Conduit's external/public API.
- Introducing a generic dependency-injection framework.
