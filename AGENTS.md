# Conduit Project Guidance

- Conduit intentionally supports one connected Slack workspace. Treat single-workspace operation as a product boundary, not as a temporary limitation or a future multi-workspace roadmap gap.
- Run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` before every push. Enforce via `.git/hooks/pre-push`.

## Performance

Freezes come from work on the GTK main thread. These rules keep known hitches out; the `perf_guard_*` tests enforce them (`cargo test perf_guard`, and the "Performance guards" meson test under Xvfb in CI).

- Timeline re-renders rebind only rows whose inputs changed (`MessageRenderContext::same_row_inputs`). Never put per-render values (tokens, scroll intent, image maps) into row-compared context. An arriving image rebinds only the rows that use it.
- Never probe image headers with gdk-pixbuf on the GTK thread for PNG, GIF, JPEG or WebP: `Pixbuf::file_info` is an out-of-process glycin call of tens of milliseconds. Use `image_header::declared_header`.
- Render the sidebar through `queue_ui_invalidations(UiInvalidations::SIDEBAR)` (one render per frame), not `render_conversations()`.
- Keep startup network fan-out bounded: history prefetch is capped at 12 conversations, run 3 at a time, and skipped when the cache is current. Bulk background store writes delay realtime messages through `store_batch_admission`.
- Startup and reconnect never fan out per item: user names come from one `users.list` (skipped while the persisted directory is under 6 h old), and channel/people/user-group discovery starts 60 s after login or on first picker or completion use, at most hourly, with `conversations.list` pages paced 3 s apart (`DiscoveryGate`).
- Measure with `cargo run -- --debug` and the `scope="performance"` log lines before and after a change to these paths. Add a count-based `perf_guard_*` test for any new hot path.
