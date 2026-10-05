use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib::subclass::prelude::*;
use gtk::{
    gio, glib, pango, Box, Button, CssProvider, Grid, Image, Label,
    ListView, NoSelection, Orientation, Picture, ScrolledWindow, Separator,
    SignalListItemFactory, ToggleButton, Widget,
};

use crate::message_html::MessageHtmlContext;
use crate::models::{SlackAttachment, SlackFile, SlackMessage};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TimelineMessageObject {
        pub message: RefCell<SlackMessage>,
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

mod wrap_box_imp {
    use super::*;
    use gtk::subclass::prelude::*;

    #[derive(Default)]
    pub struct ReactionWrapBox {
        pub children: RefCell<Vec<Widget>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ReactionWrapBox {
        const NAME: &'static str = "ConduitReactionWrapBox";
        type Type = super::ReactionWrapBox;
        type ParentType = Widget;
    }

    impl ObjectImpl for ReactionWrapBox {
        fn dispose(&self) {
            while let Some(child) = self.children.borrow_mut().pop() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for ReactionWrapBox {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let children = self.children.borrow();
            if children.is_empty() {
                return (0, 0, -1, -1);
            }

            let col_spacing = 4;
            let row_spacing = 4;

            match orientation {
                Orientation::Horizontal => {
                    let min_w = children
                        .iter()
                        .map(|c| c.measure(Orientation::Horizontal, -1).0)
                        .max()
                        .unwrap_or(0);
                    let sum_nat: i32 = children
                        .iter()
                        .map(|c| c.measure(Orientation::Horizontal, -1).1)
                        .sum();
                    let total_spacing = (children.len() as i32 - 1) * col_spacing;
                    let nat_w = sum_nat + total_spacing;
                    let nat = if for_size > 0 {
                        nat_w.min(for_size)
                    } else {
                        nat_w
                    };
                    (min_w, nat, -1, -1)
                }
                Orientation::Vertical => {
                    let mut x = 0;
                    let mut y = 0;
                    let mut line_h = 0;

                    for child in children.iter() {
                        let (_, child_w, _, _) = child.measure(Orientation::Horizontal, -1);
                        let (_, child_h, _, _) = child.measure(Orientation::Vertical, child_w);

                        if for_size > 0 && x + child_w > for_size && x > 0 {
                            x = 0;
                            y += line_h + row_spacing;
                            line_h = 0;
                        }
                        x += child_w + col_spacing;
                        line_h = line_h.max(child_h);
                    }
                    let total_h = y + line_h;

                    (total_h, total_h, -1, -1)
                }
                _ => (0, 0, -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, _height: i32, _baseline: i32) {
            let children = self.children.borrow();
            let col_spacing = 4;
            let row_spacing = 4;

            let mut x = 0;
            let mut y = 0;
            let mut line_h = 0;

            for child in children.iter() {
                let (_, child_w, _, _) = child.measure(Orientation::Horizontal, -1);
                let (_, child_h, _, _) = child.measure(Orientation::Vertical, child_w);

                if x + child_w > width && x > 0 {
                    x = 0;
                    y += line_h + row_spacing;
                    line_h = 0;
                }

                let transform = gtk::gsk::Transform::new()
                    .translate(&gtk::graphene::Point::new(x as f32, y as f32));
                child.allocate(child_w, child_h, -1, Some(transform));

                x += child_w + col_spacing;
                line_h = line_h.max(child_h);
            }
        }
    }
}

glib::wrapper! {
    pub struct ReactionWrapBox(ObjectSubclass<wrap_box_imp::ReactionWrapBox>)
        @extends Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ReactionWrapBox {
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    pub fn append(&self, child: &impl IsA<Widget>) {
        let child = child.as_ref();
        child.set_parent(self);
        self.imp().children.borrow_mut().push(child.clone());
        self.queue_resize();
    }
}

impl Default for ReactionWrapBox {
    fn default() -> Self {
        Self::new()
    }
}

impl TimelineMessageObject {
    pub fn new(message: SlackMessage) -> Self {
        let obj: Self = glib::Object::builder().build();
        *obj.imp().message.borrow_mut() = message;
        obj
    }

    pub fn message(&self) -> SlackMessage {
        self.imp().message.borrow().clone()
    }
}

pub(crate) type OpenMediaCallback = Rc<dyn Fn(crate::window::MediaGalleryItem)>;

type CacheEntry = (
    Option<gdk_pixbuf::PixbufAnimation>,
    gtk::gdk::Texture,
);

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
fn wrap_collapsible_media(media: Widget, ts: &str, slot: &str) -> Box {
    let container = Box::new(Orientation::Vertical, 2);
    container.set_halign(gtk::Align::Start);

    let collapsed = is_media_collapsed(ts, slot);

    let toggle = ToggleButton::new();
    toggle.set_focus_on_click(false);
    toggle.set_icon_name(if collapsed {
        "pan-end-symbolic"
    } else {
        "pan-down-symbolic"
    });
    toggle.add_css_class("flat");
    toggle.add_css_class("circular");
    toggle.set_halign(gtk::Align::Start);
    toggle.set_tooltip_text(Some("Show or hide image"));
    toggle.set_active(!collapsed);

    media.set_visible(!collapsed);

    let ts_owned = ts.to_string();
    let slot_owned = slot.to_string();
    let media_weak = media.downgrade();
    toggle.connect_toggled(move |btn| {
        let expanded = btn.is_active();
        btn.set_icon_name(if expanded {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        });
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
fn load_animated_or_static_picture(
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
            .reaction-pill { padding: 2px 6px; min-width: 0; min-height: 0; border-radius: 12px; }
            .reaction-pill label { min-width: 0; }
            .reaction-emoji-unicode { font-size: 32px; line-height: 1; }
            .reaction-emoji-image { margin-top: 3px; margin-bottom: 3px; }
            .timeline-attachment { border-left: 3px solid #e0e0e0; padding-left: 8px; margin-left: 4px; }
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
            "#,
        );
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

fn get_or_load_texture(path: &Path) -> Option<gtk::gdk::Texture> {
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
    ToggleReaction { ts: String, name: String, add: bool },
    ForwardMessage(String),
    MarkUnread(String),
    CopyMessageLink(String),
    CopyMessageText(String),
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
    on_action: Rc<RefCell<Option<Rc<dyn Fn(TimelineAction)>>>>,
    asset_update_pending: Rc<Cell<bool>>,
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
        let on_action: Rc<RefCell<Option<Rc<dyn Fn(TimelineAction)>>>> = Rc::new(RefCell::new(None));
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

            if let Some(ctx) = context_clone.borrow().as_ref() {
                let on_action = on_action_clone.clone();
                let cb_media: OpenMediaCallback = {
                    let on_action = on_action.clone();
                    Rc::new(move |media_item| {
                        let on_action = on_action.clone();
                        glib::idle_add_local_once(move || {
                            if let Some(h) = on_action.borrow().as_ref() {
                                h(TimelineAction::OpenMedia(media_item));
                            }
                        });
                    })
                };
                let cb_thread: Rc<dyn Fn(String)> = {
                    let on_action = on_action.clone();
                    Rc::new(move |thread_ts| {
                        let on_action = on_action.clone();
                        glib::idle_add_local_once(move || {
                            if let Some(h) = on_action.borrow().as_ref() {
                                h(TimelineAction::OpenThread(thread_ts));
                            }
                        });
                    })
                };
                let cb_reaction: Rc<dyn Fn(String, String, bool)> = {
                    let on_action = on_action.clone();
                    Rc::new(move |ts, name, add| {
                        let on_action = on_action.clone();
                        glib::idle_add_local_once(move || {
                            if let Some(h) = on_action.borrow().as_ref() {
                                h(TimelineAction::ToggleReaction { ts, name, add });
                            }
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
                                    &on_action_for_hover,
                                    &hover_generation,
                                    &hovered,
                                    &current_popover,
                                    &bar_motion,
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
            scrolled_window.vadjustment().connect_value_changed(move |_| {
                hover_generation.set(hover_generation.get().wrapping_add(1));
                *hovered.borrow_mut() = None;
                if let Some(bar) = quick_bar_weak.upgrade() {
                    bar.set_visible(false);
                }
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
        }
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
                if let Some(obj) = this.store.item(i).and_then(|o| o.downcast::<TimelineMessageObject>().ok()) {
                    new_items.push(TimelineMessageObject::new(obj.message()));
                }
            }
            if !new_items.is_empty() {
                this.store.splice(0, n_items, &new_items);
            }
        });
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
        self.store.remove_all();
        for msg in messages.iter().rev() {
            let obj = TimelineMessageObject::new(msg.clone());
            self.store.append(&obj);
        }

        let focus_index = focus_ts.and_then(|ts| {
            messages
                .iter()
                .rev()
                .position(|msg| msg.ts == ts)
                .map(|pos| pos as u32)
        });

        let list_view = self.list_view.clone();
        let vadj = self.scrolled_window.vadjustment();
        glib::idle_add_local_once(move || {
            if let Some(index) = focus_index {
                list_view.scroll_to(index, gtk::ListScrollFlags::empty(), None);
            } else {
                vadj.set_value(vadj.upper() - vadj.page_size());
            }
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
        let clean_key = cache_key.split('?').next().unwrap_or(cache_key).trim_matches('/');
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

fn render_text_content(
    text: &str,
    target_box: &Box,
    context: &MessageHtmlContext,
) {
    let segments = parse_text_segments(text);
    for seg in segments {
        match seg {
            TextSegment::CodeBlock { lang: _, code } => {
                let frame = Box::new(Orientation::Vertical, 0);
                frame.add_css_class("code-block");
                let label = Label::new(None);
                label.set_wrap(true);
                label.set_wrap_mode(pango::WrapMode::WordChar);
                label.set_selectable(true);
                label.set_focus_on_click(false);
                label.set_xalign(0.0);
                label.set_markup(&format!("<tt>{}</tt>", glib::markup_escape_text(&code)));
                frame.append(&label);
                target_box.append(&frame);
            }
            TextSegment::Quote(quote_text) => {
                register_timeline_css();
                let quote_box = Box::new(Orientation::Vertical, 0);
                quote_box.add_css_class("blockquote");
                let pango = crate::message_html::mrkdwn_to_pango(&quote_text, context);
                let label = Label::new(None);
                label.set_wrap(true);
                label.set_wrap_mode(pango::WrapMode::WordChar);
                label.set_selectable(true);
                label.set_focus_on_click(false);
                label.set_xalign(0.0);
                label.set_markup(&format!("<i>{}</i>", pango));
                quote_box.append(&label);
                target_box.append(&quote_box);
            }
            TextSegment::Normal(normal_text) => {
                if !normal_text.trim().is_empty() {
                    let pango = crate::message_html::mrkdwn_to_pango(&normal_text, context);
                    let label = Label::new(None);
                    label.set_wrap(true);
                    label.set_wrap_mode(pango::WrapMode::WordChar);
                    label.set_selectable(true);
                    label.set_focus_on_click(false);
                    label.set_xalign(0.0);
                    label.set_markup(&pango);
                    target_box.append(&label);
                }
            }
        }
    }
}

fn extract_block_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(text) = value.get("text") {
        if let Some(t) = text.as_str() {
            return Some(t.to_string());
        }
        if let Some(t) = text.get("text").and_then(|v| v.as_str()) {
            return Some(t.to_string());
        }
    }
    None
}

fn extract_rich_text_string(section_or_elem: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(elements) = section_or_elem.get("elements").and_then(|e| e.as_array()) {
        for sub in elements {
            let sub_type = sub.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match sub_type {
                "text" => {
                    if let Some(t) = sub.get("text").and_then(|v| v.as_str()) {
                        let style = sub.get("style");
                        let is_bold = style
                            .and_then(|s| s.get("bold"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let is_italic = style
                            .and_then(|s| s.get("italic"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let is_strike = style
                            .and_then(|s| s.get("strike"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let is_code = style
                            .and_then(|s| s.get("code"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);

                        let mut text = t.to_string();
                        if is_code {
                            text = format!("`{text}`");
                        }
                        if is_bold {
                            text = format!("*{text}*");
                        }
                        if is_italic {
                            text = format!("_{text}_");
                        }
                        if is_strike {
                            text = format!("~{text}~");
                        }
                        out.push_str(&text);
                    }
                }
                "link" => {
                    let url = sub.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    let text = sub.get("text").and_then(|v| v.as_str());
                    if let Some(t) = text {
                        out.push_str(&format!("<{url}|{t}>"));
                    } else {
                        out.push_str(&format!("<{url}>"));
                    }
                }
                "user" => {
                    if let Some(uid) = sub.get("user_id").and_then(|v| v.as_str()) {
                        out.push_str(&format!("<@{uid}>"));
                    }
                }
                "emoji" => {
                    if let Some(name) = sub.get("name").and_then(|v| v.as_str()) {
                        out.push_str(&format!(":{name}:"));
                    }
                }
                "channel" => {
                    if let Some(cid) = sub.get("channel_id").and_then(|v| v.as_str()) {
                        out.push_str(&format!("<#{cid}>"));
                    }
                }
                _ => {
                    if let Some(t) = sub.get("text").and_then(|v| v.as_str()) {
                        out.push_str(t);
                    }
                }
            }
        }
    } else if let Some(t) = extract_block_text(section_or_elem) {
        out.push_str(&t);
    }
    out
}

fn render_blocks(
    blocks: &[serde_json::Value],
    target_box: &Box,
    context: &MessageHtmlContext,
) {
    for block in blocks {
        let Some(kind) = block.get("type").and_then(|k| k.as_str()) else {
            continue;
        };

        match kind {
            "header" => {
                if let Some(text) = extract_block_text(block) {
                    if !text.trim().is_empty() {
                        let label = Label::new(None);
                        label.set_wrap(true);
                        label.set_wrap_mode(pango::WrapMode::WordChar);
                        label.set_selectable(true);
                        label.set_focus_on_click(false);
                        label.set_xalign(0.0);
                        label.set_markup(&format!(
                            "<span size=\"larger\" weight=\"bold\">{}</span>",
                            glib::markup_escape_text(&text)
                        ));
                        target_box.append(&label);
                    }
                }
            }
            "section" => {
                let section_box = Box::new(Orientation::Vertical, 4);
                if let Some(text) = extract_block_text(block) {
                    if !text.trim().is_empty() {
                        render_text_content(&text, &section_box, context);
                    }
                }
                if let Some(fields) = block.get("fields").and_then(|f| f.as_array()) {
                    if !fields.is_empty() {
                        let grid = Grid::new();
                        grid.set_column_spacing(12);
                        grid.set_row_spacing(4);
                        for (idx, field) in fields.iter().enumerate() {
                            if let Some(field_text) = extract_block_text(field) {
                                if !field_text.trim().is_empty() {
                                    let pango = crate::message_html::mrkdwn_to_pango(&field_text, context);
                                    let field_label = Label::new(None);
                                    field_label.set_wrap(true);
                                    field_label.set_wrap_mode(pango::WrapMode::WordChar);
                                    field_label.set_selectable(true);
                                    field_label.set_focus_on_click(false);
                                    field_label.set_xalign(0.0);
                                    field_label.set_markup(&pango);
                                    let col = (idx % 2) as i32;
                                    let row = (idx / 2) as i32;
                                    grid.attach(&field_label, col, row, 1, 1);
                                }
                            }
                        }
                        section_box.append(&grid);
                    }
                }
                target_box.append(&section_box);
            }
            "rich_text" => {
                if let Some(elements) = block.get("elements").and_then(|e| e.as_array()) {
                    for elem in elements {
                        let elem_type = elem.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match elem_type {
                            "rich_text_section" => {
                                let text = extract_rich_text_string(elem);
                                if !text.trim().is_empty() {
                                    render_text_content(&text, target_box, context);
                                }
                            }
                            "rich_text_list" => {
                                let style = elem
                                    .get("style")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("bullet");
                                let indent_level =
                                    elem.get("indent").and_then(|i| i.as_u64()).unwrap_or(0) as i32;
                                let list_box = Box::new(Orientation::Vertical, 2);
                                if indent_level > 0 {
                                    list_box.set_margin_start(indent_level * 16);
                                }
                                if let Some(items) = elem.get("elements").and_then(|e| e.as_array())
                                {
                                    for (idx, item) in items.iter().enumerate() {
                                        let prefix = if style == "ordered" {
                                            format!("{}. ", idx + 1)
                                        } else {
                                            "• ".to_string()
                                        };
                                        let item_text = extract_rich_text_string(item);
                                        let full_text = format!("{prefix}{item_text}");
                                        let pango =
                                            crate::message_html::mrkdwn_to_pango(&full_text, context);
                                        let label = Label::new(None);
                                        label.set_wrap(true);
                                        label.set_wrap_mode(pango::WrapMode::WordChar);
                                        label.set_selectable(true);
                                        label.set_focus_on_click(false);
                                        label.set_xalign(0.0);
                                        label.set_markup(&pango);
                                        list_box.append(&label);
                                    }
                                }
                                target_box.append(&list_box);
                            }
                            "rich_text_preformatted" => {
                                let code_text = extract_rich_text_string(elem);
                                let frame = Box::new(Orientation::Vertical, 0);
                                frame.add_css_class("code-block");
                                let label = Label::new(None);
                                label.set_wrap(true);
                                label.set_wrap_mode(pango::WrapMode::WordChar);
                                label.set_selectable(true);
                                label.set_focus_on_click(false);
                                label.set_xalign(0.0);
                                label.set_markup(&format!(
                                    "<tt>{}</tt>",
                                    glib::markup_escape_text(&code_text)
                                ));
                                frame.append(&label);
                                target_box.append(&frame);
                            }
                            "rich_text_quote" => {
                                register_timeline_css();
                                let quote_text = extract_rich_text_string(elem);
                                let quote_box = Box::new(Orientation::Vertical, 0);
                                quote_box.add_css_class("blockquote");
                                let pango =
                                    crate::message_html::mrkdwn_to_pango(&quote_text, context);
                                let label = Label::new(None);
                                label.set_wrap(true);
                                label.set_wrap_mode(pango::WrapMode::WordChar);
                                label.set_selectable(true);
                                label.set_focus_on_click(false);
                                label.set_xalign(0.0);
                                label.set_markup(&format!("<i>{}</i>", pango));
                                quote_box.append(&label);
                                target_box.append(&quote_box);
                            }
                            _ => {}
                        }
                    }
                }
            }
            "divider" => {
                let separator = Separator::new(Orientation::Horizontal);
                target_box.append(&separator);
            }
            "actions" => {
                let actions_box = Box::new(Orientation::Horizontal, 6);
                if let Some(elements) = block.get("elements").and_then(|e| e.as_array()) {
                    for elem in elements {
                        let btn_text =
                            extract_block_text(elem).unwrap_or_else(|| "Button".to_string());
                        let btn = Button::with_label(&btn_text);
                        btn.set_focus_on_click(false);
                        actions_box.append(&btn);
                    }
                }
                target_box.append(&actions_box);
            }
            "context" => {
                let context_box = Box::new(Orientation::Horizontal, 6);
                if let Some(elements) = block.get("elements").and_then(|e| e.as_array()) {
                    for elem in elements {
                        let elem_type = elem.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if elem_type == "image" || elem.get("image_url").is_some() {
                            let image_url = elem
                                .get("image_url")
                                .and_then(|u| u.as_str())
                                .or_else(|| elem.get("url").and_then(|u| u.as_str()));
                            let img_widget: Widget = if let Some(url) = image_url {
                                let local_path = resolve_cached_asset_path(url, context)
                                    .or_else(|| Path::new(url).exists().then(|| PathBuf::from(url)));
                                if let Some(path) = local_path {
                                    let pic = if let Some(tex) = get_or_load_texture(&path) {
                                        Picture::for_paintable(&tex)
                                    } else {
                                        Picture::for_filename(&path)
                                    };
                                    pic.set_size_request(16, 16);
                                    pic.set_content_fit(gtk::ContentFit::Cover);
                                    pic.upcast::<Widget>()
                                } else {
                                    let img = Image::from_icon_name("image-x-generic-symbolic");
                                    img.set_pixel_size(16);
                                    img.upcast::<Widget>()
                                }
                            } else {
                                let img = Image::from_icon_name("image-x-generic-symbolic");
                                img.set_pixel_size(16);
                                img.upcast::<Widget>()
                            };
                            context_box.append(&img_widget);
                        } else if let Some(text) = extract_block_text(elem) {
                            if !text.trim().is_empty() {
                                let pango = crate::message_html::mrkdwn_to_pango(&text, context);
                                let label = Label::new(None);
                                label.set_wrap(true);
                                label.set_wrap_mode(pango::WrapMode::WordChar);
                                label.set_selectable(true);
                                label.set_focus_on_click(false);
                                label.set_xalign(0.0);
                                label.add_css_class("dim-label");
                                label.set_markup(&format!("<span size=\"small\">{}</span>", pango));
                                context_box.append(&label);
                            }
                        }
                    }
                }
                target_box.append(&context_box);
            }
            _ => {}
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

            files_box.append(&wrap_collapsible_media(container.upcast::<Widget>(), ts, &format!("file:{index}")));
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
                let pic = load_animated_or_static_picture(&path, 400, 300, gtk::ContentFit::ScaleDown);
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

            files_box.append(&wrap_collapsible_media(container.upcast::<Widget>(), ts, &format!("file:{index}")));
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

fn render_attachments(
    attachments: &[SlackAttachment],
    root_box: &Box,
    context: &MessageHtmlContext,
    ts: &str,
) {
    register_timeline_css();
    for (index, attachment) in attachments.iter().enumerate() {
        if let Some(pretext) = attachment.pretext.as_deref().filter(|s| !s.trim().is_empty()) {
            let pango = crate::message_html::mrkdwn_to_pango(pretext, context);
            let label = Label::new(None);
            label.set_wrap(true);
            label.set_wrap_mode(pango::WrapMode::WordChar);
            label.set_selectable(true);
            label.set_focus_on_click(false);
            label.set_xalign(0.0);
            label.set_markup(&pango);
            root_box.append(&label);
        }

        let attach_box = Box::new(Orientation::Vertical, 4);
        attach_box.add_css_class("timeline-attachment");

        if let Some(title) = attachment.title.as_deref().filter(|s| !s.trim().is_empty()) {
            let label = Label::new(None);
            label.set_wrap(true);
            label.set_wrap_mode(pango::WrapMode::WordChar);
            label.set_selectable(true);
            label.set_focus_on_click(false);
            label.set_xalign(0.0);
            if let Some(title_link) = attachment
                .title_link
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                label.set_markup(&format!(
                    "<a href=\"{}\"><b>{}</b></a>",
                    glib::markup_escape_text(title_link),
                    glib::markup_escape_text(title)
                ));
            } else {
                label.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(title)));
            }
            attach_box.append(&label);
        }

        if let Some(text) = attachment.text.as_deref().filter(|s| !s.trim().is_empty()) {
            let pango = crate::message_html::mrkdwn_to_pango(text, context);
            let label = Label::new(None);
            label.set_wrap(true);
            label.set_wrap_mode(pango::WrapMode::WordChar);
            label.set_selectable(true);
            label.set_focus_on_click(false);
            label.set_xalign(0.0);
            label.set_markup(&pango);
            attach_box.append(&label);
        }

        if let Some(fields) = attachment.fields.as_deref().filter(|f| !f.is_empty()) {
            let grid = Grid::new();
            grid.set_column_spacing(12);
            grid.set_row_spacing(4);
            for (idx, field) in fields.iter().enumerate() {
                let title = field.title.as_deref().unwrap_or("");
                let val = field.value.as_deref().unwrap_or("");
                let markup = match (!title.is_empty(), !val.is_empty()) {
                    (true, true) => format!(
                        "<b>{}</b>\n{}",
                        glib::markup_escape_text(title),
                        crate::message_html::mrkdwn_to_pango(val, context)
                    ),
                    (true, false) => format!("<b>{}</b>", glib::markup_escape_text(title)),
                    (false, true) => crate::message_html::mrkdwn_to_pango(val, context),
                    (false, false) => String::new(),
                };
                if !markup.is_empty() {
                    let field_label = Label::new(None);
                    field_label.set_wrap(true);
                    field_label.set_wrap_mode(pango::WrapMode::WordChar);
                    field_label.set_selectable(true);
                    field_label.set_focus_on_click(false);
                    field_label.set_xalign(0.0);
                    field_label.set_markup(&markup);
                    let col = (idx % 2) as i32;
                    let row = (idx / 2) as i32;
                    grid.attach(&field_label, col, row, 1, 1);
                }
            }
            attach_box.append(&grid);
        }

        if let Some(image_url) = attachment
            .image_url
            .as_deref()
            .or(attachment.thumb_url.as_deref())
            .filter(|s| !s.trim().is_empty())
        {
            let local_path = resolve_cached_asset_path(image_url, context).or_else(|| {
                std::path::Path::new(image_url)
                    .exists()
                    .then(|| std::path::PathBuf::from(image_url))
            });

            if let Some(path) = local_path {
                let pic = load_animated_or_static_picture(&path, 400, 300, gtk::ContentFit::ScaleDown);
                pic.add_css_class("rounded");
                let title = attachment
                    .title
                    .as_deref()
                    .or(attachment.fallback.as_deref())
                    .unwrap_or("");
                if !title.is_empty() {
                    pic.set_tooltip_text(Some(title));
                }
                attach_box.append(&wrap_collapsible_media(
                    pic,
                    ts,
                    &format!("attachment:{index}"),
                ));
            }
        }

        if let Some(attach_blocks) = attachment
            .blocks
            .as_ref()
            .and_then(|b| b.as_array())
            .filter(|b| !b.is_empty())
        {
            render_blocks(attach_blocks, &attach_box, context);
        }

        root_box.append(&attach_box);
    }
}

fn build_avatar_widget(message: &SlackMessage, context: &MessageHtmlContext) -> Widget {
    let user_avatar_url = message
        .author_user_id()
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
                let delay_ms = iter.delay_time().map(|d| d.as_millis() as u64).unwrap_or(60).max(60);

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
                if let Some(scaled) = pixbuf.scale_simple(32, 32, gdk_pixbuf::InterpType::Bilinear) {
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
            } else if clean_name.chars().any(|c| !c.is_ascii()) {
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

fn rebuild_quick_bar(
    quick_bar: &Box,
    ts: &str,
    reactions: &[crate::emoji::EmojiEntry],
    context: &MessageHtmlContext,
    on_action: &Rc<RefCell<Option<Rc<dyn Fn(TimelineAction)>>>>,
    hover_generation: &Rc<Cell<u64>>,
    hovered: &Rc<RefCell<Option<HoveredMessage>>>,
    current_popover: &Rc<RefCell<Option<gtk::Popover>>>,
    bar_motion: &gtk::EventControllerMotion,
) {
    while let Some(child) = quick_bar.first_child() {
        quick_bar.remove(&child);
    }

    let emoji_catalog = crate::emoji::EmojiCatalog::new(&context.custom_emojis);

    for entry in reactions {
        let btn = Button::new();
        btn.set_focus_on_click(false);
        let widget = resolve_reaction_emoji_widget(&entry.name, &entry.name, context, &emoji_catalog);
        btn.set_child(Some(&widget));
        btn.add_css_class("flat");
        btn.add_css_class("circular");
        btn.set_tooltip_text(Some(&format!(":{}:", entry.name)));
        let ts = ts.to_string();
        let name = entry.name.clone();
        let on_action = on_action.clone();
        btn.connect_clicked(move |_| {
            if let Some(h) = on_action.borrow().as_ref() {
                h(TimelineAction::ToggleReaction {
                    ts: ts.clone(),
                    name: name.clone(),
                    add: true,
                });
            }
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
                        if let Some(h) = on_action.borrow().as_ref() {
                            h(TimelineAction::ToggleReaction {
                                ts: ts.clone(),
                                name: clean.to_string(),
                                add: true,
                            });
                        }
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
            if let Some(h) = on_action.borrow().as_ref() {
                h(TimelineAction::OpenThread(ts.clone()));
            }
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
            if let Some(h) = on_action.borrow().as_ref() {
                h(TimelineAction::ForwardMessage(ts.clone()));
            }
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
            if let Some(h) = on_action.borrow().as_ref() {
                h(TimelineAction::MarkUnread(ts.clone()));
            }
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
            if let Some(h) = on_action.borrow().as_ref() {
                h(TimelineAction::CopyMessageLink(ts.clone()));
            }
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
            if let Some(h) = on_action.borrow().as_ref() {
                h(TimelineAction::CopyMessageText(ts.clone()));
            }
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

pub(crate) fn build_timeline_message_widget(
    message: &SlackMessage,
    context: &MessageHtmlContext,
    on_open_media: Option<&OpenMediaCallback>,
    on_open_thread: Option<&Rc<dyn Fn(String)>>,
    on_toggle_reaction: Option<&Rc<dyn Fn(String, String, bool)>>,
    on_hover: Option<&Rc<dyn Fn(HoverEvent)>>,
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

            let sys_text = message.text.as_deref().unwrap_or_else(|| match subtype {
                "channel_join" => "joined the channel",
                "channel_leave" => "left the channel",
                "channel_topic" => "set the channel topic",
                "channel_purpose" => "set the channel purpose",
                _ => "system event",
            });

            let pango = crate::message_html::mrkdwn_to_pango(sys_text, context);
            let label = Label::new(None);
            label.set_wrap(true);
            label.set_wrap_mode(pango::WrapMode::WordChar);
            label.set_selectable(true);
            label.set_focus_on_click(false);
            label.set_xalign(0.0);
            label.add_css_class("dim-label");
            label.set_markup(&format!("<i>{}</i>", pango));
            root_box.append(&label);

            return root_box;
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
    header_box.append(&avatar_widget);

    let author_name = message
        .author_user_id()
        .and_then(|user_id| {
            context
                .user_full_names
                .get(user_id)
                .cloned()
                .or_else(|| context.user_names.get(user_id).cloned())
        })
        .unwrap_or_else(|| message.author_label());

    let author_label = Label::new(None);
    author_label.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(&author_name)));
    author_label.set_xalign(0.0);
    header_box.append(&author_label);

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

    // Content: Blocks or Text
    let has_rendered_blocks = if let Some(blocks) = message
        .blocks
        .as_ref()
        .and_then(|b| b.as_array())
        .filter(|b| !b.is_empty())
    {
        render_blocks(blocks, &root_box, context);
        true
    } else {
        false
    };

    if !has_rendered_blocks {
        let content_text = message.text.as_deref().unwrap_or("");
        if !content_text.is_empty() {
            render_text_content(content_text, &root_box, context);
        }
    }

    // Files
    if let Some(files) = message.files.as_deref().filter(|f| !f.is_empty()) {
        render_files(files, &root_box, context, on_open_media, &message.ts);
    }

    // Attachments
    if let Some(attachments) = message
        .attachments
        .as_deref()
        .filter(|a| !a.is_empty())
    {
        render_attachments(attachments, &root_box, context, &message.ts);
    }

    // Reactions
    if let Some(reactions) = message.reactions.as_deref().filter(|r| !r.is_empty()) {
        register_timeline_css();
        let wrap_box = ReactionWrapBox::new();

        let emoji_catalog = crate::emoji::EmojiCatalog::new(&context.custom_emojis);

        for r in reactions {
            let raw_name = r.name.as_deref().unwrap_or("reaction");
            let count = r.count.unwrap_or(1);
            let clean_name = raw_name.trim_matches(':');

            let emoji_widget = resolve_reaction_emoji_widget(clean_name, raw_name, context, &emoji_catalog);

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
                    .filter_map(|user_id| {
                        context
                            .user_names
                            .get(user_id)
                            .map(|s| s.as_str())
                    })
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
                let Some(btn) = weak_add_btn.upgrade() else { return; };
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
        let replied = message
            .reply_users
            .as_deref()
            .is_some_and(|users| context.current_user_id.as_deref().is_some_and(|me| users.iter().any(|u| u == me)));
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

    root_box
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::message_html::MessageHtmlContext;
    use crate::models::{SlackAttachmentField, SlackFile, SlackMessage, SlackReaction};

    fn test_context() -> MessageHtmlContext {
        MessageHtmlContext {
            user_names: Arc::new(HashMap::from([("U123".to_string(), "Alice".to_string())])),
            user_full_names: Arc::default(),
            user_avatar_urls: Arc::default(),
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
            message_control_handles: HashMap::default(),
            message_control_action_handles: HashMap::default(),
        }
    }

    fn ensure_gtk() -> bool {
        if gtk::is_initialized() {
            return true;
        }
        std::panic::catch_unwind(|| gtk::init()).ok().and_then(|r| r.ok()).is_some()
    }

    fn run_gtk_test<F: FnOnce()>(f: F) {
        if !ensure_gtk() {
            return;
        }
        f();
    }

    #[test]
    fn rich_text_channel_element_renders_as_pill_instead_of_vanishing() {
        let elem = serde_json::json!({
            "type": "rich_text_section",
            "elements": [{"type": "channel", "channel_id": "C123"}]
        });
        let token = extract_rich_text_string(&elem);
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
        assert_eq!(a, b, "querying the same ts/slot twice must yield the same key");

        let c = media_collapse_key("1710000000.000100", "attachment:0");
        assert_ne!(
            a, c,
            "a file and an attachment at the same index must not collide"
        );
    }

    #[test]
    fn giphy_attachment_image_renders_instead_of_nothing() {
        if !ensure_gtk() {
            return;
        }

        let cache_dir = crate::config::image_asset_cache_dir();
        let ws_dir = cache_dir.join("test_ws_giphy");
        let _ = std::fs::create_dir_all(&ws_dir);

        let test_url = "https://media.giphy.com/media/test123/giphy.gif";
        let hash = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update("test_ws_giphy".as_bytes());
            hasher.update([0]);
            hasher.update(test_url.as_bytes());
            format!("{:x}", hasher.finalize())
        };
        let dummy_file = ws_dir.join(format!("{hash}.png"));
        // Not a real image - this only exercises the "fails to parse as an
        // animation" fallback to a static texture/picture, which is enough to
        // prove the attachment renders an image widget instead of nothing.
        std::fs::write(&dummy_file, b"not a real image").unwrap();

        let ctx = test_context();
        let root_box = Box::new(Orientation::Vertical, 0);
        let attachment = SlackAttachment {
            image_url: Some(test_url.to_string()),
            is_animated: Some(true),
            ..Default::default()
        };
        render_attachments(&[attachment], &root_box, &ctx, "1710000000.000100");

        let attach_box = root_box
            .first_child()
            .and_then(|w| w.downcast::<Box>().ok())
            .expect("attach_box should be appended to root_box");
        let media_wrapper = attach_box
            .first_child()
            .and_then(|w| w.downcast::<Box>().ok())
            .expect("collapsible media wrapper should be the only attach_box child");
        let toggle = media_wrapper
            .first_child()
            .and_then(|w| w.downcast::<ToggleButton>().ok());
        assert!(
            toggle.is_some(),
            "expected a collapse/expand toggle button, i.e. an image widget was built"
        );

        let _ = std::fs::remove_file(dummy_file);
        let _ = std::fs::remove_dir(ws_dir);
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

    #[test]
    fn test_timeline_message_widget_all_gtk() {
        if !ensure_gtk() {
            return;
        }

        let ctx = test_context();

        // 1. Basic message
        let mut msg1 = SlackMessage::default();
        msg1.ts = "1700000000.000100".to_string();
        msg1.user = Some("U123".to_string());
        msg1.text = Some("Hello *world*!".to_string());
        msg1.reactions = Some(vec![
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
        ]);
        msg1.reply_count = Some(5);

        let widget1 = build_timeline_message_widget(&msg1, &ctx, None, None, None, None);
        assert_eq!(widget1.orientation(), Orientation::Vertical);

        // 2. Section blocks and fields
        let mut msg2 = SlackMessage::default();
        msg2.ts = "1700000000.000200".to_string();
        msg2.user = Some("U123".to_string());
        msg2.blocks = Some(serde_json::json!([
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
        ]));

        let widget2 = build_timeline_message_widget(&msg2, &ctx, None, None, None, None);
        assert_eq!(widget2.orientation(), Orientation::Vertical);

        // 3. Divider, actions and context
        let mut msg3 = SlackMessage::default();
        msg3.ts = "1700000000.000300".to_string();
        msg3.user = Some("U123".to_string());
        msg3.blocks = Some(serde_json::json!([
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
        ]));

        let widget3 = build_timeline_message_widget(&msg3, &ctx, None, None, None, None);
        assert_eq!(widget3.orientation(), Orientation::Vertical);

        // 4. Attachments with color border
        let mut msg4 = SlackMessage::default();
        msg4.ts = "1700000000.000400".to_string();
        msg4.user = Some("U123".to_string());
        msg4.attachments = Some(vec![SlackAttachment {
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
        }]);

        let widget4 = build_timeline_message_widget(&msg4, &ctx, None, None, None, None);
        assert_eq!(widget4.orientation(), Orientation::Vertical);

        // 5. Image file
        let mut msg_img = SlackMessage::default();
        msg_img.ts = "1700000000.000500".to_string();
        msg_img.user = Some("U123".to_string());
        msg_img.files = Some(vec![SlackFile {
            id: Some("F1".to_string()),
            name: Some("photo.png".to_string()),
            title: Some("Sample Photo".to_string()),
            mimetype: Some("image/png".to_string()),
            thumb_360: Some("https://example.com/thumb.png".to_string()),
            ..Default::default()
        }]);

        let widget_img = build_timeline_message_widget(&msg_img, &ctx, None, None, None, None);
        assert_eq!(widget_img.orientation(), Orientation::Vertical);

        // 6. Video file
        let mut msg_vid = SlackMessage::default();
        msg_vid.ts = "1700000000.000600".to_string();
        msg_vid.user = Some("U123".to_string());
        msg_vid.files = Some(vec![SlackFile {
            id: Some("F2".to_string()),
            name: Some("demo.mp4".to_string()),
            title: Some("Demo Recording".to_string()),
            mimetype: Some("video/mp4".to_string()),
            thumb_video: Some("https://example.com/video_thumb.png".to_string()),
            ..Default::default()
        }]);

        let widget_vid = build_timeline_message_widget(&msg_vid, &ctx, None, None, None, None);
        assert_eq!(widget_vid.orientation(), Orientation::Vertical);

        // 7. Document file with size
        let mut msg_doc = SlackMessage::default();
        msg_doc.ts = "1700000000.000700".to_string();
        msg_doc.user = Some("U123".to_string());
        msg_doc.files = Some(vec![SlackFile {
            id: Some("F3".to_string()),
            name: Some("report.pdf".to_string()),
            title: Some("Annual Report".to_string()),
            mimetype: Some("application/pdf".to_string()),
            size: Some(2097152),
            ..Default::default()
        }]);

        let widget_doc = build_timeline_message_widget(&msg_doc, &ctx, None, None, None, None);
        assert_eq!(widget_doc.orientation(), Orientation::Vertical);

        // 8. Rich text blocks
        let mut msg_rich = SlackMessage::default();
        msg_rich.ts = "1700000000.000800".to_string();
        msg_rich.user = Some("U123".to_string());
        msg_rich.blocks = Some(serde_json::json!([
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
        ]));

        let widget_rich = build_timeline_message_widget(&msg_rich, &ctx, None, None, None, None);
        assert_eq!(widget_rich.orientation(), Orientation::Vertical);

        // 9. Subtype system message
        let mut msg_sys = SlackMessage::default();
        msg_sys.ts = "1700000000.000900".to_string();
        msg_sys.user = Some("U123".to_string());
        msg_sys.subtype = Some("channel_join".to_string());
        msg_sys.text = Some("joined the channel".to_string());

        let widget_sys = build_timeline_message_widget(&msg_sys, &ctx, None, None, None, None);
        assert_eq!(widget_sys.orientation(), Orientation::Horizontal);

        // 10. Thread broadcast banner
        let mut msg_bc = SlackMessage::default();
        msg_bc.ts = "1700000000.0001000".to_string();
        msg_bc.user = Some("U123".to_string());
        msg_bc.text = Some("Broadcast reply".to_string());
        msg_bc.is_thread_broadcast = Some(true);

        let widget_bc = build_timeline_message_widget(&msg_bc, &ctx, None, None, None, None);
        assert_eq!(widget_bc.orientation(), Orientation::Vertical);

        // 11. Native timeline view & update_image_asset
        let timeline_view = NativeTimelineView::new();
        timeline_view.set_messages(&[msg1, msg2], &ctx, None);
        assert_eq!(timeline_view.store.n_items(), 2);
        timeline_view.update_image_asset(&ctx);
        assert_eq!(timeline_view.store.n_items(), 2);

        // 12. Custom emoji reactions & resolve_cached_asset_path
        let mut ctx_emoji = ctx.clone();
        std::sync::Arc::make_mut(&mut ctx_emoji.custom_emojis)
            .insert("party_blob".to_string(), "https://example.com/blob.gif".to_string());
        let mut msg_reaction = SlackMessage::default();
        msg_reaction.ts = "1700000000.001100".to_string();
        msg_reaction.user = Some("U123".to_string());
        msg_reaction.text = Some("Reaction test".to_string());
        msg_reaction.reactions = Some(vec![
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
        ]);
        let widget_rx = build_timeline_message_widget(&msg_reaction, &ctx_emoji, None, None, None, None);
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
    }

    #[test]
    fn test_reaction_wrap_box() {
        if !ensure_gtk() {
            return;
        }

        let wrap_box = ReactionWrapBox::new();

        let btn1 = Button::with_label("👍 1");
        let btn2 = Button::with_label("❤️ 2");
        wrap_box.append(&btn1);
        wrap_box.append(&btn2);

        let (min_w, nat_w, _, _) = wrap_box.measure(Orientation::Horizontal, -1);
        assert!(min_w >= 0);
        assert!(nat_w >= min_w);

        let (min_h, nat_h, _, _) = wrap_box.measure(Orientation::Vertical, 100);
        assert!(min_h >= 0);
        assert_eq!(min_h, nat_h);

        wrap_box.allocate(200, 100, -1, None);
    }
}
