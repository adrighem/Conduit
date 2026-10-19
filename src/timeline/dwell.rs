use gtk::prelude::*;
use gtk::Widget;

pub const READ_DWELL: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(50)
} else {
    std::time::Duration::from_secs(2)
};

/// A row qualifies as "read" for auto-mark-read purposes once at least 80%
/// of either its own height or the viewport's height is within the visible
/// range `[0, viewport_height]` (`row_top`/`row_bottom` in the same
/// coordinate space, i.e. relative to the viewport's own origin).
pub fn row_qualifies_for_read(row_top: f64, row_bottom: f64, viewport_height: f64) -> bool {
    let row_height = row_bottom - row_top;
    if row_height <= 0.0 || viewport_height <= 0.0 {
        return false;
    }
    let overlap = (row_bottom.min(viewport_height) - row_top.max(0.0)).max(0.0);
    overlap / row_height >= 0.8 || overlap / viewport_height >= 0.8
}

/// Recovers the message-ts tag set via `set_widget_name` in
/// `wrap_with_unread_separator_if_needed`. `GtkListView` realizes each row
/// behind its own internal wrapper widget, so the tagged content widget is
/// one or more levels below whatever `list_view.first_child()`/`next_sibling()`
/// yields, not the wrapper itself (which always has an empty name).
pub fn row_message_ts(widget: &Widget) -> Option<String> {
    let name = widget.widget_name();
    if crate::models::is_slack_timestamp(&name) {
        return Some(name.to_string());
    }
    // GtkListView row items wrap message box inside internal wrapper widget.
    // Inspect direct children rather than recursing deeply into message components.
    let mut child = widget.first_child();
    while let Some(candidate) = child {
        let name = candidate.widget_name();
        if crate::models::is_slack_timestamp(&name) {
            return Some(name.to_string());
        }
        child = candidate.next_sibling();
    }
    None
}
