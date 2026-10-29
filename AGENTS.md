# Conduit Project Guidance

- Conduit intentionally supports one connected Slack workspace. Treat single-workspace operation as a product boundary, not as a temporary limitation or a future multi-workspace roadmap gap.
- Run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` before every push. Enforce via `.git/hooks/pre-push`.

## Testing

- Graphical tests (anything that creates GTK widgets or windows, or needs a display) skip themselves unless `CONDUIT_GRAPHICAL_TESTS=1`. Plain `cargo test` never initialises GTK or opens a window. Never run them on a desktop session.
- Put every graphical test in a `mod graphical` and start its body with `run_gtk_test(...)` (`src/timeline/tests.rs`). The module path is what selects them, so a graphical test outside `graphical::` would run with the plain suite.
- The pre-push hook and CI run them under Xvfb. The filter selects exactly the graphical tests:

  ```sh
  xvfb-run -a env CONDUIT_GRAPHICAL_TESTS=1 GDK_BACKEND=x11 GSK_RENDERER=cairo GSETTINGS_BACKEND=memory GTK_A11Y=none cargo test graphical::
  ```

  With the variable set, a GTK that cannot initialise fails the test instead of skipping. Meson runs the same command as the "Graphical tests" target (`meson test --suite graphical`); "Cargo tests" runs everything else once, without a display.
- Wait on conditions with the shared `pump_until` (and `pump` for a fixed settle) from `src/timeline/tests.rs`, not ad-hoc loops or sleeps.

## Performance

Freezes come from work on the GTK main thread. These rules keep known hitches out; the `perf_guard_*` tests enforce them. The display-free guards run in plain `cargo test perf_guard` and meson "Cargo tests"; the GTK-backed guards (timeline rebinds) are graphical and run in the "Graphical tests" Xvfb target, so a full check needs both runs.

- Timeline re-renders rebind only rows a context change reaches (`MessageRenderContext::delta_from`) and keep rows whose message renders the same (`SlackMessage::renders_same`). Never put per-render values (tokens, scroll intent, image maps) into row-compared context. An arriving image rebinds only the rows that use it.
- Never probe image headers with gdk-pixbuf on the GTK thread for PNG, GIF, JPEG or WebP: `Pixbuf::file_info` is an out-of-process glycin call of tens of milliseconds. Use `image_header::declared_header`.
- Render the sidebar through `queue_ui_invalidations(UiInvalidations::SIDEBAR)` (one render per frame), not `render_conversations()`.
- The conversation picker is a virtualized `GtkListView`: data changes queue `UiInvalidations::PICKER` (one rebuild per frame, only while a picker is open), a picker that is not mapped only marks itself stale (`PickerRefreshGate`), and picker sorts compute each row's sort key once. A data refresh applies keyed edits (`picker_edits`) and updates changed rows in place, so the scroll position and selection survive; only a new query resets to the top.
- People picker and `@`/`#`/`:` completion refreshes after data changes go through `PEOPLE_PICKER` and `COMPLETION` invalidations: nothing runs without an open picker or an active completion token, and an unchanged people list is not rebuilt. The people picker is a virtualized `GtkListView` (`PeoplePickerModel`) that splices only added, removed or renamed people and keeps checked people in a set, so checks and scroll position survive refreshes. Completion searches a cached `CompletionIndex` (fields normalized and sort keys computed once, dropped through `invalidate_completion_data` when users, groups, channels or custom emoji change); a query that extends the previous one filters the previous matches (`CompletionMatches`) instead of the whole index, and suggestion rows with an unchanged entry keep their widgets. Ranking changes must keep the index and the full-scan reference tests equal.
- Keep startup network fan-out bounded: history prefetch is capped at 12 conversations, run 3 at a time, and skipped when the cache is current. Bulk background store writes delay realtime messages through `store_batch_admission`.
- Startup and reconnect never fan out per item: user names come from one `users.list` (skipped while the persisted directory is under 6 h old). Channel discovery is a persisted, resumable sweep: stored channels hydrate at startup, a sweep runs at most every 24 h (starting 60 s after login or on first picker use, pages 4 s apart, capped per run, cursor persisted), and unknown `<#C...>` mention names are resolved on demand with at most 3 `conversations.info` calls at once (`DiscoveryGate`, `ChannelNameLookups`).
- Measure with `cargo run -- --debug` and the `scope="performance"` log lines before and after a change to these paths. Add a count-based `perf_guard_*` test for any new hot path.
