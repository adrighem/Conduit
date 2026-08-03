# Lazy WebView and Emoji Materialization

## Summary

Reduce startup memory and timeline document cost by creating the thread WebView only when a
thread is first opened and by replacing the eagerly rendered emoji catalogue with a lightweight
picker shell backed by bounded native queries.

## Requirements

1. No thread WebView or secondary renderer is created before the first thread is opened.
2. Thread close and reopen behavior follows a measured, documented retain-or-release policy that
   preserves navigation, focus, and scroll expectations.
3. Initial message documents contain only a lightweight emoji-picker shell, not the full Unicode
   or workspace emoji catalogue.
4. Picker queries use the shared `EmojiPickerModel`, return bounded results, reject stale
   generations, and never expose untrusted values as executable JavaScript.
5. Category browsing, custom emoji, type-ahead search, keyboard navigation, accessible labels,
   Escape cancellation, focus restoration, and reaction selection preserve the behavior delivered
   for issues #4 and #5.
6. The native status emoji chooser uses the reaction picker's search, category, grid, and bounded
   paging layout without creating another WebView.
7. Adding or removing a reaction does not reset the timeline scroll position.
8. Release-build measurements record initial HTML size, cold render time, first picker-open
   latency, thread reopen latency, and process-tree proportional set size before and after.

## Acceptance Criteria

- A lifecycle regression test proves that startup does not construct the thread WebView.
- Opening the first thread creates one usable WebView; subsequent close and reopen behavior matches
  the documented policy.
- Generated initial timeline HTML does not contain the complete emoji catalogue.
- Picker result materialization has an explicit upper bound independent of catalogue size.
- Picker search, categories, custom emoji, keyboard movement, cancellation, focus restoration, and
  reactions pass automated and headless regression coverage.
- Status emoji selection uses the same bounded category-grid interaction while retaining the
  explicit no-emoji choice and status-dialog save behavior.
- Main and thread timelines retain their scroll position across picker use and reaction updates.
- Reproducible release-build before/after measurements are recorded without credentials or private
  workspace content.

## Out of Scope

- Replacing WebKitGTK or GTK.
- Changing composer `@` or `:` completion.
- Implementing the cached-media URI scheme from issue #9.
- Sharing render processes unless controlled measurements show a clear benefit.
- Aggressive thread renderer teardown without a measured memory benefit and acceptable reopen
  latency.
