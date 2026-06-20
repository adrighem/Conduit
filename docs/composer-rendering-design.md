# Composer And Rendering Design

## Assessment

This feature makes sense for a lightweight GNOME Slack client when implemented as the common daily-use subset.

Conduit should not attempt full Slack WYSIWYG or Block Kit authoring. It should provide a reliable native text composer, enough formatting affordance for normal Slack `mrkdwn`, and rendering improvements that keep real Slack messages understandable.

## Current Slice

This slice implements:

- Multiline GTK composers for channel messages and thread replies.
- Bounded composer height so longer drafts can scroll without resizing the whole window.
- `Ctrl+Enter` send from both composers while plain Enter inserts a newline.
- Existing send and upload buttons remain unchanged.
- Edited-message metadata in rendered messages.
- Deleted-message rendering that avoids the misleading "No message text" fallback.

This keeps the composer native and practical without turning the current message pane into a full rich-text editor.

## UI

The single-line `GtkEntry` composers are replaced with `GtkTextView` widgets inside `GtkScrolledWindow` containers.

The composer is intentionally plain text. Users can type Slack `mrkdwn` directly, and formatting helpers can later insert syntax around the current selection.

## Rendering

The renderer already handles common Slack text constructs:

- Inline code and code blocks.
- Bold, italic, and strike markers.
- Mentions, channels, links, emoji shortcodes, reactions, threads, files, images, and common Block Kit blocks.

This slice adds explicit edited and deleted message states. These are small but important because they prevent normal Slack lifecycle events from looking like malformed messages.

## Deferred

The following still belongs in later slices:

- Formatting toolbar buttons for bold, italic, strike, code, quote, code block, ordered list, and bullet list.
- Emoji picker and custom emoji cache.
- Mention and channel autocomplete.
- Draft persistence in the cache.
- Paste handling for files and images.
- Broader Block Kit layout coverage.
- Full WYSIWYG editing.

## Tests

The implementation should keep unit coverage for:

- Edited metadata rendering.
- Deleted message rendering.
- Existing `mrkdwn`, reaction, attachment, and image rendering behavior.
- Composer UI XML validity and full build integration.
