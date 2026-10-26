use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::{
    gio, glib, Box, Label, ListView, NoSelection, Orientation, ScrolledWindow,
    SignalListItemFactory, Widget,
};

use crate::message_markup::{MessageRenderContext, TimelineScrollBehavior};
use crate::models::SlackMessage;

use super::actions::*;
use super::dwell::*;
use super::model::*;
use super::quick_actions::*;
use super::row::*;

thread_local! {
    /// Row binds and their total cost since the last idle flush, plus whether
    /// that flush is scheduled (debug only).
    static BIND_STATS: Cell<(u32, f64, bool)> = const { Cell::new((0, 0.0, false)) };
}

/// Times one row bind and folds it into [`BIND_STATS`] on drop, so every exit
/// path of the bind closure is counted. Logged once per main-loop idle as
/// `timeline_binds count=<n> elapsed_ms=<total>`.
struct BindTimer(Option<Instant>);

impl BindTimer {
    fn start() -> Self {
        Self(crate::debug::enabled().then(Instant::now))
    }
}

impl Drop for BindTimer {
    fn drop(&mut self) {
        let Some(started) = self.0 else {
            return;
        };
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        let (count, total, scheduled) = BIND_STATS.get();
        BIND_STATS.set((count + 1, total + elapsed_ms, true));
        if !scheduled {
            glib::idle_add_local_once(|| {
                let (count, total, _) = BIND_STATS.replace((0, 0.0, false));
                crate::debug::log(
                    "performance",
                    &format!("timeline_binds count={count} elapsed_ms={total:.2}"),
                );
            });
        }
    }
}

#[derive(Clone)]
pub struct NativeTimelineView {
    container: Box,
    pub(crate) scrolled_window: ScrolledWindow,
    pub(crate) list_view: ListView,
    placeholder_label: Label,
    pub store: gio::ListStore,
    pub(crate) context: Rc<RefCell<Option<MessageRenderContext>>>,
    on_action: ActionSlot,
    asset_update_pending: Rc<Cell<bool>>,
    /// Timestamps of rows waiting for the queued image re-render.
    asset_update_rows: Rc<RefCell<HashSet<String>>>,
    read_mark_target: ReadMarkTarget,
    latest_message_ts: Rc<RefCell<Option<String>>>,
    read_candidate: Rc<RefCell<Option<String>>>,
    read_generation: Rc<Cell<u64>>,
    /// True while a dwell timer for `read_candidate` is in flight, so a
    /// recheck keeps that dwell instead of restarting it.
    dwell_pending: Rc<Cell<bool>>,
    recheck_read_visibility: Rc<dyn Fn()>,
    /// Reads only count while the user can see them: auto-marking pauses
    /// while the window is unfocused and re-evaluates on refocus.
    window_active: Rc<Cell<bool>>,
    /// False inside the thread pane: the root message is already the open
    /// thread, so it gets no "N replies" footer linking to itself.
    opens_threads: Rc<Cell<bool>>,
    bind_row: Rc<dyn Fn(&gtk::ListItem)>,
    pub(crate) bound_items: BoundItems,
    /// Message opened from search, a link or a notification: highlighted
    /// briefly so it stands out after the jump.
    highlight_ts: Rc<RefCell<Option<String>>>,
    highlight_generation: Rc<Cell<u64>>,
    /// Older-history paging: an older page is in flight (spinner shown).
    loading_older: Rc<Cell<bool>>,
    loading_older_indicator: Widget,
    /// Backoff after a failed older-page load, so sitting near the top
    /// while offline or rate-limited does not re-request in a tight loop.
    older_backoff: Rc<OlderPageBackoff>,
    /// True between `set_messages` and the settled layout, so programmatic
    /// scrolling during a render does not count as reaching the top.
    settling: Rc<Cell<bool>>,
    settle_generation: Rc<Cell<u64>>,
    /// Set when the shown conversation/thread changes, so the next render
    /// lands on the newest message instead of preserving a stale position.
    surface_changed: Rc<Cell<bool>>,
    check_load_older: Rc<dyn Fn()>,
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
        let context: Rc<RefCell<Option<MessageRenderContext>>> = Rc::new(RefCell::new(None));
        let on_action: ActionSlot = Rc::new(RefCell::new(None));
        let hovered: Rc<RefCell<Option<HoveredMessage>>> = Rc::new(RefCell::new(None));
        let hover_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        let read_mark_target: ReadMarkTarget = Rc::new(RefCell::new(None));
        // Holds the quick-bar's current overflow popover (rebuilt fresh on
        // every hover). The hover-leave debounce below queries its live
        // `is_visible()` directly instead of a manually-toggled flag, so it
        // can never go stale relative to whatever GTK's show/closed signals
        // actually do or when they fire.
        let current_popover: Rc<RefCell<Option<gtk::Popover>>> = Rc::new(RefCell::new(None));
        let opens_threads = Rc::new(Cell::new(true));

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
        let opens_threads_for_bind = opens_threads.clone();
        let highlight_ts: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let highlight_ts_for_bind = highlight_ts.clone();
        let hovered_for_bind = hovered.clone();
        let hover_generation_for_bind = hover_generation.clone();
        let current_popover_for_bind = current_popover.clone();
        let bar_motion_for_bind = bar_motion.clone();
        let quick_bar_for_bind = quick_bar.clone();
        let overlay_for_bind = overlay.clone();
        let read_mark_target_for_bind = read_mark_target.clone();
        let bind_row: Rc<dyn Fn(&gtk::ListItem)> = Rc::new(move |item| {
            let _bind_timer = BindTimer::start();
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
                    let read_mark_target = read_mark_target_for_bind.clone();
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
                            *hovered.borrow_mut() = Some(HoveredMessage {
                                row,
                                ts: ts.clone(),
                            });
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
                                        // Thread read state is local-only,
                                        // so only conversations rewind.
                                        can_mark_unread: read_mark_target
                                            .borrow()
                                            .as_ref()
                                            .is_some_and(|(_, thread_ts)| thread_ts.is_none()),
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
                let show_header = !msg_obj.hide_header();
                let msg_widget = build_timeline_message_widget(
                    &msg,
                    ctx,
                    Some(&cb_media),
                    opens_threads_for_bind.get().then_some(&cb_thread),
                    Some(&cb_reaction),
                    Some(&cb_hover),
                    Some(&cb_author),
                    show_header,
                );
                if highlight_ts_for_bind.borrow().as_deref() == Some(msg.ts.as_str()) {
                    msg_widget.add_css_class(FOCUS_TARGET_CLASS);
                }
                item.set_child(Some(&msg_widget));
            }
        });

        // Rows currently bound to a widget, so in-place updates (image assets,
        // unread separator) can rebuild them without replacing store items.
        let bound_items: BoundItems = Rc::new(RefCell::new(Vec::new()));
        {
            let bind_row = bind_row.clone();
            let bound_items = bound_items.clone();
            factory.connect_bind(move |_factory, list_item| {
                let item = list_item
                    .downcast_ref::<gtk::ListItem>()
                    .expect("ListItem expected");
                bound_items.borrow_mut().push(item.downgrade());
                bind_row(item);
            });
        }
        {
            let bound_items = bound_items.clone();
            factory.connect_unbind(move |_factory, list_item| {
                bound_items
                    .borrow_mut()
                    .retain(|weak| weak.upgrade().is_some_and(|bound| bound != *list_item));
            });
        }

        let list_view = ListView::new(Some(selection), Some(factory));
        // Tab into the list lands on the remembered row, not on the oldest message.
        list_view.set_tab_behavior(gtk::ListTabBehavior::Item);
        list_view.add_css_class("conduit-timeline");

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

        {
            let scrolled_window_weak = scrolled_window.downgrade();
            let bar_scroll = gtk::EventControllerScroll::new(
                gtk::EventControllerScrollFlags::BOTH_AXES
                    | gtk::EventControllerScrollFlags::DISCRETE
                    | gtk::EventControllerScrollFlags::KINETIC,
            );
            bar_scroll.connect_scroll(move |controller, dx, dy| {
                let Some(scroller) = scrolled_window_weak.upgrade() else {
                    return glib::Propagation::Proceed;
                };

                let unit = controller.unit();
                let vadj = scroller.vadjustment();
                let hadj = scroller.hadjustment();

                let step_v = if unit == gtk::gdk::ScrollUnit::Wheel {
                    vadj.step_increment().max(20.0)
                } else {
                    1.0
                };
                let step_h = if unit == gtk::gdk::ScrollUnit::Wheel {
                    hadj.step_increment().max(20.0)
                } else {
                    1.0
                };

                if dy != 0.0 {
                    let new_v = (vadj.value() + dy * step_v).clamp(
                        vadj.lower(),
                        (vadj.upper() - vadj.page_size()).max(vadj.lower()),
                    );
                    vadj.set_value(new_v);
                }

                if dx != 0.0 {
                    let new_h = (hadj.value() + dx * step_h).clamp(
                        hadj.lower(),
                        (hadj.upper() - hadj.page_size()).max(hadj.lower()),
                    );
                    hadj.set_value(new_h);
                }

                glib::Propagation::Stop
            });
            quick_bar.add_controller(bar_scroll);
        }

        // --- Visibility-based automatic read-marking ---
        // Purely event-driven: re-evaluated on scroll, on viewport resize,
        // and once after new content loads - never on a per-frame tick
        // callback, which would keep the compositor/display pipeline from
        // idling for a check that's a no-op nearly all the time.
        let latest_message_ts: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let read_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
        let read_candidate: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let dwell_pending = Rc::new(Cell::new(false));
        let window_active = Rc::new(Cell::new(true));

        let recheck_read_visibility: Rc<dyn Fn()> = {
            let list_view = list_view.clone();
            let scrolled_window = scrolled_window.clone();
            let read_mark_target = read_mark_target.clone();
            let latest_message_ts = latest_message_ts.clone();
            let read_generation = read_generation.clone();
            let read_candidate = read_candidate.clone();
            let dwell_pending = dwell_pending.clone();
            let window_active = window_active.clone();
            let on_action = on_action.clone();
            Rc::new(move || {
                // A hidden stack page keeps its stale allocation, so the
                // height alone does not prove the rows are on screen.
                let viewport_height = f64::from(scrolled_window.height());
                if viewport_height <= 0.0 || !window_active.get() || !scrolled_window.is_drawable()
                {
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
                dwell_pending.set(false);
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
                let read_candidate = read_candidate.clone();
                let dwell_pending = dwell_pending.clone();
                let scrolled_window = scrolled_window.downgrade();
                let on_action = on_action.clone();
                dwell_pending.set(true);
                glib::timeout_add_local_once(READ_DWELL, move || {
                    if read_generation.get() != expected {
                        return;
                    }
                    dwell_pending.set(false);
                    // Hidden mid-dwell: forget the candidate so the rows
                    // dwell afresh once they are shown again.
                    if !scrolled_window
                        .upgrade()
                        .is_some_and(|scrolled_window| scrolled_window.is_drawable())
                    {
                        *read_candidate.borrow_mut() = None;
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

        // --- Older-history paging ---
        // Near the top of the loaded messages, ask the window for the
        // previous page; GtkListView keeps its anchor row in place when the
        // page is prepended, so the reading position survives.
        let loading_older = Rc::new(Cell::new(false));
        let settling = Rc::new(Cell::new(false));
        let older_backoff = Rc::new(OlderPageBackoff::default());
        let check_load_older: Rc<dyn Fn()> = {
            let scrolled_window = scrolled_window.downgrade();
            let read_mark_target = read_mark_target.clone();
            let loading_older = loading_older.clone();
            let settling = settling.clone();
            let older_backoff = older_backoff.clone();
            let on_action = on_action.clone();
            let queued = Rc::new(Cell::new(false));
            Rc::new(move || {
                if settling.get()
                    || loading_older.get()
                    || older_backoff.cooling_down(std::time::Instant::now())
                {
                    return;
                }
                let Some(scrolled_window) = scrolled_window.upgrade() else {
                    return;
                };
                let vadj = scrolled_window.vadjustment();
                if !scrolled_window.is_drawable()
                    || !near_oldest_edge(vadj.value() - vadj.lower(), vadj.page_size())
                {
                    return;
                }
                let Some((channel_id, thread_ts)) = read_mark_target.borrow().clone() else {
                    return;
                };
                // Scroll events arrive in bursts: keep one request queued.
                if queued.replace(true) {
                    return;
                }
                let on_action = on_action.clone();
                let queued = queued.clone();
                glib::idle_add_local_once(move || {
                    queued.set(false);
                    dispatch_timeline_action(
                        &on_action,
                        TimelineAction::LoadOlder {
                            channel_id,
                            thread_ts,
                        },
                    );
                });
            })
        };
        {
            let check_load_older = check_load_older.clone();
            scrolled_window
                .vadjustment()
                .connect_value_changed(move |_| check_load_older());
        }

        overlay.set_child(Some(&scrolled_window));

        let loading_older_indicator = Box::new(Orientation::Horizontal, 0);
        loading_older_indicator.add_css_class("osd");
        loading_older_indicator.add_css_class("timeline-loading-older");
        loading_older_indicator.set_halign(gtk::Align::Center);
        loading_older_indicator.set_valign(gtk::Align::Start);
        loading_older_indicator.set_margin_top(12);
        loading_older_indicator.set_can_target(false);
        loading_older_indicator.set_visible(false);
        loading_older_indicator.update_property(&[gtk::accessible::Property::Label(
            &gettextrs::gettext("Loading older messages"),
        )]);
        let spinner = adw::Spinner::new();
        spinner.set_size_request(20, 20);
        loading_older_indicator.append(&spinner);
        overlay.add_overlay(&loading_older_indicator);

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
                let y = bounds.y().max(0.0);
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

        let view = Self {
            container,
            scrolled_window,
            list_view,
            placeholder_label,
            store,
            context,
            on_action,
            asset_update_pending: Rc::new(Cell::new(false)),
            asset_update_rows: Rc::default(),
            read_mark_target,
            latest_message_ts,
            read_candidate,
            read_generation,
            dwell_pending,
            recheck_read_visibility,
            window_active,
            opens_threads,
            bind_row,
            bound_items,
            highlight_ts,
            highlight_generation: Rc::new(Cell::new(0)),
            loading_older,
            loading_older_indicator: loading_older_indicator.upcast(),
            older_backoff,
            settling,
            settle_generation: Rc::new(Cell::new(0)),
            surface_changed: Rc::new(Cell::new(false)),
            check_load_older,
        };
        schedule_day_rollover_rebind(
            view.container.downgrade(),
            view.bound_items.clone(),
            view.bind_row.clone(),
        );
        view
    }

    /// Ends the older-page load of `channel_id`/`thread_ts` if this view
    /// still shows that surface; a late reply for another one is ignored.
    pub(crate) fn finish_loading_older(&self, channel_id: &str, thread_ts: Option<&str>) {
        if self.shows_surface(channel_id, thread_ts) {
            self.older_backoff.reset();
            self.set_loading_older(false);
        }
    }

    /// Ends a failed older-page load of `channel_id`/`thread_ts` and holds
    /// off the next one with a growing delay, re-checking once it elapses.
    /// A failure while no older page is in flight (an initial load) leaves
    /// paging alone.
    pub(crate) fn fail_loading_older(&self, channel_id: &str, thread_ts: Option<&str>) {
        if !self.shows_surface(channel_id, thread_ts) || !self.loading_older.get() {
            return;
        }
        let delay = self.older_backoff.fail(std::time::Instant::now());
        self.set_loading_older(false);
        let check_load_older = self.check_load_older.clone();
        glib::timeout_add_local_once(delay, move || check_load_older());
    }

    fn shows_surface(&self, channel_id: &str, thread_ts: Option<&str>) -> bool {
        self.read_mark_target
            .borrow()
            .as_ref()
            .is_some_and(|(channel, thread)| {
                channel == channel_id && thread.as_deref() == thread_ts
            })
    }

    /// Shows or hides the "loading older messages" spinner. While shown, no
    /// further `TimelineAction::LoadOlder` is dispatched. Clearing it
    /// re-checks the position, so a page too short to scroll away from the
    /// top chains into the next one.
    pub(crate) fn set_loading_older(&self, loading: bool) {
        if self.loading_older.replace(loading) == loading {
            return;
        }
        self.loading_older_indicator.set_visible(loading);
        if !loading {
            let check_load_older = self.check_load_older.clone();
            glib::idle_add_local_once(move || check_load_older());
        }
    }

    /// Highlights the row for `ts`, fading out after a moment. Re-renders in
    /// between keep it, since rows are bound against `highlight_ts`.
    fn set_highlight(&self, ts: &str) {
        *self.highlight_ts.borrow_mut() = Some(ts.to_string());
        let generation = self.highlight_generation.get().wrapping_add(1);
        self.highlight_generation.set(generation);
        let this = self.clone();
        glib::timeout_add_local_once(FOCUS_TARGET_DURATION, move || {
            if this.highlight_generation.get() != generation {
                return;
            }
            this.highlight_ts.borrow_mut().take();
            for item in this.bound_items.borrow().iter() {
                if let Some(child) = item.upgrade().and_then(|item| item.child()) {
                    child.remove_css_class(FOCUS_TARGET_CLASS);
                }
            }
        });
    }

    /// Re-renders the row of message `ts`, e.g. to re-enable its controls
    /// after an action finished.
    pub(crate) fn refresh_message(&self, ts: &str) {
        self.rebind_rows("other", |obj| {
            !obj.is_day_separator() && obj.message().ts == ts
        });
    }

    /// Rebuilds the bound rows matching `matches` in place. Store items are
    /// kept: replacing them destroys the focused row, and GTK's focus fallback
    /// then scrolls the list to its first item.
    fn rebind_rows(&self, reason: &str, matches: impl Fn(&TimelineMessageObject) -> bool) -> usize {
        let started = crate::debug::enabled().then(Instant::now);
        let mut rebound = 0;
        let items = self
            .bound_items
            .borrow()
            .iter()
            .filter_map(glib::WeakRef::upgrade)
            .collect::<Vec<_>>();
        for item in items {
            let Some(obj) = item.item().and_downcast::<TimelineMessageObject>() else {
                continue;
            };
            if !matches(&obj) {
                continue;
            }
            keep_focus_on_row(&item);
            (self.bind_row)(&item);
            rebound += 1;
        }
        if let Some(started) = started {
            crate::debug::log(
                "performance",
                &format!(
                    "timeline_rebind reason={reason} rows={rebound} elapsed_ms={:.2}",
                    started.elapsed().as_secs_f64() * 1_000.0
                ),
            );
        }
        rebound
    }

    /// Whether messages with replies show a footer that opens their thread.
    /// The thread pane turns this off for its own root message.
    pub(crate) fn set_opens_threads(&self, opens_threads: bool) {
        self.opens_threads.set(opens_threads);
    }

    /// Sets which conversation/thread this view's visibility-based
    /// auto-read-marking should advance. `thread_ts` is `None` for the main
    /// timeline surface (advances the conversation's `last_read` via
    /// `conversations.mark`) and `Some` for a thread pane surface (advances
    /// the thread's local-only read state instead).
    ///
    /// This is also the view's surface identity: switching it to another
    /// conversation or thread drops a pending older-page spinner and makes
    /// the next `set_messages` open at the newest message.
    pub(crate) fn set_read_mark_target(&self, channel_id: &str, thread_ts: Option<&str>) {
        let target = Some((channel_id.to_string(), thread_ts.map(ToString::to_string)));
        if *self.read_mark_target.borrow() != target {
            self.surface_changed.set(true);
            self.older_backoff.reset();
            self.set_loading_older(false);
        }
        *self.read_mark_target.borrow_mut() = target;
        // The pending dwell carries the old target, so always restart it.
        self.reset_read_candidate();
        (self.recheck_read_visibility)();
    }

    /// Forgets the dwelling candidate and re-evaluates visible rows, so rows
    /// already on screen are marked again, e.g. after read state changed
    /// elsewhere or the window regained focus. A dwell still pending for the
    /// same candidate is kept: rechecks arriving faster than the dwell would
    /// otherwise restart it forever and never mark anything.
    pub(crate) fn recheck_read(&self) {
        if !self.dwell_pending.get() {
            self.reset_read_candidate();
        }
        (self.recheck_read_visibility)();
    }

    /// Pauses auto-marking while the window is unfocused; regaining focus
    /// restarts the dwell for whatever is visible.
    pub(crate) fn set_window_active(&self, active: bool) {
        if self.window_active.replace(active) == active {
            return;
        }
        if active {
            self.recheck_read();
        } else {
            self.reset_read_candidate();
        }
    }

    /// Clears the candidate and cancels its in-flight dwell timer.
    fn reset_read_candidate(&self) {
        *self.read_candidate.borrow_mut() = None;
        self.read_generation
            .set(self.read_generation.get().wrapping_add(1));
        self.dwell_pending.set(false);
    }

    pub(crate) fn set_on_action<F: Fn(TimelineAction) + 'static>(&self, f: F) {
        *self.on_action.borrow_mut() = Some(Rc::new(f));
    }

    /// Re-renders the bound rows of messages `uses_asset` after an image asset
    /// arrived, keeping scroll position and focus. Arrivals within one idle
    /// are coalesced. Only the image fields of `context` are taken; the rest
    /// (unread anchor, thread) belongs to the rendered conversation.
    pub fn update_image_asset(&self, context: &MessageRenderContext, uses_asset: HashSet<String>) {
        {
            let mut current = self.context.borrow_mut();
            let Some(current) = current.as_mut() else {
                return;
            };
            current.image_assets = context.image_assets.clone();
            current.failed_image_urls = context.failed_image_urls.clone();
            current.user_avatar_urls = context.user_avatar_urls.clone();
        }
        self.asset_update_rows.borrow_mut().extend(uses_asset);
        if self.asset_update_pending.get() {
            return;
        }
        self.asset_update_pending.set(true);
        let this = self.clone();
        glib::idle_add_local_once(move || {
            this.asset_update_pending.set(false);
            let rows = std::mem::take(&mut *this.asset_update_rows.borrow_mut());
            this.rebind_rows("image_asset", |obj| {
                !obj.is_day_separator() && rows.contains(&obj.message().ts)
            });
        });
    }

    fn update_store_grouping(&self, unread_separator_ts: Option<&str>) -> HashSet<String> {
        let mut changed = HashSet::new();
        let n = self.store.n_items();
        let mut prev_msg: Option<SlackMessage> = None;
        for i in 0..n {
            let Some(obj) = self
                .store
                .item(i)
                .and_then(|o| o.downcast::<TimelineMessageObject>().ok())
            else {
                continue;
            };
            if obj.is_day_separator() {
                prev_msg = None;
                continue;
            }
            let msg = obj.message();
            let is_unread = unread_separator_ts == Some(msg.ts.as_str());
            let group = !is_unread
                && prev_msg
                    .as_ref()
                    .is_some_and(|prev| can_group_messages(prev, &msg));
            if obj.hide_header() != group {
                obj.set_hide_header(group);
                changed.insert(msg.ts.clone());
            }
            if is_system_subtype(&msg) || is_thread_broadcast(&msg) {
                prev_msg = None;
            } else {
                prev_msg = Some(msg);
            }
        }
        changed
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
        let changed = self.update_store_grouping(anchor_ts.as_deref());
        // Rows scrolled out of view pick the new anchor up from the context
        // when they are bound again.
        self.rebind_rows("other", |obj| {
            if obj.is_day_separator() {
                return false;
            }
            let ts = obj.message().ts;
            changed.contains(&ts)
                || previous.as_deref() == Some(ts.as_str())
                || anchor_ts.as_deref() == Some(ts.as_str())
        });
    }

    pub fn set_messages(
        &self,
        messages: &[SlackMessage],
        context: &MessageRenderContext,
        focus_ts: Option<&str>,
    ) {
        let started = crate::debug::enabled().then(Instant::now);
        self.placeholder_label.set_visible(false);
        self.scrolled_window.set_visible(true);
        // Cheap: directory snapshots are shared `Arc`s, cloned by refcount and
        // compared by pointer first (see `MessageRenderContext`).
        let previous = self.context.replace(Some(context.clone()));
        let rows_stale = previous
            .as_ref()
            .is_none_or(|previous| !previous.same_row_inputs(context));
        let previous_anchor = previous.and_then(|previous| previous.unread_separator_ts);
        let previous_highlight = self.highlight_ts.borrow().clone();
        *self.latest_message_ts.borrow_mut() = messages.first().map(|msg| msg.ts.clone());
        self.reset_read_candidate();
        // Removing the focused row would hand focus to GTK's fallback, which
        // scrolls the list to its first item. Park focus and restore it on the
        // target row instead.
        let had_focus = self
            .list_view
            .state_flags()
            .contains(gtk::StateFlags::FOCUS_WITHIN);
        if had_focus {
            if let Some(root) = self.list_view.root() {
                root.set_focus(None::<&Widget>);
            }
        }
        if let Some(ts) = focus_ts {
            self.set_highlight(ts);
        }
        let vadj = self.scrolled_window.vadjustment();
        let fresh = self.surface_changed.replace(false) || self.store.n_items() == 0;
        let pin_to_bottom = pins_to_bottom(context.timeline_scroll, fresh, is_near_bottom(&vadj));
        self.settling.set(true);
        let settle_generation = self.settle_generation.get().wrapping_add(1);
        self.settle_generation.set(settle_generation);

        let unread_ts = context.unread_separator_ts.as_deref();
        let (items, focus_index) = build_store_items(messages, focus_ts, unread_ts);
        let inserted = self.splice_changed_items(&items);
        let kept = |obj: &TimelineMessageObject| !inserted.iter().any(|new| new == obj);
        let rebound = if rows_stale {
            // Kept rows were built against the previous context.
            self.rebind_rows("context", kept)
        } else {
            // Otherwise only kept rows that gain or lose the unread separator
            // or the focus highlight differ.
            let mut moved = Vec::new();
            if previous_anchor != context.unread_separator_ts {
                moved.extend(previous_anchor.as_deref());
                moved.extend(unread_ts);
            }
            if focus_ts.is_some() {
                moved.extend(previous_highlight.as_deref());
                moved.extend(focus_ts);
            }
            if moved.is_empty() {
                0
            } else {
                self.rebind_rows("other", |obj| {
                    !obj.is_day_separator()
                        && moved.contains(&obj.message().ts.as_str())
                        && kept(obj)
                })
            }
        };
        if let Some(started) = started {
            crate::debug::log(
                "performance",
                &format!(
                    "timeline_set_messages rows={} inserted={} rebound={rebound} context_changed={rows_stale} elapsed_ms={:.2}",
                    items.len(),
                    inserted.len(),
                    started.elapsed().as_secs_f64() * 1_000.0
                ),
            );
        }

        let list_view = self.list_view.clone();
        let n_items = items.len() as u32;
        let bound_items = self.bound_items.clone();
        let recheck_read_visibility = self.recheck_read_visibility.clone();
        glib::idle_add_local_once(move || {
            // Without a focus target or a pin, GtkListView keeps its anchor
            // row in place, which preserves the position across prepends.
            let target =
                focus_index.or_else(|| pin_to_bottom.then(|| n_items.checked_sub(1)).flatten());
            if let Some(index) = target {
                let info = gtk::ScrollInfo::new();
                list_view.scroll_to(index, gtk::ListScrollFlags::FOCUS, Some(info));
                if focus_index.is_some() {
                    if vadj.page_size() > 0.0 && n_items > 0 {
                        let approx = (f64::from(index) / f64::from(n_items)) * vadj.upper()
                            - vadj.page_size() / 2.0;
                        let max = (vadj.upper() - vadj.page_size()).max(vadj.lower());
                        vadj.set_value(approx.clamp(vadj.lower(), max));
                    }
                    center_row_after_layout(&list_view, vadj, bound_items, index, n_items);
                }
            } else if pin_to_bottom {
                vadj.set_value(vadj.upper() - vadj.page_size());
            }
            if had_focus {
                list_view.grab_focus();
            }
            recheck_read_visibility();
        });
        let recheck_delayed = self.recheck_read_visibility.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
            recheck_delayed();
        });
        // Centering a focus target keeps adjusting for a few hundred ms.
        let settle_delay = if focus_index.is_some() { 450 } else { 150 };
        let settling = self.settling.clone();
        let current_settle = self.settle_generation.clone();
        let check_load_older = self.check_load_older.clone();
        let vadj = self.scrolled_window.vadjustment();
        let pin_pending = pin_to_bottom && focus_index.is_none();
        let settle_started = Instant::now();
        glib::timeout_add_local(std::time::Duration::from_millis(settle_delay), move || {
            if current_settle.get() != settle_generation {
                return glib::ControlFlow::Break;
            }
            // The pin lands on the next layout. When that frame is slower
            // than the settle delay the view still sits at the oldest row,
            // and ending the settle there would page in older history.
            if pin_pending
                && !is_near_bottom(&vadj)
                && settle_started.elapsed() < PIN_SETTLE_DEADLINE
            {
                return glib::ControlFlow::Continue;
            }
            settling.set(false);
            check_load_older();
            glib::ControlFlow::Break
        });
    }

    /// Replaces only run of store items differing from `items`, so
    /// update to one message rebuilds one row instead of whole timeline.
    /// Returns inserted items.
    fn splice_changed_items<'a>(
        &self,
        items: &'a [TimelineMessageObject],
    ) -> &'a [TimelineMessageObject] {
        let old_len = self.store.n_items() as usize;
        let old_at = |index: usize| {
            self.store
                .item(index as u32)
                .and_downcast::<TimelineMessageObject>()
        };
        let max_common = old_len.min(items.len());
        let prefix = (0..max_common)
            .take_while(|&i| old_at(i).is_some_and(|old| old.same_row(&items[i])))
            .count();
        let suffix = (0..max_common - prefix)
            .take_while(|&i| {
                old_at(old_len - 1 - i).is_some_and(|old| old.same_row(&items[items.len() - 1 - i]))
            })
            .count();
        let inserted = &items[prefix..items.len() - suffix];
        let removed = old_len - prefix - suffix;
        if removed > 0 || !inserted.is_empty() {
            self.store.splice(prefix as u32, removed as u32, inserted);
        }
        inserted
    }

    /// Shows a plain-text placeholder (loading state, empty state, error message) in place of
    /// the message list, for surfaces that have no structured content to render yet.
    pub fn show_placeholder(&self, text: &str) {
        self.set_loading_older(false);
        self.store.remove_all();
        self.placeholder_label.set_label(text);
        self.placeholder_label.set_visible(true);
        self.scrolled_window.set_visible(false);
    }

    pub fn widget(&self) -> &Widget {
        self.container.upcast_ref()
    }
}

/// Distance from the newest message within which the view still counts as
/// following the conversation.
const STICK_TO_BOTTOM_SLACK: f64 = 48.0;

/// How long a render pinned to the newest message keeps paging suspended
/// while waiting for the pin to land.
const PIN_SETTLE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

fn is_near_bottom(vadj: &gtk::Adjustment) -> bool {
    vadj.page_size() <= 0.0
        || vadj.upper() - vadj.page_size() - vadj.value() <= STICK_TO_BOTTOM_SLACK
}

/// Whether a render should land on the newest message. Fresh content always
/// does; otherwise only `Bottom`, or `StickToBottom` while already there.
/// `Preserve`/`PreservePrepend` leave the position to GtkListView's anchor.
pub(crate) fn pins_to_bottom(
    behavior: TimelineScrollBehavior,
    fresh: bool,
    was_near_bottom: bool,
) -> bool {
    fresh
        || match behavior {
            TimelineScrollBehavior::Bottom => true,
            TimelineScrollBehavior::StickToBottom => was_near_bottom,
            TimelineScrollBehavior::Preserve | TimelineScrollBehavior::PreservePrepend => false,
        }
}

/// Exponential backoff for failed older-page loads: 2s, 4s, 8s, ... capped
/// at 30s, cleared by a successful page or a surface switch.
#[derive(Default)]
pub(crate) struct OlderPageBackoff {
    failures: Cell<u32>,
    retry_at: Cell<Option<std::time::Instant>>,
}

impl OlderPageBackoff {
    const BASE: std::time::Duration = std::time::Duration::from_secs(2);
    const MAX: std::time::Duration = std::time::Duration::from_secs(30);

    /// Records a failure at `now` and returns how long to wait before the
    /// next attempt.
    pub(crate) fn fail(&self, now: std::time::Instant) -> std::time::Duration {
        let failures = self.failures.get().saturating_add(1);
        self.failures.set(failures);
        let delay = Self::BASE
            .saturating_mul(1 << (failures - 1).min(5))
            .min(Self::MAX);
        self.retry_at.set(Some(now + delay));
        delay
    }

    pub(crate) fn cooling_down(&self, now: std::time::Instant) -> bool {
        self.retry_at.get().is_some_and(|at| now < at)
    }

    pub(crate) fn reset(&self) {
        self.failures.set(0);
        self.retry_at.set(None);
    }
}

/// Whether the viewport is within half a page of the oldest loaded message.
pub(crate) fn near_oldest_edge(offset_from_top: f64, page_size: f64) -> bool {
    page_size > 0.0 && offset_from_top < page_size / 2.0
}

/// Retry budget for [`center_row_after_layout`]: matches the 450ms
/// `settle_delay` that `set_messages` already allows a focus target to keep
/// adjusting for, so a row that never centers gives up on the same schedule
/// the rest of the settle logic assumes rather than spinning indefinitely.
const CENTER_ROW_DEADLINE: std::time::Duration = std::time::Duration::from_millis(450);

/// Whether `index` can no longer be centered because it fell out of the
/// list's current model, e.g. a splice shrank or reordered the store after
/// centering started.
fn index_out_of_bounds(list_view: &gtk::ListView, index: u32) -> bool {
    list_view
        .model()
        .is_none_or(|model| index >= model.n_items())
}

/// `scroll_to` only brings a row to the nearest edge and `ScrollInfo` cannot
/// center, so recenter once the scrolled layout is in place: tick callbacks
/// run before layout, hence the second frame.
///
/// Returns a flag set once the retry loop stops (centered, deadline hit, or
/// the target row became invalid), mainly so tests can observe that it does
/// stop instead of retrying forever.
fn center_row_after_layout(
    list_view: &gtk::ListView,
    vadj: gtk::Adjustment,
    bound_items: BoundItems,
    index: u32,
    n_items: u32,
) -> Rc<Cell<bool>> {
    let done = Rc::new(Cell::new(false));
    let start = std::time::Instant::now();

    // Fast path: when running under a live compositor, tick callbacks fire every frame.
    {
        let done = done.clone();
        let list_view_weak = list_view.downgrade();
        let vadj = vadj.clone();
        let bound_items = bound_items.clone();
        let frames = Cell::new(0_u8);
        list_view.add_tick_callback(move |list_view, _| {
            // The target index itself going out of bounds (e.g. a shrinking
            // splice) is a correctness issue, not just a slow retry, so it
            // stops on the very next tick rather than waiting out the
            // deadline below.
            if done.get()
                || index_out_of_bounds(list_view, index)
                || start.elapsed() >= CENTER_ROW_DEADLINE
            {
                done.set(true);
                return glib::ControlFlow::Break;
            }
            frames.set(frames.get() + 1);
            if frames.get() < 2 {
                list_view.queue_draw();
                return glib::ControlFlow::Continue;
            }
            if center_row(list_view, &vadj, &bound_items, index) {
                done.set(true);
                glib::ControlFlow::Break
            } else {
                if let Some(lv) = list_view_weak.upgrade() {
                    lv.scroll_to(
                        index,
                        gtk::ListScrollFlags::FOCUS,
                        Some(gtk::ScrollInfo::new()),
                    );
                }
                glib::ControlFlow::Continue
            }
        });
    }

    // Fallback path: in headless environments or under heavy load where frame ticks
    // do not drive the frame clock, poll until the list view is allocated and centered.
    {
        let list_view = list_view.clone();
        let attempts = Rc::new(Cell::new(0_u8));
        let done = done.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(20), move || {
            if done.get()
                || index_out_of_bounds(&list_view, index)
                || start.elapsed() >= CENTER_ROW_DEADLINE
            {
                done.set(true);
                return glib::ControlFlow::Break;
            }
            let count = attempts.get() + 1;
            attempts.set(count);
            if list_view.height() == 0 {
                return if count >= 20 {
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                };
            }
            if center_row(&list_view, &vadj, &bound_items, index) {
                done.set(true);
                glib::ControlFlow::Break
            } else {
                if vadj.page_size() > 0.0 && n_items > 0 {
                    let approx = (f64::from(index) / f64::from(n_items)) * vadj.upper()
                        - vadj.page_size() / 2.0;
                    let max = (vadj.upper() - vadj.page_size()).max(vadj.lower());
                    vadj.set_value(approx.clamp(vadj.lower(), max));
                }
                list_view.scroll_to(
                    index,
                    gtk::ListScrollFlags::FOCUS,
                    Some(gtk::ScrollInfo::new()),
                );
                if count >= 20 {
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            }
        });
    }

    done
}

fn center_row(
    list_view: &gtk::ListView,
    vadj: &gtk::Adjustment,
    bound_items: &BoundItems,
    index: u32,
) -> bool {
    let bound = bound_items.borrow();
    let matching_items: Vec<_> = bound
        .iter()
        .filter_map(glib::WeakRef::upgrade)
        .filter(|item| item.position() == index)
        .collect();
    if matching_items.is_empty() {
        return false;
    }
    let item = &matching_items[0];
    let Some(row) = item.child() else {
        return false;
    };
    let Some(bounds) = row.compute_bounds(list_view) else {
        return false;
    };
    if bounds.height() <= 0.0 {
        return false;
    }
    let offset = f64::from(bounds.y() + bounds.height() / 2.0) - vadj.page_size() / 2.0;
    let max = (vadj.upper() - vadj.page_size()).max(vadj.lower());
    vadj.set_value((vadj.value() + offset).clamp(vadj.lower(), max));
    true
}

impl Default for NativeTimelineView {
    fn default() -> Self {
        Self::new()
    }
}

/// Moves focus from inside `item`'s row content to the row itself before the
/// content is replaced, so focus survives and the list does not jump.
/// Rows compute their day-relative timestamp and day labels ("Today") at
/// bind time, and re-renders keep unchanged rows bound, so rebind them all
/// once the local day rolls over. Stops when the view's widgets are gone.
fn schedule_day_rollover_rebind(
    container: glib::WeakRef<Box>,
    bound_items: BoundItems,
    bind_row: Rc<dyn Fn(&gtk::ListItem)>,
) {
    glib::timeout_add_seconds_local_once(seconds_until_local_midnight(), move || {
        if container.upgrade().is_none() {
            return;
        }
        let items = bound_items
            .borrow()
            .iter()
            .filter_map(glib::WeakRef::upgrade)
            .collect::<Vec<_>>();
        for item in items {
            keep_focus_on_row(&item);
            bind_row(&item);
        }
        schedule_day_rollover_rebind(container, bound_items, bind_row);
    });
}

/// Seconds until just past the next local midnight; an hour if the local
/// time cannot be determined, so the rollover is retried rather than lost.
fn seconds_until_local_midnight() -> u32 {
    const FALLBACK_SECONDS: u32 = 3600;
    let Ok(now) = glib::DateTime::now_local() else {
        return FALLBACK_SECONDS;
    };
    let midnight = now.add_days(1).and_then(|tomorrow| {
        glib::DateTime::from_local(
            tomorrow.year(),
            tomorrow.month(),
            tomorrow.day_of_month(),
            0,
            0,
            0.0,
        )
    });
    match midnight {
        Ok(midnight) => {
            let micros = midnight.difference(&now).as_microseconds().max(0);
            u32::try_from(micros / 1_000_000 + 1).unwrap_or(FALLBACK_SECONDS)
        }
        Err(_) => FALLBACK_SECONDS,
    }
}

fn keep_focus_on_row(item: &gtk::ListItem) {
    let Some(child) = item.child() else {
        return;
    };
    if !child.has_focus() && !child.state_flags().contains(gtk::StateFlags::FOCUS_WITHIN) {
        return;
    }
    if let Some(row) = child.parent() {
        row.grab_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::tests::{pump, pump_until, run_gtk_test, test_context};

    #[test]
    fn next_rollover_is_within_one_local_day() {
        // A DST day can last 25 hours; the extra second lands past midnight.
        let seconds = seconds_until_local_midnight();
        assert!((1..=25 * 3600 + 1).contains(&seconds), "got {seconds}");
    }

    /// Tests that create GTK widgets. They run only when
    /// `CONDUIT_GRAPHICAL_TESTS` is set (see `run_gtk_test`).
    mod graphical {
        use super::*;

        /// A store shrinking out from under an in-flight centering retry (e.g.
        /// `splice_changed_items` dropping rows while a scroll-to-focus pass is
        /// still settling) must not leave the retry spinning on every frame
        /// forever: it should notice the target index is no longer valid and
        /// stop on the very next tick.
        #[test]
        fn center_row_after_layout_stops_when_target_index_is_out_of_bounds() {
            run_gtk_test(|| {
                let ctx = test_context();
                let messages = (0..5)
                    .map(|i| SlackMessage {
                        ts: format!("170000000{i}.000100"),
                        user: Some("U123".to_string()),
                        text: Some(format!("message {i}")),
                        ..Default::default()
                    })
                    .collect::<Vec<_>>();
                let view = NativeTimelineView::new();
                let window = gtk::Window::new();
                window.set_default_size(400, 500);
                window.set_child(Some(view.widget()));
                window.present();
                view.set_messages(&messages, &ctx, None);
                pump(300);

                // Simulate the target row having fallen out of the store (e.g. an
                // incremental splice shrank it) before centering could finish.
                let out_of_range_index = view.store.n_items() + 100;
                let vadj = view.scrolled_window.vadjustment();
                let start = std::time::Instant::now();
                let done = center_row_after_layout(
                    &view.list_view,
                    vadj,
                    view.bound_items.clone(),
                    out_of_range_index,
                    view.store.n_items(),
                );

                pump_until(2_000, || done.get());
                let elapsed = start.elapsed();

                assert!(
                    done.get(),
                    "an out-of-bounds target should stop the retry instead of spinning forever"
                );
                assert!(
                    elapsed < CENTER_ROW_DEADLINE,
                    "an out-of-bounds index should be noticed well before the \
                     {CENTER_ROW_DEADLINE:?} retry deadline, took {elapsed:?}"
                );

                window.destroy();
            });
        }
    }
}
