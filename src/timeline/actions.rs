use std::cell::RefCell;
use std::rc::Rc;

use gtk::{glib, Widget};

pub(crate) type OpenMediaCallback = Rc<dyn Fn(crate::window::MediaGalleryItem)>;

pub(crate) type ActionHandler = Rc<dyn Fn(TimelineAction)>;
pub(crate) type ActionSlot = Rc<RefCell<Option<ActionHandler>>>;
pub(crate) type BoundItems = Rc<RefCell<Vec<glib::WeakRef<gtk::ListItem>>>>;

pub(crate) type ReadMarkTarget = Rc<RefCell<Option<(String, Option<String>)>>>;
pub(crate) type ToggleReactionCallback = Rc<dyn Fn(String, String, bool)>;

/// Invokes the timeline action handler without holding the `RefCell` borrow:
/// handlers may re-render the view, which replaces the handler via
/// `set_on_action` and would otherwise panic with "already borrowed".
pub(crate) fn dispatch_timeline_action(
    on_action: &RefCell<Option<ActionHandler>>,
    action: TimelineAction,
) {
    let handler = on_action.borrow().clone();
    if let Some(handler) = handler {
        handler(action);
    }
}

#[derive(Clone, Debug)]
pub(crate) enum TimelineAction {
    OpenMedia(crate::window::MediaGalleryItem),
    OpenThread(String),
    ToggleReaction {
        ts: String,
        name: String,
        add: bool,
    },
    ForwardMessage(String),
    /// Download a non-media file attachment and open it with the default app.
    DownloadFile {
        url: String,
        name: String,
    },
    MarkUnread(String),
    CopyMessageLink(String),
    CopyMessageText(String),
    CopyMessageWithMetadata(String),
    /// Open (or create) the direct message with this user.
    MessageUser(String),
    /// A link in message text; Slack message permalinks open in Conduit.
    OpenLink(String),
    /// Show the native profile dialog for this user.
    ShowProfile(String),
    /// Show the members of this user group (`S…` ID).
    ShowUserGroup(String),
    /// Fired by the visibility-based auto-read-marking mechanism once a
    /// message has dwelled sufficiently on screen. Carries its own
    /// channel_id/thread_ts explicitly rather than relying on the handler
    /// to infer "whichever conversation is currently visible", since the
    /// dwell timer can fire slightly after the user has navigated away.
    AutoMarkRead {
        channel_id: String,
        thread_ts: Option<String>,
        ts: String,
    },
    ExecuteControlAction {
        ts: String,
        key: crate::rich_message::MessageControlKey,
    },
    /// The user scrolled near the oldest loaded message: fetch the previous
    /// page. Carries the surface it came from, like `AutoMarkRead`. The
    /// handler answers with `NativeTimelineView::set_loading_older`.
    LoadOlder {
        channel_id: String,
        thread_ts: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub enum HoverEvent {
    Enter {
        row: Widget,
        ts: String,
        reactions: Vec<crate::emoji::EmojiEntry>,
    },
    Leave,
}

#[derive(Clone, Debug)]
pub struct HoveredMessage {
    pub row: Widget,
    pub ts: String,
}
