use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib::subclass::prelude::*;
use gtk::{
    gio, glib, pango, Box, Button, CssProvider, Image, Label, ListView, NoSelection, Orientation,
    Picture, ScrolledWindow, Separator, SignalListItemFactory, TextView, ToggleButton, Widget,
};

use crate::message_html::MessageHtmlContext;
use crate::models::{SlackFile, SlackMessage};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TimelineMessageObject {
        pub message: RefCell<SlackMessage>,
        /// True for a day-separator row; `message` is then the first message
        /// of the day it introduces (only its `ts` is used).
        pub day_separator: std::cell::Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TimelineMessageObject {
        const NAME: &'static str = "ConduitTimelineMessageObject";
        type Type = super::TimelineMessageObject;
        type ParentType = glib::Object;
    }

    impl ObjectImpl for TimelineMessageObject {}
}

glib::wrapper! {
    pub struct TimelineMessageObject(ObjectSubclass<imp::TimelineMessageObject>);
}

/// Wrapping row for chips and pills: natural-width children, 4px gaps.
pub(crate) fn new_chip_wrap_box() -> adw::WrapBox {
    adw::WrapBox::builder()
        .child_spacing(4)
        .line_spacing(4)
        .build()
}

impl TimelineMessageObject {
    pub fn new(message: SlackMessage) -> Self {
        let obj: Self = glib::Object::builder().build();
        *obj.imp().message.borrow_mut() = message;
        obj
    }

    /// Separator row introducing the day of `first_message_of_day`.
    pub fn day_separator(first_message_of_day: SlackMessage) -> Self {
        let obj = Self::new(first_message_of_day);
        obj.imp().day_separator.set(true);
        obj
    }

    pub fn is_day_separator(&self) -> bool {
        self.imp().day_separator.get()
    }

    /// Fresh object with the same content and kind, to force a re-bind.
    pub fn duplicate(&self) -> Self {
        if self.is_day_separator() {
            Self::day_separator(self.message())
        } else {
            Self::new(self.message())
        }
    }

    pub fn message(&self) -> SlackMessage {
        self.imp().message.borrow().clone()
    }
}

pub(crate) type OpenMediaCallback = Rc<dyn Fn(crate::window::MediaGalleryItem)>;

pub(crate) type ActionHandler = Rc<dyn Fn(TimelineAction)>;
type ActionSlot = Rc<RefCell<Option<ActionHandler>>>;
type ReadMarkTarget = Rc<RefCell<Option<(String, Option<String>)>>>;
type ToggleReactionCallback = Rc<dyn Fn(String, String, bool)>;
type CacheEntry = (Option<gdk_pixbuf::PixbufAnimation>, gtk::gdk::Texture);

const MAX_TEXTURE_CACHE_SIZE: usize = 256;

struct BoundedTextureCache {
    entries: HashMap<PathBuf, CacheEntry>,
    order: std::collections::VecDeque<PathBuf>,
}

impl BoundedTextureCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: std::collections::VecDeque::new(),
        }
    }

    fn get(&mut self, path: &Path) -> Option<CacheEntry> {
        if let Some(entry) = self.entries.get(path) {
            if let Some(pos) = self.order.iter().position(|p| p == path) {
                self.order.remove(pos);
            }
            self.order.push_back(path.to_path_buf());
            return Some(entry.clone());
        }
        None
    }

    fn insert(&mut self, path: PathBuf, value: CacheEntry) {
        if self.entries.contains_key(&path) {
            self.entries.insert(path.clone(), value);
            if let Some(pos) = self.order.iter().position(|p| p == &path) {
                self.order.remove(pos);
            }
            self.order.push_back(path);
        } else {
            if self.entries.len() >= MAX_TEXTURE_CACHE_SIZE {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
            self.entries.insert(path.clone(), value);
            self.order.push_back(path);
        }
    }
}

thread_local! {
    static TEXTURE_CACHE: RefCell<BoundedTextureCache> = RefCell::new(BoundedTextureCache::new());
    // Collapsed image/video attachments, in-memory only (reset on app restart).
    // Message rows are fully rebuilt on every scroll-recycle (no connect_unbind),
    // so this state cannot live on the transient widget - it must survive here,
    // keyed by a stable ts+index pair.
    static COLLAPSED_MEDIA: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Invokes the timeline action handler without holding the `RefCell` borrow:
/// handlers may re-render the view, which replaces the handler via
/// `set_on_action` and would otherwise panic with "already borrowed".
fn dispatch_timeline_action(on_action: &RefCell<Option<ActionHandler>>, action: TimelineAction) {
    let handler = on_action.borrow().clone();
    if let Some(handler) = handler {
        handler(action);
    }
}

fn media_collapse_key(ts: &str, slot: &str) -> String {
    format!("{ts}:{slot}")
}

fn is_media_collapsed(ts: &str, slot: &str) -> bool {
    let key = media_collapse_key(ts, slot);
    COLLAPSED_MEDIA.with(|set| set.borrow().contains(&key))
}

fn set_media_collapsed(ts: &str, slot: &str, collapsed: bool) {
    let key = media_collapse_key(ts, slot);
    COLLAPSED_MEDIA.with(|set| {
        if collapsed {
            set.borrow_mut().insert(key);
        } else {
            set.borrow_mut().remove(&key);
        }
    });
}

/// Wraps a media widget (image/video) with a small ▶/▼ toggle button that
/// shows/hides it, mirroring Slack's own "collapse image previews" affordance.
/// `slot` must be stable and unique per message (e.g. `"file:{index}"` vs
/// `"attachment:{index}"`) so the collapsed state survives row rebuilds and
/// files/attachments at the same index don't collide in the same key space.
pub(crate) fn wrap_collapsible_media(
    media: Widget,
    ts: &str,
    slot: &str,
    title: Option<&str>,
) -> Box {
    let container = Box::new(Orientation::Vertical, 2);
    container.set_halign(gtk::Align::Start);

    let collapsed = is_media_collapsed(ts, slot);

    let icon_for = |expanded: bool| {
        if expanded {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        }
    };
    let toggle = ToggleButton::new();
    toggle.set_focus_on_click(false);
    // Titled media ("GIF ▾") shows dim title text followed by the caret, like
    // Slack; untitled media keeps the compact icon-only circular toggle.
    let caret = Image::from_icon_name(icon_for(!collapsed));
    if let Some(title) = title {
        let title_row = Box::new(Orientation::Horizontal, 2);
        let label = Label::new(Some(title));
        label.set_ellipsize(pango::EllipsizeMode::End);
        title_row.append(&label);
        title_row.append(&caret);
        toggle.set_child(Some(&title_row));
        toggle.add_css_class("timeline-media-title");
        toggle.add_css_class("dim-label");
    } else {
        toggle.set_child(Some(&caret));
        toggle.add_css_class("circular");
    }
    toggle.add_css_class("flat");
    toggle.set_halign(gtk::Align::Start);
    toggle.set_tooltip_text(Some("Show or hide image"));
    toggle.set_active(!collapsed);

    media.set_visible(!collapsed);

    let ts_owned = ts.to_string();
    let slot_owned = slot.to_string();
    let media_weak = media.downgrade();
    toggle.connect_toggled(move |btn| {
        let expanded = btn.is_active();
        caret.set_icon_name(Some(icon_for(expanded)));
        if let Some(media) = media_weak.upgrade() {
            media.set_visible(expanded);
        }
        set_media_collapsed(&ts_owned, &slot_owned, !expanded);
    });

    container.append(&toggle);
    container.append(&media);
    container
}

/// Loads `path` as an animated `Picture` when it's a multi-frame image (GIF/
/// animated WebP), falling back to the shared static-texture cache otherwise.
/// Generalizes the animation driver already used for custom emoji reactions
/// (see `load_custom_emoji_picture`) to arbitrary message/attachment images.
pub(crate) fn load_animated_or_static_picture(
    path: &Path,
    width: i32,
    height: i32,
    content_fit: gtk::ContentFit,
) -> Widget {
    if let Ok(anim) = gdk_pixbuf::PixbufAnimation::from_file(path) {
        if !anim.is_static_image() {
            let iter = anim.iter(None);
            let first_pixbuf = iter.pixbuf();
            let pic = Picture::for_paintable(&gtk::gdk::Texture::for_pixbuf(&first_pixbuf));
            pic.set_size_request(width, height);
            pic.set_content_fit(content_fit);

            let start_animation = move |p: &Picture, anim_obj: &gdk_pixbuf::PixbufAnimation| {
                let iter = anim_obj.iter(None);
                let last_update = Rc::new(Cell::new(std::time::Instant::now()));
                let delay_ms = iter
                    .delay_time()
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(60)
                    .max(60);
                p.add_tick_callback(move |p, _frame_clock| {
                    if !p.is_mapped() {
                        return glib::ControlFlow::Break;
                    }
                    if last_update.get().elapsed().as_millis() as u64 >= delay_ms {
                        last_update.set(std::time::Instant::now());
                        iter.advance(std::time::SystemTime::now());
                        p.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&iter.pixbuf())));
                    }
                    glib::ControlFlow::Continue
                });
            };

            let anim_for_map = anim.clone();
            pic.connect_map(move |p| {
                start_animation(p, &anim_for_map);
            });
            if pic.is_mapped() {
                start_animation(&pic, &anim);
            }

            return pic.upcast::<Widget>();
        }
    }

    let pic = if let Some(tex) = get_or_load_texture(path) {
        Picture::for_paintable(&tex)
    } else {
        Picture::for_filename(path)
    };
    pic.set_size_request(width, height);
    pic.set_content_fit(content_fit);
    pic.upcast::<Widget>()
}

pub(crate) fn register_timeline_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if !gtk::is_initialized() {
            return;
        }
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };
        let provider = CssProvider::new();
        provider.load_from_string(
            r#"
            .blockquote { border-left: 3px solid #888888; padding-left: 8px; margin-left: 4px; }
            .author-menu-button { padding: 2px; margin: -2px; min-width: 0; min-height: 0; }
            .reaction-pill { padding: 2px 6px; min-width: 0; min-height: 0; border-radius: 12px; }
            .reaction-pill label { min-width: 0; }
            .reaction-emoji-unicode { font-size: 32px; line-height: 1; }
            .reaction-emoji-image { margin-top: 3px; margin-bottom: 3px; }
            .timeline-attachment { border-left: 3px solid #e0e0e0; padding-left: 8px; margin-left: 4px; }
            .timeline-media-image { border-radius: 8px; }
            .timeline-media-placeholder {
                border-radius: 8px;
                background-color: alpha(currentColor, 0.08);
            }
            .timeline-media-title { padding: 0 4px; min-height: 0; font-size: smaller; }
            .timeline-video-play-icon {
                background-color: rgba(0, 0, 0, 0.55);
                border-radius: 9999px;
                padding: 12px;
                color: #ffffff;
            }
            .thread-reply-pill {
                background-color: #D6ECFF;
                color: #1264A3;
                border-radius: 12px;
                padding: 2px 10px;
            }
            .thread-reply-active { font-weight: bold; }
            .reaction-pill-active { background-color: #D6ECFF; color: #1264A3; }
            .day-separator-line {
                background-color: color-mix(in srgb, currentColor 15%, transparent);
                min-height: 1px;
            }
            .unread-separator-line { background-color: #1264A3; min-height: 2px; }
            .unread-separator-label { color: #1264A3; font-weight: bold; }
            .timeline-text-view,
            .timeline-text-view:focus,
            .timeline-text-view text {
                background-color: transparent;
                outline: none;
                box-shadow: none;
                padding: 0;
                margin: 0;
            }
            "#,
        );
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

pub(crate) fn get_or_load_texture(path: &Path) -> Option<gtk::gdk::Texture> {
    TEXTURE_CACHE.with(|cache| {
        let mut map = cache.borrow_mut();
        if let Some((_, tex)) = map.get(path) {
            return Some(tex);
        }
        if let Ok(tex) = gtk::gdk::Texture::from_filename(path) {
            map.insert(path.to_path_buf(), (None, tex.clone()));
            return Some(tex);
        }
        None
    })
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
    MarkUnread(String),
    CopyMessageLink(String),
    CopyMessageText(String),
    /// Open (or create) the direct message with this user.
    MessageUser(String),
    /// Show the native profile dialog for this user.
    ShowProfile(String),
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
}

#[derive(Clone, Debug)]
pub(crate) enum HoverEvent {
    Enter {
        row: Widget,
        ts: String,
        reactions: Vec<crate::emoji::EmojiEntry>,
    },
    Leave,
}

#[derive(Clone)]
struct HoveredMessage {
    row: Widget,
    ts: String,
}

#[derive(Clone)]
pub struct NativeTimelineView {
    container: Box,
    scrolled_window: ScrolledWindow,
    list_view: ListView,
    placeholder_label: Label,
    pub store: gio::ListStore,
    context: Rc<RefCell<Option<MessageHtmlContext>>>,
    on_action: ActionSlot,
    asset_update_pending: Rc<Cell<bool>>,
    read_mark_target: ReadMarkTarget,
    latest_message_ts: Rc<RefCell<Option<String>>>,
    read_candidate: Rc<RefCell<Option<String>>>,
    read_generation: Rc<Cell<u64>>,
    recheck_read_visibility: Rc<dyn Fn()>,
}

impl std::fmt::Debug for NativeTimelineView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeTimelineView")
            .field("scrolled_window", &self.scrolled_window)
            .field("store", &self.store)
            .field("context", &self.context)
            .finish()
    }
}

impl NativeTimelineView {
    pub fn new() -> Self {
        let store = gio::ListStore::new::<TimelineMessageObject>();
        let selection = NoSelection::new(Some(store.clone()));

        let factory = SignalListItemFactory::new();
        let context: Rc<RefCell<Option<MessageHtmlContext>>> = Rc::new(RefCell::new(None));
        let on_action: ActionSlot = Rc::new(RefCell::new(None));
        let hovered: Rc<RefCell<Option<HoveredMessage>>> = Rc::new(RefCell::new(None));
        let hover_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        // Holds the quick-bar's current overflow popover (rebuilt fresh on
        // every hover). The hover-leave debounce below queries its live
        // `is_visible()` directly instead of a manually-toggled flag, so it
        // can never go stale relative to whatever GTK's show/closed signals
        // actually do or when they fire.
        let current_popover: Rc<RefCell<Option<gtk::Popover>>> = Rc::new(RefCell::new(None));

        let quick_bar = Box::new(Orientation::Horizontal, 2);
        quick_bar.add_css_class("card");
        quick_bar.set_visible(false);
        quick_bar.set_halign(gtk::Align::Start);
        quick_bar.set_valign(gtk::Align::Start);

        let bar_motion = gtk::EventControllerMotion::new();
        {
            let hover_generation = hover_generation.clone();
            bar_motion.connect_enter(move |_, _, _| {
                hover_generation.set(hover_generation.get().wrapping_add(1));
                crate::debug::log(
                    "quickbar",
                    &format!("bar_motion enter generation={}", hover_generation.get()),
                );
            });
        }
        {
            let hover_generation = hover_generation.clone();
            let hovered = hovered.clone();
            let quick_bar_weak = quick_bar.downgrade();
            let current_popover = current_popover.clone();
            bar_motion.connect_leave(move |_| {
                hover_generation.set(hover_generation.get().wrapping_add(1));
                let expected = hover_generation.get();
                crate::debug::log(
                    "quickbar",
                    &format!("bar_motion leave generation={expected} scheduling hide check in 150ms"),
                );
                let hovered = hovered.clone();
                let quick_bar_weak = quick_bar_weak.clone();
                let hover_generation = hover_generation.clone();
                let current_popover = current_popover.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                    let popover_open = current_popover
                        .borrow()
                        .as_ref()
                        .is_some_and(|p| p.is_visible());
                    let current_generation = hover_generation.get();
                    let stale = current_generation != expected;
                    crate::debug::log(
                        "quickbar",
                        &format!(
                            "bar_motion leave timeout fired expected_gen={expected} current_gen={current_generation} stale={stale} popover_open={popover_open} will_hide={}",
                            !stale && !popover_open
                        ),
                    );
                    if current_generation == expected && !popover_open {
                        *hovered.borrow_mut() = None;
                        if let Some(bar) = quick_bar_weak.upgrade() {
                            bar.set_visible(false);
                        }
                    }
                });
            });
        }
        quick_bar.add_controller(bar_motion.clone());

        let overlay = gtk::Overlay::new();
        overlay.add_overlay(&quick_bar);

        factory.connect_setup(|_factory, list_item| {
            let item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("ListItem expected");
            item.set_child(Some(&Box::new(Orientation::Vertical, 0)));
        });

        let context_clone = context.clone();
        let on_action_clone = on_action.clone();
        let hovered_for_bind = hovered.clone();
        let hover_generation_for_bind = hover_generation.clone();
        let current_popover_for_bind = current_popover.clone();
        let bar_motion_for_bind = bar_motion.clone();
        let quick_bar_for_bind = quick_bar.clone();
        let overlay_for_bind = overlay.clone();
        factory.connect_bind(move |_factory, list_item| {
            let item = list_item
                .downcast_ref::<gtk::ListItem>()
                .expect("ListItem expected");
            let msg_obj = item
                .item()
                .and_then(|obj| obj.downcast::<TimelineMessageObject>().ok())
                .expect("TimelineMessageObject expected");
            let msg = msg_obj.message();

            let is_separator = msg_obj.is_day_separator();
            item.set_selectable(!is_separator);
            item.set_activatable(!is_separator);
            if is_separator {
                item.set_child(Some(&day_separator_widget(&msg.ts)));
                return;
            }

            if let Some(ctx) = context_clone.borrow().as_ref() {
                let on_action = on_action_clone.clone();
                let cb_media: OpenMediaCallback = {
                    let on_action = on_action.clone();
                    Rc::new(move |media_item| {
                        let on_action = on_action.clone();
                        glib::idle_add_local_once(move || {
                            dispatch_timeline_action(
                                &on_action,
                                TimelineAction::OpenMedia(media_item),
                            );
                        });
                    })
                };
                let cb_thread: Rc<dyn Fn(String)> = {
                    let on_action = on_action.clone();
                    Rc::new(move |thread_ts| {
                        let on_action = on_action.clone();
                        glib::idle_add_local_once(move || {
                            dispatch_timeline_action(
                                &on_action,
                                TimelineAction::OpenThread(thread_ts),
                            );
                        });
                    })
                };
                let cb_reaction: Rc<dyn Fn(String, String, bool)> = {
                    let on_action = on_action.clone();
                    Rc::new(move |ts, name, add| {
                        let on_action = on_action.clone();
                        glib::idle_add_local_once(move || {
                            dispatch_timeline_action(
                                &on_action,
                                TimelineAction::ToggleReaction { ts, name, add },
                            );
                        });
                    })
                };
                let cb_author: Rc<dyn Fn(TimelineAction)> = {
                    let on_action = on_action.clone();
                    Rc::new(move |action| {
                        let on_action = on_action.clone();
                        glib::idle_add_local_once(move || {
                            dispatch_timeline_action(&on_action, action);
                        });
                    })
                };
                let cb_hover: Rc<dyn Fn(HoverEvent)> = {
                    let hovered = hovered_for_bind.clone();
                    let hover_generation = hover_generation_for_bind.clone();
                    let current_popover = current_popover_for_bind.clone();
                    let bar_motion = bar_motion_for_bind.clone();
                    let quick_bar = quick_bar_for_bind.clone();
                    let overlay = overlay_for_bind.clone();
                    let context_for_hover = context_clone.clone();
                    let on_action_for_hover = on_action_clone.clone();
                    Rc::new(move |event| match event {
                        HoverEvent::Enter { row, ts, reactions } => {
                            hover_generation.set(hover_generation.get().wrapping_add(1));
                            // Overlay/hit-test boundary jitter right at the bar's own
                            // edge (e.g. over the overflow button) can fire a spurious
                            // re-"enter" for the SAME row that's already showing the
                            // bar. Rebuilding in that case would tear down (not
                            // cleanly close) any popover the user currently has open.
                            // Only rebuild when the hovered message actually changed.
                            let already_showing_this_message = quick_bar.get_visible()
                                && hovered
                                    .borrow()
                                    .as_ref()
                                    .is_some_and(|current| current.ts == ts);
                            crate::debug::log(
                                "quickbar",
                                &format!(
                                    "row HoverEnter ts={ts} generation={} already_showing={already_showing_this_message}",
                                    hover_generation.get()
                                ),
                            );
                            *hovered.borrow_mut() = Some(HoveredMessage { row, ts: ts.clone() });
                            if already_showing_this_message {
                                return;
                            }
                            if let Some(ctx) = context_for_hover.borrow().as_ref() {
                                rebuild_quick_bar(
                                    &quick_bar,
                                    &ts,
                                    &reactions,
                                    ctx,
                                    &QuickBarState {
                                        on_action: &on_action_for_hover,
                                        hover_generation: &hover_generation,
                                        hovered: &hovered,
                                        current_popover: &current_popover,
                                        bar_motion: &bar_motion,
                                    },
                                );
                            }
                            quick_bar.set_visible(true);
                            overlay.queue_allocate();
                        }
                        HoverEvent::Leave => {
                            hover_generation.set(hover_generation.get().wrapping_add(1));
                            let expected = hover_generation.get();
                            crate::debug::log(
                                "quickbar",
                                &format!(
                                    "row HoverLeave generation={expected} scheduling hide check in 150ms"
                                ),
                            );
                            let hovered = hovered.clone();
                            let quick_bar_weak = quick_bar.downgrade();
                            let hover_generation = hover_generation.clone();
                            let current_popover = current_popover.clone();
                            glib::timeout_add_local_once(
                                std::time::Duration::from_millis(150),
                                move || {
                                    let popover_open = current_popover
                                        .borrow()
                                        .as_ref()
                                        .is_some_and(|p| p.is_visible());
                                    let current_generation = hover_generation.get();
                                    let stale = current_generation != expected;
                                    crate::debug::log(
                                        "quickbar",
                                        &format!(
                                            "row HoverLeave timeout fired expected_gen={expected} current_gen={current_generation} stale={stale} popover_open={popover_open} will_hide={}",
                                            !stale && !popover_open
                                        ),
                                    );
                                    if current_generation == expected && !popover_open {
                                        *hovered.borrow_mut() = None;
                                        if let Some(bar) = quick_bar_weak.upgrade() {
                                            bar.set_visible(false);
                                        }
                                    }
                                },
                            );
                        }
                    })
                };
                let msg_widget = build_timeline_message_widget(
                    &msg,
                    ctx,
                    Some(&cb_media),
                    Some(&cb_thread),
                    Some(&cb_reaction),
                    Some(&cb_hover),
                    Some(&cb_author),
                );
                item.set_child(Some(&msg_widget));
            }
        });

        let list_view = ListView::new(Some(selection), Some(factory));

        let scrolled_window = ScrolledWindow::new();
        scrolled_window.set_hexpand(true);
        scrolled_window.set_vexpand(true);
        scrolled_window.set_child(Some(&list_view));

        {
            let hovered = hovered.clone();
            let hover_generation = hover_generation.clone();
            let quick_bar_weak = quick_bar.downgrade();
            scrolled_window
                .vadjustment()
                .connect_value_changed(move |_| {
                    hover_generation.set(hover_generation.get().wrapping_add(1));
                    *hovered.borrow_mut() = None;
                    if let Some(bar) = quick_bar_weak.upgrade() {
                        bar.set_visible(false);
                    }
                });
        }

        // --- Visibility-based automatic read-marking ---
        // Purely event-driven: re-evaluated on scroll, on viewport resize,
        // and once after new content loads - never on a per-frame tick
        // callback, which would keep the compositor/display pipeline from
        // idling for a check that's a no-op nearly all the time.
        let read_mark_target: ReadMarkTarget = Rc::new(RefCell::new(None));
        let latest_message_ts: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let read_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        let read_candidate: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

        let recheck_read_visibility: Rc<dyn Fn()> = {
            let list_view = list_view.clone();
            let scrolled_window = scrolled_window.clone();
            let read_mark_target = read_mark_target.clone();
            let latest_message_ts = latest_message_ts.clone();
            let read_generation = read_generation.clone();
            let read_candidate = read_candidate.clone();
            let on_action = on_action.clone();
            Rc::new(move || {
                let viewport_height = f64::from(scrolled_window.height());
                if viewport_height <= 0.0 {
                    return;
                }
                let mut best: Option<String> = None;
                let mut child = list_view.first_child();
                while let Some(widget) = child {
                    if let Some(bounds) = widget.compute_bounds(&scrolled_window) {
                        let row_height = f64::from(bounds.height());
                        if row_height > 0.0 {
                            let top = f64::from(bounds.y());
                            let bottom = top + row_height;
                            if row_qualifies_for_read(top, bottom, viewport_height) {
                                // `list_view.first_child()` walks GTK's own
                                // internal row wrapper widgets, not the
                                // content widget we tagged via
                                // `set_widget_name` in
                                // `wrap_with_unread_separator_if_needed` -
                                // the tag lives one or more levels further
                                // down the tree, so descend to find it
                                // rather than reading the wrapper's (always
                                // empty) name directly.
                                if let Some(ts) = row_message_ts(&widget) {
                                    best = match best {
                                        Some(current)
                                            if crate::models::slack_timestamp_is_after(
                                                &ts, &current,
                                            ) =>
                                        {
                                            Some(ts)
                                        }
                                        Some(current) => Some(current),
                                        None => Some(ts),
                                    };
                                }
                            }
                        }
                    }
                    child = widget.next_sibling();
                }

                if *read_candidate.borrow() == best {
                    return;
                }
                *read_candidate.borrow_mut() = best.clone();
                // Invalidate any in-flight dwell timer for the previous
                // candidate - a changed or vanished candidate resets the
                // dwell clock rather than pausing it.
                read_generation.set(read_generation.get().wrapping_add(1));
                let expected = read_generation.get();

                let Some(candidate_ts) = best else {
                    return;
                };
                let Some((channel_id, thread_ts)) = read_mark_target.borrow().clone() else {
                    return;
                };
                // Thread read-state is all-or-nothing (`ThreadRecord::mark_read`
                // jumps straight to `latest_reply`) - only fire for a thread
                // surface once the dwelling candidate IS the actual latest
                // message, never for partial progress through older replies.
                if thread_ts.is_some()
                    && latest_message_ts.borrow().as_deref() != Some(candidate_ts.as_str())
                {
                    return;
                }
                let read_generation = read_generation.clone();
                let on_action = on_action.clone();
                glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
                    if read_generation.get() != expected {
                        return;
                    }
                    dispatch_timeline_action(
                        &on_action,
                        TimelineAction::AutoMarkRead {
                            channel_id,
                            thread_ts,
                            ts: candidate_ts,
                        },
                    );
                });
            })
        };

        let scroll_check_pending = Rc::new(Cell::new(false));
        {
            let recheck = recheck_read_visibility.clone();
            let pending = scroll_check_pending.clone();
            scrolled_window
                .vadjustment()
                .connect_value_changed(move |_| {
                    if !pending.get() {
                        pending.set(true);
                        let recheck = recheck.clone();
                        let pending = pending.clone();
                        glib::timeout_add_local_once(
                            std::time::Duration::from_millis(50),
                            move || {
                                pending.set(false);
                                recheck();
                            },
                        );
                    }
                });
        }
        {
            let recheck = recheck_read_visibility.clone();
            scrolled_window
                .vadjustment()
                .connect_notify_local(Some("page-size"), move |_, _| {
                    recheck();
                });
        }
        {
            let recheck = recheck_read_visibility.clone();
            scrolled_window
                .vadjustment()
                .connect_notify_local(Some("upper"), move |_, _| {
                    recheck();
                });
        }

        overlay.set_child(Some(&scrolled_window));

        {
            let hovered = hovered.clone();
            let quick_bar_for_position = quick_bar.clone().upcast::<Widget>();
            overlay.connect_get_child_position(move |ov, widget| {
                if *widget != quick_bar_for_position {
                    return None;
                }
                let hovered_ref = hovered.borrow();
                let hovered = hovered_ref.as_ref()?;
                let bounds = hovered.row.compute_bounds(ov)?;
                let (_, bar_w, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                let (_, bar_h, _, _) = widget.measure(gtk::Orientation::Vertical, -1);
                let overlay_width = ov.width();
                let max_x = (overlay_width - bar_w).max(0) as f32;
                let x = (bounds.x() + bounds.width() - bar_w as f32 - 8.0)
                    .max(4.0)
                    .min(max_x.max(4.0));
                let y = (bounds.y() + 4.0).max(0.0);
                Some(gtk::gdk::Rectangle::new(
                    x.round() as i32,
                    y.round() as i32,
                    bar_w,
                    bar_h,
                ))
            });
        }

        let placeholder_label = Label::new(None);
        placeholder_label.set_hexpand(true);
        placeholder_label.set_vexpand(true);
        placeholder_label.set_valign(gtk::Align::Center);
        placeholder_label.set_halign(gtk::Align::Center);
        placeholder_label.set_justify(gtk::Justification::Center);
        placeholder_label.set_wrap(true);
        placeholder_label.add_css_class("dim-label");
        placeholder_label.set_visible(false);

        let container = Box::new(Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_vexpand(true);
        container.append(&overlay);
        container.append(&placeholder_label);

        Self {
            container,
            scrolled_window,
            list_view,
            placeholder_label,
            store,
            context,
            on_action,
            asset_update_pending: Rc::new(Cell::new(false)),
            read_mark_target,
            latest_message_ts,
            read_candidate,
            read_generation,
            recheck_read_visibility,
        }
    }

    /// Sets which conversation/thread this view's visibility-based
    /// auto-read-marking should advance. `thread_ts` is `None` for the main
    /// timeline surface (advances the conversation's `last_read` via
    /// `conversations.mark`) and `Some` for a thread pane surface (advances
    /// the thread's local-only read state instead).
    pub(crate) fn set_read_mark_target(&self, channel_id: &str, thread_ts: Option<&str>) {
        *self.read_mark_target.borrow_mut() =
            Some((channel_id.to_string(), thread_ts.map(ToString::to_string)));
        *self.read_candidate.borrow_mut() = None;
        self.read_generation
            .set(self.read_generation.get().wrapping_add(1));
        (self.recheck_read_visibility)();
    }

    pub(crate) fn set_on_action<F: Fn(TimelineAction) + 'static>(&self, f: F) {
        *self.on_action.borrow_mut() = Some(Rc::new(f));
    }

    pub fn update_image_asset(&self, context: &MessageHtmlContext) {
        *self.context.borrow_mut() = Some(context.clone());
        if self.asset_update_pending.get() {
            return;
        }
        self.asset_update_pending.set(true);
        let this = self.clone();
        glib::idle_add_local_once(move || {
            this.asset_update_pending.set(false);
            let n_items = this.store.n_items();
            let mut new_items = Vec::with_capacity(n_items as usize);
            for i in 0..n_items {
                if let Some(obj) = this
                    .store
                    .item(i)
                    .and_then(|o| o.downcast::<TimelineMessageObject>().ok())
                {
                    new_items.push(obj.duplicate());
                }
            }
            if !new_items.is_empty() {
                this.store.splice(0, n_items, &new_items);
            }
        });
    }

    /// Moves the "New" unread separator to `anchor_ts`, rebuilding only the
    /// rows that gain or lose it so the list keeps its scroll position.
    pub fn set_unread_separator(&self, anchor_ts: Option<String>) {
        let previous = {
            let mut context = self.context.borrow_mut();
            let Some(context) = context.as_mut() else {
                return;
            };
            if context.unread_separator_ts == anchor_ts {
                return;
            }
            std::mem::replace(&mut context.unread_separator_ts, anchor_ts.clone())
        };
        for i in 0..self.store.n_items() {
            let Some(obj) = self
                .store
                .item(i)
                .and_then(|o| o.downcast::<TimelineMessageObject>().ok())
            else {
                continue;
            };
            if obj.is_day_separator() {
                continue;
            }
            let ts = obj.message().ts;
            if previous.as_deref() == Some(ts.as_str()) || anchor_ts.as_deref() == Some(ts.as_str())
            {
                self.store.splice(i, 1, &[obj.duplicate()]);
            }
        }
    }

    pub fn set_messages(
        &self,
        messages: &[SlackMessage],
        context: &MessageHtmlContext,
        focus_ts: Option<&str>,
    ) {
        self.placeholder_label.set_visible(false);
        self.scrolled_window.set_visible(true);
        *self.context.borrow_mut() = Some(context.clone());
        *self.latest_message_ts.borrow_mut() = messages.first().map(|msg| msg.ts.clone());
        *self.read_candidate.borrow_mut() = None;
        self.read_generation
            .set(self.read_generation.get().wrapping_add(1));
        self.store.remove_all();
        let (items, focus_index) = build_store_items(messages, focus_ts);
        self.store.splice(0, 0, &items);

        let list_view = self.list_view.clone();
        let n_items = items.len() as u32;
        let vadj = self.scrolled_window.vadjustment();
        let recheck_read_visibility = self.recheck_read_visibility.clone();
        glib::idle_add_local_once(move || {
            if let Some(index) = focus_index {
                list_view.scroll_to(index, gtk::ListScrollFlags::empty(), None);
            } else if n_items > 0 {
                list_view.scroll_to(n_items - 1, gtk::ListScrollFlags::empty(), None);
            } else {
                vadj.set_value(vadj.upper() - vadj.page_size());
            }
            recheck_read_visibility();
        });
        let recheck_delayed = self.recheck_read_visibility.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
            recheck_delayed();
        });
    }

    /// Shows a plain-text placeholder (loading state, empty state, error message) in place of
    /// the message list, for surfaces that have no structured content to render yet.
    pub fn show_placeholder(&self, text: &str) {
        self.store.remove_all();
        self.placeholder_label.set_label(text);
        self.placeholder_label.set_visible(true);
        self.scrolled_window.set_visible(false);
    }

    pub fn widget(&self) -> &Widget {
        self.container.upcast_ref()
    }
}

const EXTENSIONS: &[&str] = &[
    "", ".png", ".gif", ".jpg", ".jpeg", ".webp", ".mp4", ".webm", ".mov", ".ogg", ".avif",
];

fn check_file_extensions(base_dir: &Path, key: &str) -> Option<PathBuf> {
    for ext in EXTENSIONS {
        let p = base_dir.join(format!("{key}{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub(crate) fn resolve_cached_asset_path(
    url: &str,
    context: &MessageHtmlContext,
) -> Option<PathBuf> {
    let source_uri = context
        .image_assets
        .get(url)
        .map(|s| s.uri())
        .unwrap_or(url);

    let cache_key = source_uri
        .strip_prefix("conduit-asset://")
        .or_else(|| source_uri.strip_prefix("conduit-cache://"));

    let cache_dir = crate::config::image_asset_cache_dir();

    if let Some(cache_key) = cache_key {
        let clean_key = cache_key
            .split('?')
            .next()
            .unwrap_or(cache_key)
            .trim_matches('/');
        if !clean_key.is_empty() {
            if let Some(path) = check_file_extensions(&cache_dir, clean_key) {
                return Some(path);
            }

            if let Ok(entries) = std::fs::read_dir(&cache_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        if let Some(sub_path) = check_file_extensions(&path, clean_key) {
                            return Some(sub_path);
                        }
                    }
                }
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(workspace_dir_name) = entry.file_name().to_str() {
                    let hash = {
                        use sha2::{Digest, Sha256};
                        let mut hasher = Sha256::new();
                        hasher.update(workspace_dir_name.as_bytes());
                        hasher.update([0]);
                        hasher.update(url.as_bytes());
                        format!("{:x}", hasher.finalize())
                    };

                    if let Some(sub_path) = check_file_extensions(&path, &hash) {
                        return Some(sub_path);
                    }
                }
            }
        }
    }

    None
}

impl Default for NativeTimelineView {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InlineCustomEmoji {
    name: String,
    url: String,
}

fn extract_custom_emojis_and_prepare_markup(pango: &str) -> (String, Vec<InlineCustomEmoji>) {
    let mut result = String::with_capacity(pango.len());
    let mut emojis = Vec::new();
    let mut rest = pango;

    while let Some(start) = rest.find("<conduit-custom-emoji ") {
        result.push_str(&rest[..start]);
        let tag_rest = &rest[start..];
        if let Some(end) = tag_rest.find("/>") {
            let tag_content = &tag_rest[..end + 2];
            let name = extract_attr_value(tag_content, "name").unwrap_or_default();
            let url = extract_attr_value(tag_content, "url").unwrap_or_default();
            emojis.push(InlineCustomEmoji { name, url });
            result.push('\u{FFFC}');
            rest = &tag_rest[end + 2..];
        } else {
            result.push_str(&rest[..start + 22]);
            rest = &rest[start + 22..];
        }
    }
    result.push_str(rest);
    (result, emojis)
}

fn unescape_xml_attribute(val: &str) -> String {
    val.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn extract_attr_value(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(unescape_xml_attribute(&tag[start..end]))
}

fn create_inline_emoji_widget(emoji: &InlineCustomEmoji, context: &MessageHtmlContext) -> Widget {
    let local_path = resolve_cached_asset_path(&emoji.url, context).or_else(|| {
        Path::new(&emoji.url)
            .exists()
            .then(|| PathBuf::from(&emoji.url))
    });

    let widget = if let Some(path) = local_path {
        load_animated_or_static_picture(&path, 18, 18, gtk::ContentFit::Contain)
    } else {
        let label = Label::new(Some(&format!(":{}:", emoji.name)));
        label.set_selectable(false);
        label.upcast::<Widget>()
    };
    widget.set_valign(gtk::Align::Center);
    widget.set_tooltip_text(Some(&format!(":{}:", emoji.name)));
    widget
}

/// A link extracted from Pango markup, as a char range of the visible text.
#[derive(Debug, PartialEq)]
struct MarkupLink {
    start: i32,
    end: i32,
    href: String,
}

/// Removes `<a href>` tags, which are only valid in GtkLabel markup, and returns the
/// links as char ranges into the text that results from parsing the stripped markup.
fn strip_anchor_tags(markup: &str) -> (String, Vec<MarkupLink>) {
    let mut out = String::with_capacity(markup.len());
    let mut links = Vec::new();
    let mut offset = 0i32;
    let mut open: Option<(i32, String)> = None;
    let mut rest = markup;

    while let Some(pos) = rest.find(['<', '&']) {
        let (text, tail) = rest.split_at(pos);
        out.push_str(text);
        offset += text.chars().count() as i32;
        if tail.starts_with('&') {
            let len = tail.find(';').map_or(1, |i| i + 1);
            out.push_str(&tail[..len]);
            offset += 1;
            rest = &tail[len..];
            continue;
        }
        let Some(close) = tail.find('>') else {
            out.push_str(tail);
            rest = "";
            break;
        };
        let tag = &tail[..=close];
        if tag.starts_with("<a ") || tag == "<a>" {
            let href = extract_attr_value(tag, "href").unwrap_or_default();
            open = Some((offset, href));
        } else if tag == "</a>" {
            if let Some((start, href)) = open.take() {
                if !href.is_empty() && offset > start {
                    links.push(MarkupLink {
                        start,
                        end: offset,
                        href,
                    });
                }
            }
        } else {
            out.push_str(tag);
        }
        rest = &tail[close + 1..];
    }
    out.push_str(rest);
    (out, links)
}

fn attach_label_links(
    label: &Label,
    context: &MessageHtmlContext,
    on_action: Option<&ActionHandler>,
) {
    let context = context.clone();
    let on_action = on_action.cloned();
    label.connect_activate_link(move |label, uri| {
        if let Some(user_id) = uri.strip_prefix("conduit-user://") {
            if let Some(on_action) = on_action.as_ref() {
                crate::author_menu::show_user_mention_popover(
                    label,
                    user_id,
                    &context,
                    on_action.clone(),
                    None,
                );
            }
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
}

/// Underlines link ranges in a TextView buffer and opens them on click.
fn attach_text_view_links(
    view: &TextView,
    links: Vec<MarkupLink>,
    context: &MessageHtmlContext,
    on_action: Option<&ActionHandler>,
) {
    if links.is_empty() {
        return;
    }
    let buffer = view.buffer();
    let mut hrefs = Vec::with_capacity(links.len());
    for (index, link) in links.into_iter().enumerate() {
        let name = format!("conduit-link-{index}");
        let tag = gtk::TextTag::builder()
            .name(name.as_str())
            .underline(pango::Underline::Single)
            .build();
        buffer.tag_table().add(&tag);
        buffer.apply_tag(
            &tag,
            &buffer.iter_at_offset(link.start),
            &buffer.iter_at_offset(link.end),
        );
        hrefs.push((name, link.href));
    }
    let click = gtk::GestureClick::new();
    let click_view = view.clone();
    let context = context.clone();
    let on_action = on_action.cloned();
    click.connect_released(move |_, _, x, y| {
        let (bx, by) =
            click_view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
        let Some(iter) = click_view.iter_at_location(bx, by) else {
            return;
        };
        for tag in iter.tags() {
            let Some(tag_name) = tag.name() else { continue };
            if let Some((_, href)) = hrefs.iter().find(|(n, _)| n.as_str() == tag_name.as_str()) {
                if let Some(user_id) = href.strip_prefix("conduit-user://") {
                    if let Some(on_action) = on_action.as_ref() {
                        let rect = gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
                        crate::author_menu::show_user_mention_popover(
                            &click_view,
                            user_id,
                            &context,
                            on_action.clone(),
                            Some(rect),
                        );
                    }
                } else {
                    let _ = gtk::gio::AppInfo::launch_default_for_uri(
                        href,
                        None::<&gtk::gio::AppLaunchContext>,
                    );
                }
                break;
            }
        }
    });
    view.add_controller(click);
}

pub(crate) fn create_message_text_widget(
    pango: &str,
    context: &MessageHtmlContext,
    on_action: Option<&ActionHandler>,
) -> Widget {
    if !pango.contains("<conduit-custom-emoji ") {
        let label = Label::new(None);
        label.set_wrap(true);
        label.set_wrap_mode(pango::WrapMode::WordChar);
        label.set_selectable(true);
        label.set_focus_on_click(false);
        label.set_xalign(0.0);
        // libadwaita's document font (family, size, line height) for reading content.
        label.add_css_class("document");
        label.set_markup(pango);
        attach_label_links(&label, context, on_action);
        return label.upcast::<Widget>();
    }

    let (markup_clean, emojis) = extract_custom_emojis_and_prepare_markup(pango);
    let (markup_clean, links) = strip_anchor_tags(&markup_clean);
    let view = TextView::new();
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.add_css_class("timeline-text-view");
    view.add_css_class("document");
    register_timeline_css();

    let buffer = view.buffer();
    let mut iter = buffer.start_iter();
    buffer.insert_markup(&mut iter, &markup_clean);
    attach_text_view_links(&view, links, context, on_action);

    let mut search_iter = buffer.start_iter();
    for emoji in &emojis {
        if let Some((start_match, end_match)) =
            search_iter.forward_search("\u{FFFC}", gtk::TextSearchFlags::empty(), None)
        {
            let mut del_start = start_match;
            let mut del_end = end_match;
            buffer.delete(&mut del_start, &mut del_end);
            let anchor = buffer.create_child_anchor(&mut del_start);
            let emoji_widget = create_inline_emoji_widget(emoji, context);
            view.add_child_at_anchor(&emoji_widget, &anchor);
            search_iter = del_start;
        } else {
            break;
        }
    }

    view.upcast::<Widget>()
}

#[derive(Debug, PartialEq)]
enum TextSegment {
    CodeBlock { lang: Option<String>, code: String },
    Quote(String),
    Normal(String),
}

fn parse_text_segments(input: &str) -> Vec<TextSegment> {
    let mut segments = Vec::new();
    let mut remaining = input;

    while !remaining.is_empty() {
        if let Some(fence_start) = remaining.find("```") {
            let before = &remaining[..fence_start];
            if !before.is_empty() {
                parse_quotes_and_normal(before, &mut segments);
            }
            let after_fence = &remaining[fence_start + 3..];
            if let Some(fence_end) = after_fence.find("```") {
                let code_content = &after_fence[..fence_end];
                let (lang, code) = if let Some((first_line, rest)) = code_content.split_once('\n') {
                    let first_line_trimmed = first_line.trim();
                    if !first_line_trimmed.is_empty() && !first_line_trimmed.contains(' ') {
                        (Some(first_line_trimmed.to_string()), rest)
                    } else {
                        (None, code_content)
                    }
                } else {
                    (None, code_content)
                };
                segments.push(TextSegment::CodeBlock {
                    lang,
                    code: code.trim_matches('\n').to_string(),
                });
                remaining = &after_fence[fence_end + 3..];
            } else {
                segments.push(TextSegment::CodeBlock {
                    lang: None,
                    code: after_fence.trim_matches('\n').to_string(),
                });
                break;
            }
        } else {
            parse_quotes_and_normal(remaining, &mut segments);
            break;
        }
    }

    segments
}

fn parse_quotes_and_normal(text: &str, segments: &mut Vec<TextSegment>) {
    if let Some(triple_idx) = text.find(">>>") {
        let before = &text[..triple_idx];
        if !before.is_empty() {
            parse_line_by_line_quotes(before, segments);
        }
        let quote_content = &text[triple_idx + 3..];
        let cleaned_quote = quote_content.trim_start_matches('\n').to_string();
        if !cleaned_quote.is_empty() {
            segments.push(TextSegment::Quote(cleaned_quote));
        }
        return;
    }

    parse_line_by_line_quotes(text, segments);
}

fn parse_line_by_line_quotes(text: &str, segments: &mut Vec<TextSegment>) {
    let mut normal_acc = String::new();
    let mut quote_acc = String::new();

    for line in text.lines() {
        if let Some(stripped) = line.strip_prefix("> ") {
            if !normal_acc.is_empty() {
                segments.push(TextSegment::Normal(normal_acc.clone()));
                normal_acc.clear();
            }
            if !quote_acc.is_empty() {
                quote_acc.push('\n');
            }
            quote_acc.push_str(stripped);
        } else if line == ">" {
            if !normal_acc.is_empty() {
                segments.push(TextSegment::Normal(normal_acc.clone()));
                normal_acc.clear();
            }
            if !quote_acc.is_empty() {
                quote_acc.push('\n');
            }
        } else {
            if !quote_acc.is_empty() {
                segments.push(TextSegment::Quote(quote_acc.clone()));
                quote_acc.clear();
            }
            if !normal_acc.is_empty() {
                normal_acc.push('\n');
            }
            normal_acc.push_str(line);
        }
    }

    if !quote_acc.is_empty() {
        segments.push(TextSegment::Quote(quote_acc));
    }
    if !normal_acc.is_empty() {
        segments.push(TextSegment::Normal(normal_acc));
    }
}

pub(crate) fn render_text_content(
    text: &str,
    target_box: &Box,
    context: &MessageHtmlContext,
    on_action: Option<&ActionHandler>,
) {
    let segments = parse_text_segments(text);
    for seg in segments {
        match seg {
            TextSegment::CodeBlock { lang: _, code } => {
                let frame = Box::new(Orientation::Vertical, 0);
                frame.add_css_class("code-block");
                frame.add_css_class("monospace");
                let label = Label::new(None);
                label.set_wrap(true);
                label.set_wrap_mode(pango::WrapMode::WordChar);
                label.set_selectable(true);
                label.set_focus_on_click(false);
                label.set_xalign(0.0);
                label.set_text(&code);
                frame.append(&label);
                target_box.append(&frame);
            }
            TextSegment::Quote(quote_text) => {
                register_timeline_css();
                let quote_box = Box::new(Orientation::Vertical, 0);
                quote_box.add_css_class("blockquote");
                let pango = crate::message_html::mrkdwn_to_pango(&quote_text, context);
                let text_widget =
                    create_message_text_widget(&format!("<i>{}</i>", pango), context, on_action);
                quote_box.append(&text_widget);
                target_box.append(&quote_box);
            }
            TextSegment::Normal(normal_text) => {
                if !normal_text.trim().is_empty() {
                    let pango = crate::message_html::mrkdwn_to_pango(&normal_text, context);
                    let text_widget = create_message_text_widget(&pango, context, on_action);
                    target_box.append(&text_widget);
                }
            }
        }
    }
}

fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn render_files(
    files: &[SlackFile],
    root_box: &Box,
    context: &MessageHtmlContext,
    on_open_media: Option<&OpenMediaCallback>,
    ts: &str,
) {
    let files_box = Box::new(Orientation::Vertical, 6);
    files_box.set_halign(gtk::Align::Start);

    for (index, file) in files.iter().enumerate() {
        let is_video = file
            .mimetype
            .as_deref()
            .is_some_and(|m| m.starts_with("video/"))
            || file.thumb_video.is_some()
            || file.supported_media_kind() == Some("video");

        let is_image = file
            .mimetype
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
            || file.thumb_360.is_some()
            || file.thumb_480.is_some()
            || file.thumb_720.is_some()
            || file.thumb_1024.is_some()
            || file.url_static_preview.is_some()
            || file.preview_url().is_some();

        let title = file.display_title().to_string();

        if is_video {
            let video_candidates: Vec<&str> = [
                file.thumb_video.as_deref(),
                file.thumb_1024.as_deref(),
                file.thumb_720.as_deref(),
                file.thumb_480.as_deref(),
                file.thumb_360.as_deref(),
                file.url_static_preview.as_deref(),
                file.preview_url(),
                file.url_private.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect();

            let local_thumb = video_candidates.iter().find_map(|&url| {
                resolve_cached_asset_path(url, context).or_else(|| {
                    if std::path::Path::new(url).exists() {
                        Some(std::path::PathBuf::from(url))
                    } else {
                        None
                    }
                })
            });

            let video_url = file
                .media_url()
                .or(file.url_private_download.as_deref())
                .or(file.url_private.as_deref())
                .unwrap_or("");

            let container = Box::new(Orientation::Vertical, 4);
            container.add_css_class("timeline-video-container");
            container.set_halign(gtk::Align::Start);

            let overlay = gtk::Overlay::new();

            if let Some(path) = local_thumb {
                let pic = if let Some(tex) = get_or_load_texture(&path) {
                    Picture::for_paintable(&tex)
                } else {
                    Picture::for_filename(&path)
                };
                pic.set_content_fit(gtk::ContentFit::ScaleDown);
                pic.set_size_request(600, 340);
                pic.set_tooltip_text(Some(&title));
                overlay.set_child(Some(&pic));
            } else {
                let poster_box = Box::new(Orientation::Vertical, 0);
                poster_box.set_size_request(360, 200);
                poster_box.add_css_class("card");
                poster_box.add_css_class("rounded");
                poster_box.set_tooltip_text(Some(&title));
                overlay.set_child(Some(&poster_box));
            }

            let play_icon = Image::from_icon_name("media-playback-start-symbolic");
            play_icon.set_pixel_size(24);
            play_icon.add_css_class("timeline-video-play-icon");
            play_icon.set_valign(gtk::Align::Center);
            play_icon.set_halign(gtk::Align::Center);
            play_icon.set_can_target(false);
            overlay.add_overlay(&play_icon);

            container.append(&overlay);

            if !video_url.is_empty() {
                let gesture = gtk::GestureClick::new();
                let cb = on_open_media.cloned();
                let url = video_url.to_string();
                let name = title.clone();
                gesture.connect_pressed(move |_, _, _, _| {
                    if let Some(cb) = &cb {
                        cb(crate::window::MediaGalleryItem {
                            url: url.clone(),
                            name: name.clone(),
                            kind: crate::window::MediaKind::Video,
                        });
                    }
                });
                container.add_controller(gesture);
                container.set_cursor_from_name(Some("pointer"));
            }

            files_box.append(&wrap_collapsible_media(
                container.upcast::<Widget>(),
                ts,
                &format!("file:{index}"),
                None,
            ));
        } else if is_image {
            let container = Box::new(Orientation::Vertical, 2);
            container.add_css_class("timeline-image-container");
            container.set_halign(gtk::Align::Start);

            let candidates: Vec<&str> = [
                file.preview_url(),
                file.url_static_preview.as_deref(),
                file.thumb_1024.as_deref(),
                file.thumb_720.as_deref(),
                file.thumb_480.as_deref(),
                file.thumb_360.as_deref(),
                file.thumb_160.as_deref(),
                file.url_private.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect();

            let local_path = candidates.iter().find_map(|&url| {
                resolve_cached_asset_path(url, context).or_else(|| {
                    if std::path::Path::new(url).exists() {
                        Some(std::path::PathBuf::from(url))
                    } else {
                        None
                    }
                })
            });

            let img_widget: Widget = if let Some(path) = local_path {
                let pic =
                    load_animated_or_static_picture(&path, 400, 300, gtk::ContentFit::ScaleDown);
                pic.add_css_class("rounded");
                pic
            } else {
                let icon = Image::from_icon_name("image-x-generic-symbolic");
                icon.set_pixel_size(48);
                icon.upcast::<Widget>()
            };

            img_widget.set_tooltip_text(Some(&title));
            container.append(&img_widget);

            if let Some(media_url) = file.media_url() {
                let gesture = gtk::GestureClick::new();
                let cb = on_open_media.cloned();
                let url = media_url.to_string();
                let name = title.clone();
                gesture.connect_pressed(move |_, _, _, _| {
                    if let Some(cb) = &cb {
                        cb(crate::window::MediaGalleryItem {
                            url: url.clone(),
                            name: name.clone(),
                            kind: crate::window::MediaKind::Image,
                        });
                    }
                });
                container.add_controller(gesture);
                container.set_cursor_from_name(Some("pointer"));
            }

            files_box.append(&wrap_collapsible_media(
                container.upcast::<Widget>(),
                ts,
                &format!("file:{index}"),
                None,
            ));
        } else {
            let file_card = Box::new(Orientation::Horizontal, 8);
            file_card.add_css_class("file-card");
            file_card.set_halign(gtk::Align::Start);

            let mime = file.mimetype.as_deref().unwrap_or("");
            let icon_name = if mime.contains("zip")
                || mime.contains("tar")
                || mime.contains("archive")
                || mime.contains("compressed")
            {
                "package-x-generic-symbolic"
            } else {
                "text-x-generic-symbolic"
            };

            let icon = Image::from_icon_name(icon_name);
            icon.set_pixel_size(24);
            file_card.append(&icon);

            let info_box = Box::new(Orientation::Vertical, 2);
            info_box.set_hexpand(true);

            let title_label = Label::new(None);
            title_label.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(&title)));
            title_label.set_xalign(0.0);
            title_label.set_ellipsize(pango::EllipsizeMode::End);
            info_box.append(&title_label);

            if let Some(bytes) = file.size {
                let size_str = format_file_size(bytes);
                let size_label = Label::new(Some(&size_str));
                size_label.add_css_class("dim-label");
                size_label.set_xalign(0.0);
                info_box.append(&size_label);
            }

            file_card.append(&info_box);

            let download_btn = Button::from_icon_name("document-save-symbolic");
            download_btn.set_focus_on_click(false);
            download_btn.add_css_class("flat");
            download_btn.set_tooltip_text(Some("Download file"));
            file_card.append(&download_btn);

            files_box.append(&file_card);
        }
    }

    root_box.append(&files_box);
}

fn build_avatar_widget(message: &SlackMessage, context: &MessageHtmlContext) -> Widget {
    let user_avatar_url = context
        .display_user_id(message)
        .and_then(|user_id| context.user_avatar_urls.get(user_id));
    let avatar_source_url = user_avatar_url
        .map(String::as_str)
        .or_else(|| message.avatar_url());

    if let Some(url) = avatar_source_url {
        let local_path = resolve_cached_asset_path(url, context)
            .or_else(|| Path::new(url).exists().then(|| PathBuf::from(url)));
        if let Some(path) = local_path {
            let picture = if let Some(tex) = get_or_load_texture(&path) {
                Picture::for_paintable(&tex)
            } else {
                Picture::for_filename(&path)
            };
            picture.set_size_request(36, 36);
            picture.set_content_fit(gtk::ContentFit::Cover);
            return picture.upcast::<Widget>();
        }
    }

    let fallback = Image::from_icon_name("avatar-default-symbolic");
    fallback.set_pixel_size(36);
    fallback.upcast::<Widget>()
}

fn load_custom_emoji_picture(path: &Path) -> Widget {
    let cached = TEXTURE_CACHE.with(|cache| {
        let mut map = cache.borrow_mut();
        if let Some(entry) = map.get(path) {
            return Some(entry.clone());
        }
        if let Ok(anim) = gdk_pixbuf::PixbufAnimation::from_file(path) {
            if anim.is_static_image() {
                if let Some(pixbuf) = anim.static_image() {
                    let scaled = pixbuf
                        .scale_simple(32, 32, gdk_pixbuf::InterpType::Bilinear)
                        .unwrap_or(pixbuf);
                    let texture = gtk::gdk::Texture::for_pixbuf(&scaled);
                    let entry = (None, texture);
                    map.insert(path.to_path_buf(), entry.clone());
                    return Some(entry);
                }
            } else {
                let iter = anim.iter(None);
                let initial_pixbuf = iter.pixbuf();
                let scaled = initial_pixbuf
                    .scale_simple(32, 32, gdk_pixbuf::InterpType::Bilinear)
                    .unwrap_or(initial_pixbuf);
                let texture = gtk::gdk::Texture::for_pixbuf(&scaled);
                let entry = (Some(anim), texture);
                map.insert(path.to_path_buf(), entry.clone());
                return Some(entry);
            }
        } else if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_file(path) {
            let scaled = pixbuf
                .scale_simple(32, 32, gdk_pixbuf::InterpType::Bilinear)
                .unwrap_or(pixbuf);
            let texture = gtk::gdk::Texture::for_pixbuf(&scaled);
            let entry = (None, texture);
            map.insert(path.to_path_buf(), entry.clone());
            return Some(entry);
        }
        None
    });

    if let Some((opt_anim, texture)) = cached {
        let pic = Picture::for_paintable(&texture);
        pic.set_size_request(32, 32);
        pic.set_can_shrink(true);
        pic.set_content_fit(gtk::ContentFit::Cover);
        pic.add_css_class("reaction-emoji-image");

        if let Some(anim) = opt_anim {
            let start_animation = |p: &Picture, anim_obj: &gdk_pixbuf::PixbufAnimation| {
                let iter = anim_obj.iter(None);
                let last_update = Rc::new(Cell::new(std::time::Instant::now()));
                let delay_ms = iter
                    .delay_time()
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(60)
                    .max(60);

                p.add_tick_callback(move |p, _frame_clock| {
                    if !p.is_mapped() {
                        return glib::ControlFlow::Break;
                    }
                    if last_update.get().elapsed().as_millis() as u64 >= delay_ms {
                        last_update.set(std::time::Instant::now());
                        iter.advance(std::time::SystemTime::now());
                        let pixbuf = iter.pixbuf();
                        let scaled = pixbuf
                            .scale_simple(32, 32, gdk_pixbuf::InterpType::Bilinear)
                            .unwrap_or(pixbuf);
                        p.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&scaled)));
                    }
                    glib::ControlFlow::Continue
                });
            };

            let anim_for_map = anim.clone();
            pic.connect_map(move |p| {
                start_animation(p, &anim_for_map);
            });

            if pic.is_mapped() {
                start_animation(&pic, &anim);
            }
        }

        return pic.upcast::<Widget>();
    }

    let pic = Picture::for_filename(path);
    pic.set_size_request(32, 32);
    pic.set_can_shrink(true);
    pic.set_content_fit(gtk::ContentFit::Cover);
    pic.add_css_class("reaction-emoji-image");
    pic.upcast::<Widget>()
}

const ADD_REACTION_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" width="32" height="32" fill="currentColor"><path fill="currentColor" d="M10.28 4.117a7 7 0 1 0 5.604 5.604A2.5 2.5 0 0 1 13 7.25v-.251h-.251a2.5 2.5 0 0 1-2.47-2.883Z"/><path fill="currentColor" fill-rule="evenodd" d="M15.5 1a.75.75 0 0 1 .75.75v2h2a.75.75 0 0 1 0 1.5h-2v2a.75.75 0 0 1-1.5 0v-2h-2a.75.75 0 0 1 0-1.5h2v-2A.75.75 0 0 1 15.5 1Zm-13 10a6.5 6.5 0 0 1 7.166-6.466.75.75 0 0 0 .152-1.493 8 8 0 1 0 7.14 7.139.75.75 0 0 0-1.492.152A6.525 6.525 0 0 1 15.5 11a6.5 6.5 0 1 1-13 0Zm4.25-.5a1.25 1.25 0 1 0 0-2.5 1.25 1.25 0 0 0 0 2.5Zm4.5 0a1.25 1.25 0 1 0 0-2.5 1.25 1.25 0 0 0 0 2.5ZM9 15c1.277 0 2.553-.724 3.06-2.173.148-.426-.209-.827-.66-.827H6.6c-.452 0-.808.4-.66.827C6.448 14.276 7.724 15 9 15Z" clip-rule="evenodd"/></svg>"#;

fn load_add_reaction_widget() -> Widget {
    let pic = Picture::new();
    pic.set_size_request(32, 32);
    pic.set_can_shrink(true);
    pic.set_content_fit(gtk::ContentFit::Cover);
    pic.add_css_class("reaction-emoji-image");

    if let Ok(loader) = gdk_pixbuf::PixbufLoader::with_type("svg") {
        if loader.write(ADD_REACTION_SVG.as_bytes()).is_ok() && loader.close().is_ok() {
            if let Some(pixbuf) = loader.pixbuf() {
                if let Some(scaled) = pixbuf.scale_simple(32, 32, gdk_pixbuf::InterpType::Bilinear)
                {
                    let texture = gtk::gdk::Texture::for_pixbuf(&scaled);
                    pic.set_paintable(Some(&texture));
                    return pic.upcast::<Widget>();
                }
            }
        }
    }
    let icon = Image::from_icon_name("list-add-symbolic");
    icon.set_pixel_size(32);
    icon.add_css_class("reaction-emoji-image");
    icon.upcast::<Widget>()
}

fn resolve_reaction_emoji_widget(
    clean_name: &str,
    raw_name: &str,
    context: &MessageHtmlContext,
    emoji_catalog: &crate::emoji::EmojiCatalog,
) -> Widget {
    match emoji_catalog.resolve(clean_name) {
        Some(crate::emoji::EmojiValue::Unicode(ch)) => {
            let lbl = Label::new(Some(ch));
            lbl.add_css_class("reaction-emoji-unicode");
            lbl.upcast()
        }
        Some(crate::emoji::EmojiValue::CustomImage(ref url)) => {
            if let Some(path) = resolve_cached_asset_path(url, context) {
                load_custom_emoji_picture(&path)
            } else {
                let icon = Image::from_icon_name("image-missing-symbolic");
                icon.set_pixel_size(32);
                icon.add_css_class("reaction-emoji-image");
                icon.upcast()
            }
        }
        None => {
            if let Some(unicode) = emojis::get(clean_name).or_else(|| emojis::get(raw_name)) {
                let lbl = Label::new(Some(unicode.as_str()));
                lbl.add_css_class("reaction-emoji-unicode");
                lbl.upcast()
            } else if !clean_name.is_ascii() {
                let lbl = Label::new(Some(clean_name));
                lbl.add_css_class("reaction-emoji-unicode");
                lbl.upcast()
            } else {
                let icon = Image::from_icon_name("image-missing-symbolic");
                icon.set_pixel_size(32);
                icon.add_css_class("reaction-emoji-image");
                icon.upcast()
            }
        }
    }
}

/// Shared hover state the quick bar buttons need to cooperate with the row.
struct QuickBarState<'a> {
    on_action: &'a ActionSlot,
    hover_generation: &'a Rc<Cell<u64>>,
    hovered: &'a Rc<RefCell<Option<HoveredMessage>>>,
    current_popover: &'a Rc<RefCell<Option<gtk::Popover>>>,
    bar_motion: &'a gtk::EventControllerMotion,
}

fn rebuild_quick_bar(
    quick_bar: &Box,
    ts: &str,
    reactions: &[crate::emoji::EmojiEntry],
    context: &MessageHtmlContext,
    state: &QuickBarState<'_>,
) {
    let QuickBarState {
        on_action,
        hover_generation,
        hovered,
        current_popover,
        bar_motion,
    } = *state;
    while let Some(child) = quick_bar.first_child() {
        quick_bar.remove(&child);
    }

    let emoji_catalog = crate::emoji::EmojiCatalog::new(&context.custom_emojis);

    for entry in reactions {
        let btn = Button::new();
        btn.set_focus_on_click(false);
        let widget =
            resolve_reaction_emoji_widget(&entry.name, &entry.name, context, &emoji_catalog);
        btn.set_child(Some(&widget));
        btn.add_css_class("flat");
        btn.add_css_class("circular");
        btn.set_tooltip_text(Some(&format!(":{}:", entry.name)));
        let ts = ts.to_string();
        let name = entry.name.clone();
        let on_action = on_action.clone();
        btn.connect_clicked(move |_| {
            dispatch_timeline_action(
                &on_action,
                TimelineAction::ToggleReaction {
                    ts: ts.clone(),
                    name: name.clone(),
                    add: true,
                },
            );
        });
        quick_bar.append(&btn);
    }

    let add_pic = load_add_reaction_widget();
    let add_btn = Button::new();
    add_btn.set_focus_on_click(false);
    add_btn.set_child(Some(&add_pic));
    add_btn.add_css_class("flat");
    add_btn.add_css_class("circular");
    add_btn.set_tooltip_text(Some("Add reaction..."));
    {
        let custom_emojis = context.custom_emojis.clone();
        let ts = ts.to_string();
        let on_action = on_action.clone();
        let weak_add_btn = add_btn.downgrade();
        add_btn.connect_clicked(move |_| {
            let Some(btn) = weak_add_btn.upgrade() else {
                return;
            };
            let on_action = on_action.clone();
            let ts = ts.clone();
            let picker = crate::emoji_picker_window::EmojiPickerWindow::new(
                &btn,
                &custom_emojis,
                move |selected_name| {
                    let clean = selected_name.trim().trim_matches(':');
                    if !clean.is_empty() {
                        dispatch_timeline_action(
                            &on_action,
                            TimelineAction::ToggleReaction {
                                ts: ts.clone(),
                                name: clean.to_string(),
                                add: true,
                            },
                        );
                    }
                },
            );
            picker.present();
        });
    }
    quick_bar.append(&add_btn);

    quick_bar.append(&gtk::Separator::new(Orientation::Vertical));

    let reply_btn = Button::from_icon_name("mail-reply-sender-symbolic");
    reply_btn.set_focus_on_click(false);
    reply_btn.add_css_class("flat");
    reply_btn.add_css_class("circular");
    reply_btn.set_tooltip_text(Some("Reply in thread"));
    {
        let ts = ts.to_string();
        let on_action = on_action.clone();
        reply_btn.connect_clicked(move |_| {
            dispatch_timeline_action(&on_action, TimelineAction::OpenThread(ts.clone()));
        });
    }
    quick_bar.append(&reply_btn);

    let forward_btn = Button::from_icon_name("mail-forward-symbolic");
    forward_btn.set_focus_on_click(false);
    forward_btn.add_css_class("flat");
    forward_btn.add_css_class("circular");
    forward_btn.set_tooltip_text(Some("Forward message..."));
    {
        let ts = ts.to_string();
        let on_action = on_action.clone();
        forward_btn.connect_clicked(move |_| {
            dispatch_timeline_action(&on_action, TimelineAction::ForwardMessage(ts.clone()));
        });
    }
    quick_bar.append(&forward_btn);

    let unread_btn = Button::from_icon_name("mail-mark-unread-symbolic");
    unread_btn.set_focus_on_click(false);
    unread_btn.add_css_class("flat");
    unread_btn.add_css_class("circular");
    unread_btn.set_tooltip_text(Some("Mark unread"));
    {
        let ts = ts.to_string();
        let on_action = on_action.clone();
        unread_btn.connect_clicked(move |_| {
            dispatch_timeline_action(&on_action, TimelineAction::MarkUnread(ts.clone()));
        });
    }
    quick_bar.append(&unread_btn);

    let overflow_btn = gtk::MenuButton::new();
    overflow_btn.set_focus_on_click(false);
    overflow_btn.set_icon_name("view-more-symbolic");
    overflow_btn.add_css_class("flat");
    overflow_btn.add_css_class("circular");
    overflow_btn.set_tooltip_text(Some("More actions"));

    let popover = gtk::Popover::new();
    let popover_box = Box::new(Orientation::Vertical, 0);

    let copy_link_btn = Button::with_label("Copy link");
    copy_link_btn.set_focus_on_click(false);
    copy_link_btn.add_css_class("flat");
    {
        let ts = ts.to_string();
        let on_action = on_action.clone();
        let popover_weak = popover.downgrade();
        copy_link_btn.connect_clicked(move |_| {
            dispatch_timeline_action(&on_action, TimelineAction::CopyMessageLink(ts.clone()));
            if let Some(p) = popover_weak.upgrade() {
                p.popdown();
            }
        });
    }
    popover_box.append(&copy_link_btn);

    let copy_text_btn = Button::with_label("Copy message");
    copy_text_btn.set_focus_on_click(false);
    copy_text_btn.add_css_class("flat");
    {
        let ts = ts.to_string();
        let on_action = on_action.clone();
        let popover_weak = popover.downgrade();
        copy_text_btn.connect_clicked(move |_| {
            dispatch_timeline_action(&on_action, TimelineAction::CopyMessageText(ts.clone()));
            if let Some(p) = popover_weak.upgrade() {
                p.popdown();
            }
        });
    }
    popover_box.append(&copy_text_btn);

    popover.set_child(Some(&popover_box));
    *current_popover.borrow_mut() = Some(popover.clone());

    {
        popover.connect_show(move |p| {
            crate::debug::log(
                "quickbar",
                &format!("popover show is_visible={}", p.is_visible()),
            );
        });
    }
    {
        let hover_generation = hover_generation.clone();
        let hovered = hovered.clone();
        let bar_motion = bar_motion.clone();
        let quick_bar_weak = quick_bar.downgrade();
        popover.connect_closed(move |p| {
            let contains_pointer = bar_motion.contains_pointer();
            crate::debug::log(
                "quickbar",
                &format!(
                    "popover closed is_visible={} bar_contains_pointer={contains_pointer}",
                    p.is_visible()
                ),
            );
            // The pointer never crosses the bar's boundary while the popover
            // has the grab, so no fresh `Enter` arrives here to tell us the
            // user is still hovering - ask the bar's own motion controller
            // directly instead of guessing from stale crossing events.
            if !contains_pointer {
                hover_generation.set(hover_generation.get().wrapping_add(1));
                *hovered.borrow_mut() = None;
                if let Some(bar) = quick_bar_weak.upgrade() {
                    bar.set_visible(false);
                    crate::debug::log("quickbar", "popover closed -> hiding quick bar");
                }
            } else {
                crate::debug::log(
                    "quickbar",
                    "popover closed -> pointer still over bar, keeping it visible",
                );
            }
        });
    }

    overflow_btn.set_popover(Some(&popover));
    quick_bar.append(&overflow_btn);
}

/// A row qualifies as "read" for auto-mark-read purposes once at least 80%
/// of either its own height or the viewport's height is within the visible
/// range `[0, viewport_height]` (`row_top`/`row_bottom` in the same
/// coordinate space, i.e. relative to the viewport's own origin).
fn row_qualifies_for_read(row_top: f64, row_bottom: f64, viewport_height: f64) -> bool {
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
fn row_message_ts(widget: &Widget) -> Option<String> {
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

/// Chronological store items for `messages` (newest first on input): a day
/// separator is inserted wherever the local calendar day changes. The store
/// is only ever rebuilt wholesale from the message list (`set_messages`) or
/// re-spliced item-for-item (`update_image_asset`), so separators are derived
/// here once and can never drift out of sync with inserts or prepends.
/// Also returns the index of `focus_ts` within the resulting items.
fn build_store_items(
    messages: &[SlackMessage],
    focus_ts: Option<&str>,
) -> (Vec<TimelineMessageObject>, Option<u32>) {
    let days: Vec<Option<i64>> = messages
        .iter()
        .rev()
        .map(|msg| {
            crate::message_html::slack_ts_datetime(&msg.ts)
                .map(|dt| crate::day_label::local_calendar_day(&dt))
        })
        .collect();
    let mut items = Vec::with_capacity(messages.len() + 4);
    let mut focus_index = None;
    for (i, msg) in messages.iter().rev().enumerate() {
        if crate::day_label::separator_before(&days, i) {
            items.push(TimelineMessageObject::day_separator(msg.clone()));
        }
        if focus_ts == Some(msg.ts.as_str()) && focus_index.is_none() {
            focus_index = Some(items.len() as u32);
        }
        items.push(TimelineMessageObject::new(msg.clone()));
    }
    (items, focus_index)
}

/// Full-width rule with the day label ("Today", "15 Sep", ...) centered on
/// it. Deliberately distinct from the red/accent unread separator.
fn day_separator_widget(ts: &str) -> Box {
    let label_text = crate::message_html::slack_ts_datetime(ts)
        .zip(gtk::glib::DateTime::now_local().ok())
        .and_then(|(dt, now)| crate::day_label::day_label(&dt, &now))
        .unwrap_or_default();

    let row = Box::new(Orientation::Horizontal, 12);
    row.set_margin_top(8);
    row.set_margin_bottom(8);
    row.set_margin_start(12);
    row.set_margin_end(12);
    row.update_property(&[gtk::accessible::Property::Label(&label_text)]);

    let append_line = || {
        let line = Separator::new(Orientation::Horizontal);
        line.add_css_class("day-separator-line");
        line.set_valign(gtk::Align::Center);
        line.set_hexpand(true);
        row.append(&line);
    };
    append_line();
    let label = Label::new(Some(&label_text));
    label.add_css_class("dim-label");
    label.add_css_class("caption");
    label.add_css_class("heading");
    row.append(&label);
    append_line();
    register_timeline_css();
    row
}

fn unread_separator_widget() -> Box {
    let separator_row = Box::new(Orientation::Horizontal, 6);
    separator_row.set_margin_top(4);
    separator_row.set_margin_bottom(4);

    let line = Separator::new(Orientation::Horizontal);
    line.add_css_class("unread-separator-line");
    line.set_valign(gtk::Align::Center);
    line.set_hexpand(true);
    separator_row.append(&line);

    let label = Label::new(Some("New"));
    label.add_css_class("caption");
    label.add_css_class("unread-separator-label");
    separator_row.append(&label);

    separator_row
}

/// Wraps `content` with the unread separator above it when `message_ts`
/// matches the frozen anchor in `context`. Must wrap in an outer VERTICAL
/// container regardless of `content`'s own orientation (e.g. the
/// horizontal system-message row), otherwise the separator would be laid
/// out side-by-side with the row instead of above it.
fn wrap_with_unread_separator_if_needed(
    content: Box,
    message_ts: &str,
    context: &MessageHtmlContext,
) -> Box {
    // Tag the row with its message ts (repurposing the CSS/accessible widget
    // name as a plain string tag, not used for CSS selection anywhere in this
    // app) so the visibility-based read-marking walk can recover which
    // message a given realized row widget corresponds to.
    content.set_widget_name(message_ts);
    if context.unread_separator_ts.as_deref() != Some(message_ts) {
        return content;
    }
    let outer = Box::new(Orientation::Vertical, 0);
    outer.append(&unread_separator_widget());
    outer.append(&content);
    outer.set_widget_name(message_ts);
    outer
}

/// Body, document nodes and files, all driven by the canonical
/// [`crate::rich_message::MessageDocument`] so cached and freshly fetched
/// messages render identically.
fn render_message_content(
    message: &SlackMessage,
    context: &MessageHtmlContext,
    on_open_media: Option<&OpenMediaCallback>,
    on_action: Option<&ActionHandler>,
    root_box: &Box,
) {
    let document = message.rendered_document();
    if !crate::timeline_document_widget::document_replaces_text(&document) {
        let content_text = message.text.as_deref().unwrap_or("");
        if !content_text.trim().is_empty() {
            render_text_content(content_text, root_box, context, on_action);
        }
    }

    let resolve = |url: &str| resolve_cached_asset_path(url, context);
    let is_failed = |url: &str| context.failed_image_urls.contains(url);
    let sources = crate::timeline_media::MediaSources {
        resolve: &resolve,
        is_failed: &is_failed,
    };
    crate::timeline_document_widget::DocumentRenderer::new(
        context,
        sources,
        &message.ts,
        on_action,
    )
    .render(document.nodes(), root_box);

    let files = message
        .files
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|file| !SlackMessage::document_renders_file(&document, file))
        .cloned()
        .collect::<Vec<_>>();
    if !files.is_empty() {
        render_files(&files, root_box, context, on_open_media, &message.ts);
    }
}

pub(crate) fn build_timeline_message_widget(
    message: &SlackMessage,
    context: &MessageHtmlContext,
    on_open_media: Option<&OpenMediaCallback>,
    on_open_thread: Option<&Rc<dyn Fn(String)>>,
    on_toggle_reaction: Option<&ToggleReactionCallback>,
    on_hover: Option<&Rc<dyn Fn(HoverEvent)>>,
    on_author_action: Option<&ActionHandler>,
) -> Box {
    register_timeline_css();
    if let Some(subtype) = message.subtype.as_deref() {
        if matches!(
            subtype,
            "channel_join" | "channel_leave" | "channel_topic" | "channel_purpose"
        ) {
            let root_box = Box::new(Orientation::Horizontal, 6);
            root_box.add_css_class("system-message");

            let icon_name = match subtype {
                "channel_join" | "channel_leave" => "emblem-shared-symbolic",
                _ => "dialog-information-symbolic",
            };
            let icon = Image::from_icon_name(icon_name);
            icon.set_pixel_size(16);
            root_box.append(&icon);

            let sys_text = message.text.as_deref().unwrap_or(match subtype {
                "channel_join" => "joined the channel",
                "channel_leave" => "left the channel",
                "channel_topic" => "set the channel topic",
                "channel_purpose" => "set the channel purpose",
                _ => "system event",
            });

            let pango = crate::message_html::mrkdwn_to_pango(sys_text, context);
            let text_widget =
                create_message_text_widget(&format!("<i>{}</i>", pango), context, on_author_action);
            text_widget.add_css_class("dim-label");
            root_box.append(&text_widget);

            return wrap_with_unread_separator_if_needed(root_box, &message.ts, context);
        }
    }

    let root_box = Box::new(Orientation::Vertical, 6);

    if message.is_thread_broadcast == Some(true)
        || message.subtype.as_deref() == Some("thread_broadcast")
    {
        let broadcast_box = Box::new(Orientation::Horizontal, 4);
        broadcast_box.add_css_class("thread-broadcast-banner");
        let pill_label = Label::new(Some("also sent to channel"));
        pill_label.add_css_class("dim-label");
        pill_label.add_css_class("pill");
        broadcast_box.append(&pill_label);
        root_box.append(&broadcast_box);
    }

    // Header
    let header_box = Box::new(Orientation::Horizontal, 8);

    let avatar_widget = build_avatar_widget(message, context);

    let author_name = context
        .display_user_id(message)
        .and_then(|user_id| {
            context
                .user_full_names
                .get(user_id)
                .cloned()
                .or_else(|| context.user_names.get(user_id).cloned())
        })
        .unwrap_or_else(|| message.author_label());

    let author_label = Label::new(None);
    author_label.set_markup(&format!(
        "<b>{}</b>",
        glib::markup_escape_text(&author_name)
    ));
    author_label.set_xalign(0.0);

    let author_user_id = context.display_user_id(message);
    let menu_access = crate::author_menu::author_menu_access(
        author_user_id,
        context.current_user_id.as_deref(),
        &context.bot_user_ids,
    );
    match (on_author_action, author_user_id) {
        (Some(on_action), Some(user_id)) if menu_access.any() => {
            header_box.append(&crate::author_menu::author_menu_button(
                &avatar_widget,
                &author_label,
                user_id,
                &author_name,
                menu_access,
                on_action.clone(),
            ));
        }
        _ => {
            header_box.append(&avatar_widget);
            header_box.append(&author_label);
        }
    }

    let timestamp_label = Label::new(None);
    if let Some((_machine, full, short)) =
        crate::message_html::localized_timestamp_parts(&message.ts)
    {
        timestamp_label.set_text(&short);
        timestamp_label.set_tooltip_text(Some(&full));
    } else {
        timestamp_label.set_text(&message.ts);
    }
    timestamp_label.add_css_class("dim-label");
    timestamp_label.set_xalign(0.0);
    header_box.append(&timestamp_label);

    root_box.append(&header_box);

    render_message_content(message, context, on_open_media, on_author_action, &root_box);

    // Reactions
    if let Some(reactions) = message.reactions.as_deref().filter(|r| !r.is_empty()) {
        register_timeline_css();
        let wrap_box = new_chip_wrap_box();
        wrap_box.add_css_class("reaction-row");

        let emoji_catalog = crate::emoji::EmojiCatalog::new(&context.custom_emojis);

        for r in reactions {
            let raw_name = r.name.as_deref().unwrap_or("reaction");
            let count = r.count.unwrap_or(1);
            let clean_name = raw_name.trim_matches(':');

            let emoji_widget =
                resolve_reaction_emoji_widget(clean_name, raw_name, context, &emoji_catalog);

            let has_self = if let Some(ref my_id) = context.current_user_id {
                r.users.as_ref().is_some_and(|users| users.contains(my_id))
            } else {
                false
            };

            let box_widget = Box::new(Orientation::Horizontal, 3);
            box_widget.append(&emoji_widget);

            let label = Label::new(Some(&count.to_string()));
            box_widget.append(&label);

            let pill_button = Button::new();
            pill_button.set_focus_on_click(false);
            pill_button.set_child(Some(&box_widget));
            pill_button.add_css_class("reaction-pill");
            pill_button.add_css_class("flat");
            if has_self {
                pill_button.add_css_class("reaction-pill-active");
            }

            if let Some(cb) = on_toggle_reaction {
                let cb = cb.clone();
                let message_ts = message.ts.clone();
                let clean_name_str = clean_name.to_string();
                pill_button.connect_clicked(move |_| {
                    cb(message_ts.clone(), clean_name_str.clone(), !has_self);
                });
            }

            let tooltip_text = if let Some(users) = &r.users {
                let reactor_names: Vec<&str> = users
                    .iter()
                    .filter_map(|user_id| context.user_names.get(user_id).map(|s| s.as_str()))
                    .collect();
                if !reactor_names.is_empty() {
                    format!(":{clean_name}: reacted by {}", reactor_names.join(", "))
                } else {
                    format!(":{clean_name}:")
                }
            } else {
                format!(":{clean_name}:")
            };
            pill_button.set_tooltip_text(Some(&tooltip_text));

            pill_button.set_halign(gtk::Align::Start);
            pill_button.set_valign(gtk::Align::Center);
            wrap_box.append(&pill_button);
        }

        let add_pic = load_add_reaction_widget();
        let add_btn = Button::new();
        add_btn.set_focus_on_click(false);
        add_btn.set_child(Some(&add_pic));
        add_btn.add_css_class("reaction-pill");
        add_btn.add_css_class("flat");
        add_btn.set_tooltip_text(Some("Add reaction..."));
        add_btn.set_halign(gtk::Align::Start);
        add_btn.set_valign(gtk::Align::Center);

        if let Some(cb) = on_toggle_reaction {
            let weak_add_btn = add_btn.downgrade();
            let custom_emojis = context.custom_emojis.clone();
            let message_ts = message.ts.clone();
            let cb = cb.clone();

            add_btn.connect_clicked(move |_| {
                let Some(btn) = weak_add_btn.upgrade() else {
                    return;
                };
                let cb = cb.clone();
                let message_ts = message_ts.clone();
                let picker = crate::emoji_picker_window::EmojiPickerWindow::new(
                    &btn,
                    &custom_emojis,
                    move |selected_name| {
                        let clean = selected_name.trim().trim_matches(':');
                        if !clean.is_empty() {
                            cb(message_ts.clone(), clean.to_string(), true);
                        }
                    },
                );
                picker.present();
            });
        }

        wrap_box.append(&add_btn);

        root_box.append(&wrap_box);
    }

    // Thread footer
    if message.reply_count.unwrap_or(0) > 0 {
        let reply_count = message.reply_count.unwrap();
        let thread_footer = Box::new(Orientation::Horizontal, 6);
        let count_str = if reply_count == 1 {
            "1 reply".to_string()
        } else {
            format!("{reply_count} replies")
        };
        let label_text = if let Some(latest_ts) = &message.latest_reply {
            if let Some((_m, _full, short)) =
                crate::message_html::localized_timestamp_parts(latest_ts)
            {
                format!("{count_str}   Last reply {short}")
            } else {
                count_str
            }
        } else {
            count_str
        };
        let reply_button = Button::with_label(&label_text);
        reply_button.set_focus_on_click(false);
        reply_button.add_css_class("flat");
        reply_button.add_css_class("thread-reply-pill");
        let replied = message.reply_users.as_deref().is_some_and(|users| {
            context
                .current_user_id
                .as_deref()
                .is_some_and(|me| users.iter().any(|u| u == me))
        });
        if replied {
            reply_button.add_css_class("thread-reply-active");
        }
        if let Some(cb) = on_open_thread {
            let cb = cb.clone();
            let ts = message.ts.clone();
            reply_button.connect_clicked(move |_| {
                cb(ts.clone());
            });
        }
        thread_footer.append(&reply_button);
        root_box.append(&thread_footer);
    }

    if let Some(cb) = on_hover {
        let motion = gtk::EventControllerMotion::new();
        let cb_enter = cb.clone();
        let ts = message.ts.clone();
        let reactions = crate::message_html::recent_reactions(context);
        let row_for_enter = root_box.clone().upcast::<Widget>();
        motion.connect_enter(move |_, _, _| {
            cb_enter(HoverEvent::Enter {
                row: row_for_enter.clone(),
                ts: ts.clone(),
                reactions: reactions.clone(),
            });
        });
        let cb_leave = cb.clone();
        motion.connect_leave(move |_| {
            cb_leave(HoverEvent::Leave);
        });
        root_box.add_controller(motion);
    }

    wrap_with_unread_separator_if_needed(root_box, &message.ts, context)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::message_html::MessageHtmlContext;
    use crate::models::{
        SlackAttachment, SlackAttachmentField, SlackFile, SlackMessage, SlackReaction,
    };

    #[test]
    fn dispatch_allows_handler_to_replace_itself() {
        let on_action: ActionSlot = Rc::new(RefCell::new(None));
        let replaced = Rc::new(Cell::new(false));
        let slot = on_action.clone();
        let flag = replaced.clone();
        *on_action.borrow_mut() = Some(Rc::new(move |_| {
            // Mirrors a re-render calling `set_on_action` from inside a handler.
            *slot.borrow_mut() = Some(Rc::new(|_| {}));
            flag.set(true);
        }));

        dispatch_timeline_action(&on_action, TimelineAction::OpenThread("1.0".to_string()));

        assert!(replaced.get());
    }

    #[test]
    fn strip_anchor_tags_returns_link_ranges_of_visible_text() {
        let (clean, links) =
            strip_anchor_tags("a &amp; <b>b</b> <a href=\"https://x.io/?a=1&amp;b=2\">link</a> z");
        assert_eq!(clean, "a &amp; <b>b</b> link z");
        assert_eq!(
            links,
            vec![MarkupLink {
                start: 6,
                end: 10,
                href: "https://x.io/?a=1&b=2".into()
            }]
        );
    }

    pub(crate) fn test_context() -> MessageHtmlContext {
        MessageHtmlContext {
            user_names: Arc::new(HashMap::from([("U123".to_string(), "Alice".to_string())])),
            user_full_names: Arc::default(),
            user_avatar_urls: Arc::default(),
            bot_user_ids: Arc::default(),
            conversation_titles: HashMap::default(),
            private_conversation_ids: std::collections::HashSet::default(),
            user_statuses: Arc::default(),
            user_group_names: Arc::default(),
            user_group_members: Arc::default(),
            current_user_id: None,
            thread_ts: None,
            load_more_url: None,
            timeline_scroll: crate::message_html::TimelineScrollBehavior::default(),
            image_assets: HashMap::default(),
            failed_image_urls: std::collections::HashSet::default(),
            recent_reactions: Vec::new(),
            custom_emojis: Arc::default(),
            timeline_generation: None,
            last_read: None,
            unread_separator_ts: None,
            message_control_handles: HashMap::default(),
            message_control_action_handles: HashMap::default(),
        }
    }

    type GtkTestJob = std::boxed::Box<dyn FnOnce() + Send>;
    static GTK_TEST_RUNNER: std::sync::OnceLock<Option<std::sync::mpsc::Sender<GtkTestJob>>> =
        std::sync::OnceLock::new();

    pub(crate) fn run_gtk_test<F: FnOnce() + Send + 'static>(f: F) {
        let sender = GTK_TEST_RUNNER.get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<std::boxed::Box<dyn FnOnce() + Send>>();
            let (init_tx, init_rx) = std::sync::mpsc::channel::<bool>();
            std::thread::Builder::new()
                .name("gtk-test-worker".into())
                .spawn(move || {
                    let ok = std::panic::catch_unwind(gtk::init)
                        .ok()
                        .and_then(|r| r.ok())
                        .is_some();
                    let _ = init_tx.send(ok);
                    if !ok {
                        return;
                    }
                    while let Ok(job) = rx.recv() {
                        job();
                    }
                })
                .ok()?;
            if init_rx.recv().unwrap_or(false) {
                Some(tx)
            } else {
                None
            }
        });

        let Some(tx) = sender.as_ref() else {
            return;
        };

        let (done_tx, done_rx) =
            std::sync::mpsc::channel::<Result<(), std::boxed::Box<dyn std::any::Any + Send>>>();
        let job = std::boxed::Box::new(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            let _ = done_tx.send(res);
        });
        if tx.send(job).is_ok() {
            if let Ok(Err(panic_payload)) = done_rx.recv() {
                std::panic::resume_unwind(panic_payload);
            }
        }
    }

    #[test]
    fn rich_text_channel_element_renders_as_pill_instead_of_vanishing() {
        let token = crate::timeline_document_widget::inlines_to_mrkdwn(&[
            crate::rich_message::RichInline::Channel("C123".to_string()),
        ]);
        assert_eq!(token, "<#C123>");

        let mut context = test_context();
        context
            .conversation_titles
            .insert("C123".to_string(), "#general".to_string());
        let pango = crate::message_html::mrkdwn_to_pango(&token, &context);
        assert!(pango.contains("background=\"#D6ECFF\""));
        assert!(pango.contains("general"));
    }

    #[test]
    fn media_collapse_key_is_stable_and_namespaced_per_slot() {
        let a = media_collapse_key("1710000000.000100", "file:0");
        let b = media_collapse_key("1710000000.000100", "file:0");
        assert_eq!(
            a, b,
            "querying the same ts/slot twice must yield the same key"
        );

        let c = media_collapse_key("1710000000.000100", "attachment:0");
        assert_ne!(
            a, c,
            "a file and an attachment at the same index must not collide"
        );
    }

    fn descendants(root: &Widget) -> Vec<Widget> {
        let mut found = Vec::new();
        let mut child = root.first_child();
        while let Some(widget) = child {
            found.push(widget.clone());
            found.extend(descendants(&widget));
            child = widget.next_sibling();
        }
        found
    }

    fn label_texts(root: &Widget) -> Vec<String> {
        descendants(root)
            .into_iter()
            .filter_map(|widget| widget.downcast::<Label>().ok())
            .map(|label| label.text().to_string())
            .collect()
    }

    fn cached_roundtrip(message: SlackMessage) -> SlackMessage {
        let stored =
            serde_json::to_value(crate::slack_message_wire::normalize_cached_message(message))
                .expect("message serializes for the cache");
        crate::slack_message_wire::normalize_cached_message(
            serde_json::from_value(stored).expect("cached message deserializes"),
        )
    }

    fn giphy_command_message() -> SlackMessage {
        crate::slack_message_wire::SlackMessageWire::from_value(serde_json::json!({
            "type": "message",
            "user": "U04R5M67EBV",
            "bot_id": "B8D9J9E80",
            "app_id": "A0F827J2C",
            "text": "plumber",
            "ts": "1789462599.765779",
            "bot_profile": {
                "id": "B8D9J9E80",
                "app_id": "A0F827J2C",
                "name": "giphy",
                "icons": {"image_72": "https://a.slack-edge.com/dc483/img/plugins/giphy/service_72.png"}
            },
            "blocks": [
                {
                    "type": "image",
                    "block_id": "giphy",
                    "title": {"type": "plain_text", "text": "plumber", "emoji": true},
                    "image_url": "https://media4.giphy.com/media/9TbgGqK1KhpnNORo9a/giphy.gif?cid=1&rid=giphy.gif&ct=g",
                    "alt_text": "plumber",
                    "image_width": 480,
                    "image_height": 270,
                    "image_bytes": 1234567,
                    "is_animated": true
                },
                {
                    "type": "context",
                    "elements": [
                        {
                            "type": "image",
                            "image_url": "https://a.slack-edge.com/dc483/img/plugins/giphy/service_32.png",
                            "alt_text": "giphy logo"
                        },
                        {
                            "type": "mrkdwn",
                            "text": "Posted using /giphy | GIF by <https://giphy.com/channel/fuzzyghost/|fuzzyghost>"
                        }
                    ]
                }
            ]
        }))
        .into_message()
        .expect("giphy message parses")
    }

    #[test]
    fn giphy_command_message_keeps_title_image_context_and_user_through_cache() {
        let message = cached_roundtrip(giphy_command_message());

        assert_eq!(message.app_invoking_user_id(), Some("U04R5M67EBV"));
        let crate::rich_message::MessageNode::Image(image) = &message.document.nodes()[0] else {
            panic!(
                "first node should be the GIF image: {:?}",
                message.document.nodes()
            );
        };
        assert_eq!(image.title.as_deref(), Some("plumber"));
        assert_eq!((image.width, image.height), (Some(480), Some(270)));
        let crate::rich_message::MessageNode::Context(elements) = &message.document.nodes()[1]
        else {
            panic!("second node should be the context line");
        };
        assert!(matches!(
            &elements[0],
            crate::rich_message::MessageContextElement::Image(icon)
                if icon.url.as_deref()
                    == Some("https://a.slack-edge.com/dc483/img/plugins/giphy/service_32.png")
        ));
        assert_eq!(
            message.document.image_urls().collect::<Vec<_>>(),
            vec![
                "https://media4.giphy.com/media/9TbgGqK1KhpnNORo9a/giphy.gif?cid=1&rid=giphy.gif&ct=g",
                "https://a.slack-edge.com/dc483/img/plugins/giphy/service_32.png",
            ]
        );
    }

    #[test]
    fn app_message_author_prefers_known_invoking_person_only() {
        let message = cached_roundtrip(giphy_command_message());
        let mut ctx = test_context();
        assert_eq!(
            ctx.display_user_id(&message),
            None,
            "unknown user keeps the app"
        );

        Arc::make_mut(&mut ctx.user_full_names)
            .insert("U04R5M67EBV".to_string(), "Robey Groeneweg".to_string());
        assert_eq!(ctx.display_user_id(&message), Some("U04R5M67EBV"));

        Arc::make_mut(&mut ctx.bot_user_ids).insert("U04R5M67EBV".to_string());
        assert_eq!(
            ctx.display_user_id(&message),
            None,
            "bot users keep the app"
        );
    }

    #[test]
    fn giphy_command_message_renders_user_title_image_and_context() {
        run_gtk_test(|| {
            let message = cached_roundtrip(giphy_command_message());
            let mut ctx = test_context();
            Arc::make_mut(&mut ctx.user_full_names)
                .insert("U04R5M67EBV".to_string(), "Robey Groeneweg".to_string());

            let widget =
                build_timeline_message_widget(&message, &ctx, None, None, None, None, None)
                    .upcast::<Widget>();
            let texts = label_texts(&widget);

            assert!(
                texts.iter().any(|text| text == "Robey Groeneweg"),
                "{texts:?}"
            );
            assert!(!texts.iter().any(|text| text == "giphy"), "{texts:?}");
            assert!(texts.iter().any(|text| text == "plumber"), "{texts:?}");
            assert!(
                texts
                    .iter()
                    .any(|text| text.starts_with("Posted using /giphy | GIF by")),
                "{texts:?}"
            );
            assert!(descendants(&widget)
                .iter()
                .any(|widget| widget.has_css_class("timeline-media-title")));
            assert!(descendants(&widget)
                .iter()
                .any(|widget| widget.has_css_class("timeline-media-placeholder")));
        });
    }

    #[test]
    fn cached_gif_picker_message_renders_titled_placeholder() {
        run_gtk_test(|| {
            let cached: SlackMessage = serde_json::from_value(serde_json::json!({
                "type": "message",
                "user": "U015HMNHYES",
                "text": "",
                "ts": "1789462245.306079",
                "files": null,
                "blocks": null,
                "attachments": null,
                "author": {"User": {"user_id": "U015HMNHYES"}},
                "document": {
                    "nodes": [{"Image": {
                        "url": "https://media1.giphy.com/media/VF4jocEMAWVAVYRIQu/200w.gif?cid=b7&rid=200w.gif&ct=g",
                        "alt": "Oh Yeah Yes GIF by FILMRISE",
                        "title": "GIF"
                    }}],
                    "accessible_fallback": null
                },
                "content_version": 1
            }))
            .expect("cached GIF picker message deserializes");
            let message = crate::slack_message_wire::normalize_cached_message(cached);

            let widget = build_timeline_message_widget(
                &message,
                &test_context(),
                None,
                None,
                None,
                None,
                None,
            )
            .upcast::<Widget>();

            assert!(label_texts(&widget).iter().any(|text| text == "GIF"));
            let placeholder = descendants(&widget)
                .into_iter()
                .find(|widget| widget.has_css_class("timeline-media-placeholder"))
                .expect("pending GIF reserves a placeholder");
            assert_eq!(
                placeholder.tooltip_text().as_deref(),
                Some("Oh Yeah Yes GIF by FILMRISE")
            );
        });
    }

    #[test]
    fn downloaded_document_image_renders_picture_fitted_to_media_box() {
        run_gtk_test(|| {
            let dir =
                std::env::temp_dir().join(format!("conduit-media-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let path = dir.join("wide.png");
            gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, 960, 540)
                .expect("pixbuf")
                .savev(&path, "png", &[])
                .expect("png written");

            let image = crate::rich_message::MessageImage::new(
                Some("https://media1.giphy.com/media/x/giphy.gif".to_string()),
                "wide",
                Some("GIF".to_string()),
            );
            let resolved = path.clone();
            let resolve = move |_: &str| Some(resolved.clone());
            let is_failed = |_: &str| false;
            let target = Box::new(Orientation::Vertical, 0);
            crate::timeline_document_widget::DocumentRenderer::new(
                &test_context(),
                crate::timeline_media::MediaSources {
                    resolve: &resolve,
                    is_failed: &is_failed,
                },
                "1789462245.306079",
                None,
            )
            .render(&[crate::rich_message::MessageNode::Image(image)], &target);

            let picture = descendants(target.upcast_ref())
                .into_iter()
                .find_map(|widget| widget.downcast::<Picture>().ok())
                .expect("downloaded image renders a picture");
            assert_eq!(picture.size_request(), (480, 270));
            assert!(picture.has_css_class("timeline-media-image"));

            let _ = std::fs::remove_dir_all(dir);
        });
    }

    #[test]
    fn file_shown_as_document_image_is_not_rendered_twice() {
        run_gtk_test(|| {
            let url = "https://files.slack.com/files-pri/T1-F1/photo.png";
            let mut message = SlackMessage {
                ts: "1700000000.000500".to_string(),
                user: Some("U123".to_string()),
                files: Some(vec![SlackFile {
                    id: Some("F1".to_string()),
                    mimetype: Some("image/png".to_string()),
                    url_private: Some(url.to_string()),
                    thumb_360: Some(url.to_string()),
                    ..Default::default()
                }]),
                blocks: Some(serde_json::json!([{
                    "type": "image",
                    "alt_text": "photo",
                    "slack_file": {"url": url}
                }])),
                ..Default::default()
            };
            message.refresh_canonical_content();

            let widget = build_timeline_message_widget(
                &message,
                &test_context(),
                None,
                None,
                None,
                None,
                None,
            )
            .upcast::<Widget>();
            let all = descendants(&widget);

            assert!(!all
                .iter()
                .any(|widget| widget.has_css_class("timeline-image-container")));
            assert_eq!(
                all.iter()
                    .filter(|widget| widget.downcast_ref::<ToggleButton>().is_some())
                    .count(),
                1
            );
        });
    }

    #[test]
    fn test_parse_text_segments() {
        let text = "Hello\n```rust\nfn main() {}\n```\n> quoted\nMore text";
        let segments = parse_text_segments(text);
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[0], TextSegment::Normal("Hello".to_string()));
        assert_eq!(
            segments[1],
            TextSegment::CodeBlock {
                lang: Some("rust".to_string()),
                code: "fn main() {}".to_string(),
            }
        );
        assert_eq!(segments[2], TextSegment::Quote("quoted".to_string()));
        assert_eq!(segments[3], TextSegment::Normal("More text".to_string()));
    }

    #[test]
    fn test_file_size_formatting() {
        assert_eq!(format_file_size(500), "500 B");
        assert_eq!(format_file_size(2048), "2.0 KB");
        assert_eq!(format_file_size(1572864), "1.5 MB");
        assert_eq!(format_file_size(10737418240), "10.0 GB");
    }

    fn find_descendant(widget: &Widget, pred: &dyn Fn(&Widget) -> bool) -> Option<Widget> {
        if pred(widget) {
            return Some(widget.clone());
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Some(found) = find_descendant(&current, pred) {
                return Some(found);
            }
            child = current.next_sibling();
        }
        None
    }

    fn find_menu_button(widget: &Widget) -> Option<gtk::MenuButton> {
        if let Some(button) = widget.downcast_ref::<gtk::MenuButton>() {
            return Some(button.clone());
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Some(found) = find_menu_button(&current) {
                return Some(found);
            }
            child = current.next_sibling();
        }
        None
    }

    #[test]
    fn author_header_is_a_menu_target_for_people_only() {
        run_gtk_test(|| {
            let message = SlackMessage {
                ts: "1700000000.000100".to_string(),
                user: Some("U123".to_string()),
                text: Some("hi".to_string()),
                ..Default::default()
            };

            let seen: Rc<RefCell<Vec<String>>> = Rc::default();
            let recorder = seen.clone();
            let on_action: Rc<dyn Fn(TimelineAction)> = Rc::new(move |action| {
                recorder.borrow_mut().push(format!("{action:?}"));
            });

            let widget = build_timeline_message_widget(
                &message,
                &test_context(),
                None,
                None,
                None,
                None,
                Some(&on_action),
            );
            let button = find_menu_button(widget.upcast_ref()).expect("author menu button");
            assert!(button.menu_model().is_some());
            button.activate_action("author.profile", None).unwrap();
            button.activate_action("author.message", None).unwrap();
            assert_eq!(
                *seen.borrow(),
                ["ShowProfile(\"U123\")", "MessageUser(\"U123\")"]
            );

            // Yourself: Message is disabled, Profile stays available.
            let mut own = test_context();
            own.current_user_id = Some("U123".to_string());
            let widget = build_timeline_message_widget(
                &message,
                &own,
                None,
                None,
                None,
                None,
                Some(&on_action),
            );
            let button = find_menu_button(widget.upcast_ref()).expect("author menu button");
            seen.borrow_mut().clear();
            button.activate_action("author.message", None).unwrap();
            assert!(seen.borrow().is_empty());
            button.activate_action("author.profile", None).unwrap();
            assert_eq!(seen.borrow().len(), 1);

            // Bots get a plain header.
            let mut bot = test_context();
            bot.bot_user_ids = Arc::new(HashSet::from(["U123".to_string()]));
            let widget = build_timeline_message_widget(
                &message,
                &bot,
                None,
                None,
                None,
                None,
                Some(&on_action),
            );
            assert!(find_menu_button(widget.upcast_ref()).is_none());
        });
    }

    #[test]
    fn test_timeline_message_widget_all_gtk() {
        run_gtk_test(|| {
            let ctx = test_context();

            // 1. Basic message
            let msg1 = SlackMessage {
                ts: "1700000000.000100".to_string(),
                user: Some("U123".to_string()),
                text: Some("Hello *world*!".to_string()),
                reactions: Some(vec![
                    SlackReaction {
                        name: Some("thumbsup".to_string()),
                        count: Some(3),
                        users: None,
                    },
                    SlackReaction {
                        name: Some("smile".to_string()),
                        count: Some(1),
                        users: None,
                    },
                ]),
                reply_count: Some(5),
                ..Default::default()
            };

            let widget1 = build_timeline_message_widget(&msg1, &ctx, None, None, None, None, None);
            assert_eq!(widget1.orientation(), Orientation::Vertical);

            // 2. Section blocks and fields
            let msg2 = SlackMessage {
                ts: "1700000000.000200".to_string(),
                user: Some("U123".to_string()),
                blocks: Some(serde_json::json!([
                    {
                        "type": "header",
                        "text": { "type": "plain_text", "text": "Header Title" }
                    },
                    {
                        "type": "section",
                        "text": { "type": "mrkdwn", "text": "Main section content" },
                        "fields": [
                            { "type": "mrkdwn", "text": "*Field 1*" },
                            { "type": "mrkdwn", "text": "*Field 2*" }
                        ]
                    }
                ])),
                ..Default::default()
            };

            let widget2 = build_timeline_message_widget(&msg2, &ctx, None, None, None, None, None);
            assert_eq!(widget2.orientation(), Orientation::Vertical);

            // 3. Divider, actions and context
            let msg3 = SlackMessage {
                ts: "1700000000.000300".to_string(),
                user: Some("U123".to_string()),
                blocks: Some(serde_json::json!([
                    { "type": "divider" },
                    {
                        "type": "actions",
                        "elements": [
                            { "type": "button", "text": { "type": "plain_text", "text": "Approve" } },
                            { "type": "button", "text": { "type": "plain_text", "text": "Reject" } }
                        ]
                    },
                    {
                        "type": "context",
                        "elements": [
                            { "type": "mrkdwn", "text": "Footer context info" }
                        ]
                    }
                ])),
                ..Default::default()
            };

            let widget3 = build_timeline_message_widget(&msg3, &ctx, None, None, None, None, None);
            assert_eq!(widget3.orientation(), Orientation::Vertical);

            // 4. Attachments with color border
            let msg4 = SlackMessage {
                ts: "1700000000.000400".to_string(),
                user: Some("U123".to_string()),
                attachments: Some(vec![SlackAttachment {
                    color: Some("good".to_string()),
                    pretext: Some("Pretext label".to_string()),
                    title: Some("Attachment Title".to_string()),
                    title_link: Some("https://example.com".to_string()),
                    text: Some("Attachment body text".to_string()),
                    fields: Some(vec![SlackAttachmentField {
                        title: Some("Priority".to_string()),
                        value: Some("High".to_string()),
                        short: Some(true),
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            };

            let widget4 = build_timeline_message_widget(&msg4, &ctx, None, None, None, None, None);
            assert_eq!(widget4.orientation(), Orientation::Vertical);

            // 5. Image file
            let msg_img = SlackMessage {
                ts: "1700000000.000500".to_string(),
                user: Some("U123".to_string()),
                files: Some(vec![SlackFile {
                    id: Some("F1".to_string()),
                    name: Some("photo.png".to_string()),
                    title: Some("Sample Photo".to_string()),
                    mimetype: Some("image/png".to_string()),
                    thumb_360: Some("https://example.com/thumb.png".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            };

            let widget_img =
                build_timeline_message_widget(&msg_img, &ctx, None, None, None, None, None);
            assert_eq!(widget_img.orientation(), Orientation::Vertical);

            // 6. Video file
            let msg_vid = SlackMessage {
                ts: "1700000000.000600".to_string(),
                user: Some("U123".to_string()),
                files: Some(vec![SlackFile {
                    id: Some("F2".to_string()),
                    name: Some("demo.mp4".to_string()),
                    title: Some("Demo Recording".to_string()),
                    mimetype: Some("video/mp4".to_string()),
                    thumb_video: Some("https://example.com/video_thumb.png".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            };

            let widget_vid =
                build_timeline_message_widget(&msg_vid, &ctx, None, None, None, None, None);
            assert_eq!(widget_vid.orientation(), Orientation::Vertical);

            // 7. Document file with size
            let msg_doc = SlackMessage {
                ts: "1700000000.000700".to_string(),
                user: Some("U123".to_string()),
                files: Some(vec![SlackFile {
                    id: Some("F3".to_string()),
                    name: Some("report.pdf".to_string()),
                    title: Some("Annual Report".to_string()),
                    mimetype: Some("application/pdf".to_string()),
                    size: Some(2097152),
                    ..Default::default()
                }]),
                ..Default::default()
            };

            let widget_doc =
                build_timeline_message_widget(&msg_doc, &ctx, None, None, None, None, None);
            assert_eq!(widget_doc.orientation(), Orientation::Vertical);

            // 8. Rich text blocks
            let msg_rich = SlackMessage {
                ts: "1700000000.000800".to_string(),
                user: Some("U123".to_string()),
                blocks: Some(serde_json::json!([
                    {
                        "type": "rich_text",
                        "elements": [
                            {
                                "type": "rich_text_section",
                                "elements": [
                                    { "type": "text", "text": "Rich text section ", "style": { "bold": true } },
                                    { "type": "link", "url": "https://example.com", "text": "click here" }
                                ]
                            },
                            {
                                "type": "rich_text_list",
                                "style": "bullet",
                                "indent": 1,
                                "elements": [
                                    {
                                        "type": "rich_text_section",
                                        "elements": [ { "type": "text", "text": "Bullet item 1" } ]
                                    },
                                    {
                                        "type": "rich_text_section",
                                        "elements": [ { "type": "text", "text": "Bullet item 2" } ]
                                    }
                                ]
                            },
                            {
                                "type": "rich_text_preformatted",
                                "elements": [
                                    { "type": "text", "text": "let x = 42;" }
                                ]
                            },
                            {
                                "type": "rich_text_quote",
                                "elements": [
                                    { "type": "text", "text": "A quoted phrase" }
                                ]
                            }
                        ]
                    }
                ])),
                ..Default::default()
            };

            let widget_rich =
                build_timeline_message_widget(&msg_rich, &ctx, None, None, None, None, None);
            assert_eq!(widget_rich.orientation(), Orientation::Vertical);

            // 9. Subtype system message
            let msg_sys = SlackMessage {
                ts: "1700000000.000900".to_string(),
                user: Some("U123".to_string()),
                subtype: Some("channel_join".to_string()),
                text: Some("joined the channel".to_string()),
                ..Default::default()
            };

            let widget_sys =
                build_timeline_message_widget(&msg_sys, &ctx, None, None, None, None, None);
            assert_eq!(widget_sys.orientation(), Orientation::Horizontal);

            // 10. Thread broadcast banner
            let msg_bc = SlackMessage {
                ts: "1700000000.0001000".to_string(),
                user: Some("U123".to_string()),
                text: Some("Broadcast reply".to_string()),
                is_thread_broadcast: Some(true),
                ..Default::default()
            };

            let widget_bc =
                build_timeline_message_widget(&msg_bc, &ctx, None, None, None, None, None);
            assert_eq!(widget_bc.orientation(), Orientation::Vertical);

            // 11. Native timeline view & update_image_asset
            let timeline_view = NativeTimelineView::new();
            timeline_view.set_messages(&[msg1, msg2], &ctx, None);
            // Two same-day messages: one leading day separator plus both rows.
            assert_eq!(timeline_view.store.n_items(), 3);
            timeline_view.update_image_asset(&ctx);
            assert_eq!(timeline_view.store.n_items(), 3);

            // Moving the unread separator rebuilds rows in place.
            let separator_ts = |view: &NativeTimelineView| {
                view.context
                    .borrow()
                    .as_ref()
                    .and_then(|context| context.unread_separator_ts.clone())
            };
            let second_ts = timeline_view
                .store
                .item(2)
                .and_downcast::<TimelineMessageObject>()
                .expect("message row")
                .message()
                .ts;
            timeline_view.set_unread_separator(Some(second_ts.clone()));
            assert_eq!(separator_ts(&timeline_view), Some(second_ts));
            assert_eq!(timeline_view.store.n_items(), 3);
            timeline_view.set_unread_separator(None);
            assert_eq!(separator_ts(&timeline_view), None);
            assert_eq!(timeline_view.store.n_items(), 3);

            // 12. Custom emoji reactions & resolve_cached_asset_path
            let mut ctx_emoji = ctx.clone();
            std::sync::Arc::make_mut(&mut ctx_emoji.custom_emojis).insert(
                "party_blob".to_string(),
                "https://example.com/blob.gif".to_string(),
            );
            let msg_reaction = SlackMessage {
                ts: "1700000000.001100".to_string(),
                user: Some("U123".to_string()),
                text: Some("Reaction test".to_string()),
                reactions: Some(vec![
                    SlackReaction {
                        name: Some(":robot_face:".to_string()),
                        count: Some(5),
                        ..Default::default()
                    },
                    SlackReaction {
                        name: Some(":party_blob:".to_string()),
                        count: Some(2),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            };
            let widget_rx = build_timeline_message_widget(
                &msg_reaction,
                &ctx_emoji,
                None,
                None,
                None,
                None,
                None,
            );
            assert_eq!(widget_rx.orientation(), Orientation::Vertical);

            let path = resolve_cached_asset_path("nonexistent_key", &ctx_emoji);
            assert!(path.is_none());

            // 13. Direct disk cache hash lookup test
            let cache_dir = crate::config::image_asset_cache_dir();
            let ws_dir = cache_dir.join("test_ws_direct");
            let _ = std::fs::create_dir_all(&ws_dir);

            let test_url = "https://example.com/direct_test_img.png";
            let hash = {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update("test_ws_direct".as_bytes());
                hasher.update([0]);
                hasher.update(test_url.as_bytes());
                format!("{:x}", hasher.finalize())
            };
            let dummy_file = ws_dir.join(format!("{hash}.png"));
            std::fs::write(&dummy_file, b"test").unwrap();

            let found = resolve_cached_asset_path(test_url, &ctx);
            assert_eq!(found, Some(dummy_file.clone()));

            let _ = std::fs::remove_file(dummy_file);
            let _ = std::fs::remove_dir(ws_dir);
        });
    }

    #[test]
    fn test_reaction_row_is_adw_wrap_box() {
        run_gtk_test(|| {
            let ctx = test_context();
            let msg = SlackMessage {
                ts: "1700000000.000900".to_string(),
                user: Some("U123".to_string()),
                text: Some("hi".to_string()),
                reactions: Some(vec![
                    SlackReaction {
                        name: Some("thumbsup".to_string()),
                        count: Some(2),
                        users: None,
                    },
                    SlackReaction {
                        name: Some("smile".to_string()),
                        count: Some(1),
                        users: None,
                    },
                ]),
                ..Default::default()
            };
            let widget = build_timeline_message_widget(&msg, &ctx, None, None, None, None, None);
            let row = find_descendant(&widget.clone().upcast(), &|w| {
                w.has_css_class("reaction-row")
            })
            .expect("reaction row");
            let wrap = row.downcast::<adw::WrapBox>().expect("adw::WrapBox");
            assert_eq!(wrap.child_spacing(), 4);
            let mut pills = 0;
            let mut child = wrap.first_child();
            while let Some(c) = child {
                assert!(c.is::<Button>());
                assert!(c.has_css_class("reaction-pill"));
                pills += 1;
                child = c.next_sibling();
            }
            assert_eq!(pills, 3);
        });
    }

    #[test]
    fn row_qualifies_when_fully_visible() {
        assert!(row_qualifies_for_read(0.0, 100.0, 500.0));
    }

    #[test]
    fn row_qualifies_via_own_height_even_if_small_viewport_fraction() {
        // A short row (40px) fully on-screen inside a tall viewport (2000px)
        // satisfies the row-height condition even though it's a tiny sliver
        // of the viewport.
        assert!(row_qualifies_for_read(10.0, 50.0, 2000.0));
    }

    #[test]
    fn row_qualifies_via_viewport_height_for_a_row_taller_than_the_viewport() {
        // A row taller than the viewport can never show 80% of itself, but
        // qualifies once it covers 80% of the viewport instead.
        assert!(row_qualifies_for_read(-500.0, 1000.0, 400.0));
    }

    #[test]
    fn row_does_not_qualify_when_mostly_scrolled_past() {
        // Only the top 20px of a 100px row is visible in a tall viewport.
        assert!(!row_qualifies_for_read(-80.0, 20.0, 2000.0));
    }

    #[test]
    fn row_does_not_qualify_when_fully_offscreen() {
        assert!(!row_qualifies_for_read(600.0, 700.0, 500.0));
        assert!(!row_qualifies_for_read(-200.0, -100.0, 500.0));
    }

    #[test]
    fn row_does_not_qualify_for_degenerate_sizes() {
        assert!(!row_qualifies_for_read(0.0, 0.0, 500.0));
        assert!(!row_qualifies_for_read(0.0, 100.0, 0.0));
    }

    #[test]
    fn test_extract_custom_emojis_and_prepare_markup() {
        let pango = "Hello <b><conduit-custom-emoji name=\"heart-sparkle\" url=\"https://example.com/heart.gif\"/></b> world";
        let (clean, emojis) = extract_custom_emojis_and_prepare_markup(pango);
        assert_eq!(clean, "Hello <b>\u{FFFC}</b> world");
        assert_eq!(emojis.len(), 1);
        assert_eq!(emojis[0].name, "heart-sparkle");
        assert_eq!(emojis[0].url, "https://example.com/heart.gif");
    }

    #[test]
    fn test_create_message_text_widget_with_custom_emoji() {
        run_gtk_test(|| {
            let context = test_context();
            let pango = "Hi <b><conduit-custom-emoji name=\"heart-sparkle\" url=\"https://example.com/heart.gif\"/></b> there";
            let widget = create_message_text_widget(pango, &context, None);
            let view = widget.downcast::<TextView>().ok();
            assert!(view.is_some(), "expected TextView for custom emoji markup");
        });
    }

    #[test]
    fn test_create_message_text_widget_with_user_mention() {
        run_gtk_test(|| {
            let context = test_context();
            let pango = "<a href=\"conduit-user://U123\"><span background=\"#D6ECFF\" foreground=\"#1264A3\"> @Alice </span></a>";
            let widget = create_message_text_widget(pango, &context, None);
            let label = widget.downcast::<Label>().ok();
            assert!(
                label.is_some(),
                "expected Label for standard mention markup"
            );
        });
    }
}
