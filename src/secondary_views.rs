//! Native Threads, Search, Files and Later views.
//!
//! One list widget serves all four views: the window builds view-model rows
//! ([`model`], [`search`], [`files`]) from workspace data and hands them to
//! [`SecondaryView`], which renders cards ([`rows`]) and reports user intent
//! as [`SecondaryAction`]s.

mod files;
mod model;
mod rows;
mod search;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib::subclass::prelude::*;
use gtk::{gio, glib};

pub(crate) use files::file_rows;
pub(crate) use model::{
    saved_rows, thread_rows, SecondaryAction, SecondaryKind, SecondaryRow, ThreadState,
};
pub(crate) use search::search_rows;

use crate::message_markup::{ContextDelta, MessageRenderContext};
use rows::Dispatch;

const STATUS_PAGE: &str = "status";
const LIST_PAGE: &str = "list";

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SecondaryRowObject {
        pub(crate) row: RefCell<Option<Rc<SecondaryRow>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SecondaryRowObject {
        const NAME: &'static str = "ConduitSecondaryRowObject";
        type Type = super::SecondaryRowObject;
        type ParentType = glib::Object;
    }

    impl ObjectImpl for SecondaryRowObject {}
}

glib::wrapper! {
    pub struct SecondaryRowObject(ObjectSubclass<imp::SecondaryRowObject>);
}

impl SecondaryRowObject {
    fn new(row: SecondaryRow) -> Self {
        let object: Self = glib::Object::builder().build();
        *object.imp().row.borrow_mut() = Some(Rc::new(row));
        object
    }

    fn row(&self) -> Option<Rc<SecondaryRow>> {
        self.imp().row.borrow().clone()
    }

    /// Swaps in `other`'s row without rebinding, for rows that render the same.
    fn adopt_row(&self, other: &Self) {
        *self.imp().row.borrow_mut() = other.row();
    }
}

/// Store edits around `kept` (ascending pairs of old and new indices whose
/// old item stays): `(position in old, removed, new index range)` each.
fn splices_around(
    kept: &[(usize, usize)],
    old_len: usize,
    new_len: usize,
) -> Vec<(usize, usize, std::ops::Range<usize>)> {
    let mut splices = Vec::new();
    let (mut old_next, mut new_next) = (0, 0);
    for &(old_index, new_index) in kept.iter().chain([&(old_len, new_len)]) {
        if old_index > old_next || new_index > new_next {
            splices.push((old_next, old_index - old_next, new_next..new_index));
        }
        (old_next, new_next) = (old_index + 1, new_index + 1);
    }
    splices
}

type ActionHandler = Rc<RefCell<Option<Dispatch>>>;

/// Invokes the action handler without holding the `RefCell` borrow: the
/// handler may re-render this view or replace itself.
fn dispatch_action(handler: &ActionHandler, action: SecondaryAction) {
    let handler = handler.borrow().clone();
    if let Some(handler) = handler {
        handler(action);
    }
}

#[derive(Clone)]
pub(crate) struct SecondaryView {
    stack: gtk::Stack,
    status_page: adw::StatusPage,
    scrolled_window: gtk::ScrolledWindow,
    list_view: gtk::ListView,
    store: gio::ListStore,
    context: Rc<RefCell<MessageRenderContext>>,
    on_action: ActionHandler,
    kind: Rc<Cell<Option<SecondaryKind>>>,
}

impl std::fmt::Debug for SecondaryView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecondaryView")
            .field("kind", &self.kind.get())
            .field("rows", &self.store.n_items())
            .finish()
    }
}

impl SecondaryView {
    pub(crate) fn new() -> Self {
        let store = gio::ListStore::new::<SecondaryRowObject>();
        let context = Rc::new(RefCell::new(MessageRenderContext::default()));
        let on_action: ActionHandler = Rc::new(RefCell::new(None));
        let dispatch: Dispatch = {
            let on_action = on_action.clone();
            Rc::new(move |action| dispatch_action(&on_action, action))
        };

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                item.set_child(Some(&adw::Bin::new()));
            }
        });
        {
            let context = context.clone();
            let dispatch = dispatch.clone();
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(row) = item
                    .item()
                    .and_downcast::<SecondaryRowObject>()
                    .and_then(|object| object.row())
                else {
                    return;
                };
                let Some(bin) = item.child().and_downcast::<adw::Bin>() else {
                    return;
                };
                let context = context.borrow();
                item.set_accessible_label(&row.accessible_label(&context));
                item.set_activatable(row.primary_action().is_some());
                bin.set_child(Some(&rows::build_row(&row, &context, &dispatch)));
            });
        }
        factory.connect_unbind(|_, item| {
            if let Some(bin) = item
                .downcast_ref::<gtk::ListItem>()
                .and_then(|item| item.child())
                .and_downcast::<adw::Bin>()
            {
                bin.set_child(None::<&gtk::Widget>);
            }
        });

        let list_view = gtk::ListView::new(
            Some(gtk::NoSelection::new(Some(store.clone()))),
            Some(factory),
        );
        list_view.set_single_click_activate(true);
        list_view.set_tab_behavior(gtk::ListTabBehavior::Item);
        list_view.add_css_class("background");
        {
            let store = store.clone();
            list_view.connect_activate(move |_, position| {
                let action = store
                    .item(position)
                    .and_downcast::<SecondaryRowObject>()
                    .and_then(|object| object.row())
                    .and_then(|row| row.primary_action());
                if let Some(action) = action {
                    dispatch(action);
                }
            });
        }

        let scrolled_window = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .hexpand(true)
            .child(&list_view)
            .build();
        let kind: Rc<Cell<Option<SecondaryKind>>> = Rc::new(Cell::new(None));
        {
            let kind = kind.clone();
            let on_action = on_action.clone();
            scrolled_window.connect_edge_reached(move |_, position| {
                if position == gtk::PositionType::Bottom
                    && kind.get() == Some(SecondaryKind::Threads)
                {
                    dispatch_action(&on_action, SecondaryAction::LoadMoreThreads);
                }
            });
        }
        let status_page = adw::StatusPage::builder().vexpand(true).build();
        let stack = gtk::Stack::builder()
            .vexpand(true)
            .hexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        stack.add_named(&status_page, Some(STATUS_PAGE));
        stack.add_named(&scrolled_window, Some(LIST_PAGE));
        stack.set_visible_child_name(STATUS_PAGE);

        Self {
            stack,
            status_page,
            scrolled_window,
            list_view,
            store,
            context,
            on_action,
            kind,
        }
    }

    pub(crate) fn widget(&self) -> &gtk::Widget {
        self.stack.upcast_ref()
    }

    pub(crate) fn set_on_action<F: Fn(SecondaryAction) + 'static>(&self, handler: F) {
        *self.on_action.borrow_mut() = Some(Rc::new(handler));
    }

    pub(crate) fn show_loading(&self, kind: SecondaryKind, description: &str) {
        let spinner = adw::SpinnerPaintable::new(Some(&self.status_page));
        self.show_status(
            kind,
            None,
            Some(spinner.upcast_ref()),
            &kind.title(),
            description,
        );
    }

    pub(crate) fn show_error(&self, kind: SecondaryKind, title: &str, description: &str) {
        self.show_status(
            kind,
            Some("dialog-warning-symbolic"),
            None,
            title,
            description,
        );
    }

    /// Replaces the rows. Re-rendering the same view keeps its scroll offset.
    pub(crate) fn show_rows(
        &self,
        kind: SecondaryKind,
        rows: Vec<SecondaryRow>,
        context: &MessageRenderContext,
    ) {
        if rows.is_empty() {
            let empty = kind.empty_state();
            self.show_status(
                kind,
                Some(empty.icon_name),
                None,
                &empty.title,
                &empty.description,
            );
            return;
        }
        let adjustment = self.scrolled_window.vadjustment();
        let preserved_offset = (self.kind.get() == Some(kind)
            && self.stack.visible_child_name().as_deref() == Some(LIST_PAGE))
        .then(|| adjustment.value());
        let same_kind = self.kind.get() == Some(kind);
        let delta = context.delta_from(&self.context.replace(context.clone()));
        self.kind.set(Some(kind));
        let objects = rows
            .into_iter()
            .map(SecondaryRowObject::new)
            .collect::<Vec<_>>();
        if same_kind {
            self.splice_changed(&objects, &delta);
        } else {
            self.store.splice(0, self.store.n_items(), &objects);
        }
        self.stack.set_visible_child_name(LIST_PAGE);
        self.list_view
            .update_property(&[gtk::accessible::Property::Label(&kind.title())]);
        glib::idle_add_local_once(move || {
            adjustment.set_value(preserved_offset.unwrap_or(0.0));
        });
    }

    /// Replaces only the rows that render differently: kept rows (common
    /// prefix and suffix) keep their widgets, and with them any selection.
    fn splice_changed(&self, objects: &[SecondaryRowObject], delta: &ContextDelta) {
        let old = (0..self.store.n_items())
            .filter_map(|index| self.store.item(index).and_downcast::<SecondaryRowObject>())
            .collect::<Vec<_>>();
        let context = self.context.borrow();
        let keeps = |old: &SecondaryRowObject, new: &SecondaryRowObject| {
            let (Some(old), Some(new)) = (old.row(), new.row()) else {
                return false;
            };
            old.renders_same(&new)
                && match new.message() {
                    Some(message) => !delta.affects(message, &context.custom_emojis),
                    None => delta.is_empty(),
                }
        };
        let common = old.len().min(objects.len());
        let prefix = (0..common)
            .take_while(|&i| keeps(&old[i], &objects[i]))
            .count();
        let suffix = (0..common - prefix)
            .take_while(|&i| keeps(&old[old.len() - 1 - i], &objects[objects.len() - 1 - i]))
            .count();
        let kept = (0..prefix)
            .map(|i| (i, i))
            .chain(
                (0..suffix)
                    .rev()
                    .map(|i| (old.len() - 1 - i, objects.len() - 1 - i)),
            )
            .collect::<Vec<_>>();
        for &(old_index, new_index) in &kept {
            old[old_index].adopt_row(&objects[new_index]);
        }
        drop(context);
        for (position, removed, range) in splices_around(&kept, old.len(), objects.len())
            .into_iter()
            .rev()
        {
            self.store
                .splice(position as u32, removed as u32, &objects[range]);
        }
    }

    fn show_status(
        &self,
        kind: SecondaryKind,
        icon_name: Option<&str>,
        paintable: Option<&gtk::gdk::Paintable>,
        title: &str,
        description: &str,
    ) {
        self.kind.set(Some(kind));
        self.store.remove_all();
        self.status_page.set_icon_name(icon_name);
        self.status_page.set_paintable(paintable);
        self.status_page.set_title(title);
        self.status_page.set_description(Some(description));
        self.stack.set_visible_child_name(STATUS_PAGE);
    }
}

impl Default for SecondaryView {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::model::{FileOpenTarget, FileRow, SavedRow, SearchRow, ThreadRow};
    use super::*;
    use crate::models::{SearchMessageLocation, SlackMessage};
    use crate::timeline_message_widget::tests::{run_gtk_test, test_context};

    fn message(ts: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.to_string(),
            user: Some("U123".to_string()),
            text: Some("hello".to_string()),
            ..SlackMessage::default()
        }
    }

    fn sample_rows() -> Vec<SecondaryRow> {
        vec![
            SecondaryRow::Thread(ThreadRow {
                channel_id: "C1".to_string(),
                channel_title: "#general".to_string(),
                root: message("1.0"),
                reply_count: 2,
                participants: "Alice".to_string(),
                unread: true,
                unread_count: 1,
            }),
            SecondaryRow::Search(SearchRow {
                location: SearchMessageLocation::new("C1", "3.0", None),
                channel_title: "#general".to_string(),
                author: "Alice".to_string(),
                ts: Some("3.0".to_string()),
                plain_text: "ship it".to_string(),
                snippet_markup: "<b>ship</b> it".to_string(),
                permalink: Some("https://example.slack.com/p3".to_string()),
            }),
            SecondaryRow::File(FileRow {
                title: "report.pdf".to_string(),
                icon_name: "x-office-document-symbolic",
                detail: "PDF - 1.0 KB".to_string(),
                owner: Some("Alice".to_string()),
                channel_title: None,
                created_ts: None,
                thumbnail_url: None,
                open: FileOpenTarget::Unavailable,
            }),
            SecondaryRow::Saved(SavedRow {
                channel_id: "C1".to_string(),
                channel_title: "#general".to_string(),
                message: message("4.0"),
            }),
        ]
    }

    fn find_button(widget: &gtk::Widget, tooltip: &str) -> Option<gtk::Button> {
        if let Some(button) = widget.downcast_ref::<gtk::Button>() {
            if button.tooltip_text().as_deref() == Some(tooltip) {
                return Some(button.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Some(found) = find_button(&current, tooltip) {
                return Some(found);
            }
            child = current.next_sibling();
        }
        None
    }

    #[test]
    fn dispatch_allows_handler_to_replace_itself() {
        let handler: ActionHandler = Rc::new(RefCell::new(None));
        let slot = handler.clone();
        let replaced = Rc::new(Cell::new(false));
        let flag = replaced.clone();
        *handler.borrow_mut() = Some(Rc::new(move |_| {
            *slot.borrow_mut() = Some(Rc::new(|_| {}));
            flag.set(true);
        }));
        dispatch_action(
            &handler,
            SecondaryAction::OpenExternal("https://x".to_string()),
        );
        assert!(replaced.get());
    }

    /// Tests that create GTK widgets. They run only when
    /// `CONDUIT_GRAPHICAL_TESTS` is set (see `run_gtk_test`).
    mod graphical {
        use super::*;

        use crate::timeline::cache::decode;
        use crate::timeline_message_widget::tests::{
            descendants, pump, pump_until, save_test_image, AssetCacheTestDir, ManualDecodes,
        };

        /// More rows than GtkListView realizes at once, a few hundred.
        const THUMBNAIL_ROWS: i32 = 600;

        fn thumbnail_side(index: i32) -> i32 {
            16 + index % 50
        }

        fn file_row(title: &str, url: String) -> SecondaryRow {
            SecondaryRow::File(FileRow {
                title: title.to_string(),
                icon_name: "image-x-generic-symbolic",
                detail: "PNG".to_string(),
                owner: None,
                channel_title: None,
                created_ts: None,
                thumbnail_url: Some(url),
                open: FileOpenTarget::Unavailable,
            })
        }

        /// Files rows whose thumbnails are PNGs of a distinct size each, so
        /// a row showing another row's image is visible.
        fn thumbnail_rows(
            assets: &AssetCacheTestDir,
            context: &mut MessageRenderContext,
        ) -> Vec<SecondaryRow> {
            (0..THUMBNAIL_ROWS)
                .map(|index| {
                    let path = assets.0.join(format!("thumb-{index}.png"));
                    save_test_image(&path, "png", thumbnail_side(index), thumbnail_side(index));
                    let url = format!("https://files.slack.com/thumb-{index}.png");
                    context.image_assets.insert(url.clone(), path);
                    file_row(&format!("file-{index}"), url)
                })
                .collect()
        }

        /// Each bound row's title with the width of the thumbnail it shows.
        fn shown_thumbnails(view: &SecondaryView) -> Vec<(String, Option<i32>)> {
            descendants(view.list_view.upcast_ref())
                .into_iter()
                .filter_map(|widget| widget.downcast::<adw::Clamp>().ok())
                .filter_map(|clamp| {
                    let inside = descendants(clamp.upcast_ref());
                    let title = inside
                        .iter()
                        .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                        .find(|label| label.has_css_class("heading"))?
                        .text()
                        .to_string();
                    let width = inside
                        .iter()
                        .filter_map(|widget| widget.downcast_ref::<gtk::Picture>())
                        .find_map(|picture| {
                            let deferred = picture
                                .paintable()
                                .and_downcast::<decode::DeferredPaintable>()?;
                            Some(deferred.content()?.intrinsic_width())
                        });
                    Some((title, width))
                })
                .collect()
        }

        fn show_files(rows: Vec<SecondaryRow>, context: &MessageRenderContext) -> SecondaryView {
            let view = SecondaryView::new();
            let window = gtk::Window::new();
            window.set_default_size(640, 400);
            window.set_child(Some(view.widget()));
            window.present();
            pump_until(5000, || window.height() > 0);
            view.show_rows(SecondaryKind::Files, rows, context);
            pump_until(5000, || view.list_view.height() > 0);
            pump(50);
            view
        }

        /// Rows that were bound, but never mapped, wait for a request that
        /// scrolling brings; the ones that painted must show their own image.
        /// Returns how many painted.
        fn assert_each_painted_row_shows_its_own_thumbnail(view: &SecondaryView) -> usize {
            let painted: Vec<_> = shown_thumbnails(view)
                .into_iter()
                .filter_map(|(title, width)| Some((title, width?)))
                .collect();
            assert!(!painted.is_empty(), "no row painted its thumbnail");
            for (title, width) in &painted {
                let index: i32 = title["file-".len()..].parse().expect("index");
                assert_eq!(*width, thumbnail_side(index), "{title}");
            }
            painted.len()
        }

        #[test]
        fn perf_guard_files_thumbnails_decode_off_the_gtk_thread() {
            run_gtk_test(|| {
                let _ = adw::init();
                let decodes = ManualDecodes::install();
                let assets = AssetCacheTestDir::new("files-guard");
                let mut context = test_context();
                let rows = thumbnail_rows(&assets, &mut context);
                let before = crate::debug::IMAGE_DECODES.get();
                let view = show_files(rows, &context);
                pump_until(2000, || decodes.0.running().len() == decode::MAX_IN_FLIGHT);
                pump(20);
                assert_eq!(
                    crate::debug::IMAGE_DECODES.get() - before,
                    0,
                    "Files rows must leave decodes to the workers"
                );
                let bound = shown_thumbnails(&view).len();
                assert!(bound < THUMBNAIL_ROWS as usize, "every row was bound");
                assert!(shown_thumbnails(&view)
                    .iter()
                    .all(|(_, width)| width.is_none()));
                let running = decodes.0.running();
                assert_eq!(running.len(), decode::MAX_IN_FLIGHT, "{running:?}");

                // Only the rows on screen were asked for, and each paints
                // its own image in place once it lands.
                let finished = decodes.finish_all();
                pump(20);
                let painted = assert_each_painted_row_shows_its_own_thumbnail(&view);
                assert_eq!(painted, finished.len(), "{finished:?}");
                assert!(
                    painted < bound,
                    "rows that are not mapped asked for decodes"
                );
            });
        }

        #[test]
        fn recycled_file_rows_show_only_their_own_thumbnail() {
            run_gtk_test(|| {
                let _ = adw::init();
                let decodes = ManualDecodes::install();
                let assets = AssetCacheTestDir::new("files-recycle");
                let mut context = test_context();
                let rows = thumbnail_rows(&assets, &mut context);
                let view = show_files(rows, &context);
                pump_until(2000, || decodes.0.running().len() == decode::MAX_IN_FLIGHT);
                assert_eq!(decodes.0.running().len(), decode::MAX_IN_FLIGHT);
                // Scrolled away before any decode landed, so the rows they
                // were for are off screen when the workers finish.
                let adjustment = view.scrolled_window.vadjustment();
                adjustment.set_value(adjustment.upper());
                pump_until(2000, || {
                    shown_thumbnails(&view)
                        .iter()
                        .any(|(title, _)| title == "file-599")
                });
                pump(50);
                decodes.finish_all();
                pump(20);
                let shown = shown_thumbnails(&view);
                for (title, width) in &shown {
                    assert!(
                        width.is_none() || *title != "file-0",
                        "a row that scrolled away painted: {title}"
                    );
                }
                assert_each_painted_row_shows_its_own_thumbnail(&view);
            });
        }

        #[test]
        fn file_rows_keep_the_icon_when_a_thumbnail_cannot_be_decoded() {
            run_gtk_test(|| {
                let _ = adw::init();
                let decodes = ManualDecodes::install();
                let assets = AssetCacheTestDir::new("files-broken");
                let path = assets.0.join("broken.png");
                std::fs::write(&path, b"not an image").expect("write");
                let mut context = test_context();
                let url = "https://files.slack.com/broken.png".to_string();
                context.image_assets.insert(url.clone(), path);
                let view = show_files(vec![file_row("broken", url)], &context);
                pump_until(500, || !decodes.0.running().is_empty());
                decodes.finish_all();
                pump(20);
                let icon = descendants(view.list_view.upcast_ref())
                    .into_iter()
                    .find(|widget| widget.has_css_class("secondary-file-icon"))
                    .expect("icon");
                assert!(icon.is_visible(), "the file icon stays when decoding fails");
            });
        }

        #[test]
        fn secondary_view_switches_between_status_and_list_gtk() {
            run_gtk_test(|| {
                let _ = adw::init();
                let view = SecondaryView::new();
                let context = test_context();

                view.show_loading(SecondaryKind::Files, "Loading files");
                assert_eq!(
                    view.stack.visible_child_name().as_deref(),
                    Some(STATUS_PAGE)
                );
                assert!(view.status_page.paintable().is_some());

                view.show_rows(SecondaryKind::Threads, sample_rows(), &context);
                assert_eq!(view.stack.visible_child_name().as_deref(), Some(LIST_PAGE));
                assert_eq!(view.store.n_items(), 4);

                view.show_rows(SecondaryKind::Saved, Vec::new(), &context);
                assert_eq!(
                    view.stack.visible_child_name().as_deref(),
                    Some(STATUS_PAGE)
                );
                assert_eq!(view.store.n_items(), 0);
                assert_eq!(
                    view.status_page.title(),
                    SecondaryKind::Saved.empty_state().title
                );
                assert!(view.status_page.paintable().is_none());

                view.show_error(SecondaryKind::Search, "Search results", "offline");
                assert_eq!(view.status_page.description().as_deref(), Some("offline"));
            });
        }

        #[test]
        fn secondary_rows_build_cards_and_dispatch_actions_gtk() {
            run_gtk_test(|| {
                let _ = adw::init();
                let context = test_context();
                let actions = Rc::new(RefCell::new(Vec::new()));
                let sink = actions.clone();
                let dispatch: Dispatch = Rc::new(move |action| sink.borrow_mut().push(action));

                let widgets = sample_rows()
                    .iter()
                    .map(|row| rows::build_row(row, &context, &dispatch))
                    .collect::<Vec<_>>();
                assert!(widgets.iter().all(|widget| widget.is::<adw::Clamp>()));

                assert!(
                    find_button(&widgets[1], "Open in Slack").is_none(),
                    "search results open in Conduit, not in Slack"
                );
                find_button(&widgets[3], "Mark Complete")
                    .expect("saved card can be completed")
                    .emit_clicked();
                let actions = actions.borrow();
                assert!(matches!(
                    &actions[0],
                    SecondaryAction::RemoveSaved { channel_id, ts, .. } if channel_id == "C1" && ts == "4.0"
                ));
            });
        }

        #[test]
        fn list_activation_runs_primary_action_gtk() {
            run_gtk_test(|| {
                let _ = adw::init();
                let view = SecondaryView::new();
                let opened = Rc::new(RefCell::new(None));
                let sink = opened.clone();
                view.set_on_action(move |action| *sink.borrow_mut() = Some(action));
                view.show_rows(SecondaryKind::Threads, sample_rows(), &test_context());

                view.list_view.emit_by_name::<()>("activate", &[&0_u32]);
                assert!(matches!(
                    opened.borrow().as_ref(),
                    Some(SecondaryAction::OpenThread { thread_ts, .. }) if thread_ts == "1.0"
                ));
            });
        }

        #[test]
        fn rerendering_keeps_rows_that_render_the_same() {
            run_gtk_test(|| {
                let view = SecondaryView::new();
                let context = test_context();
                let items = |view: &SecondaryView| {
                    (0..view.store.n_items())
                        .map(|index| view.store.item(index).expect("row"))
                        .collect::<Vec<_>>()
                };
                view.show_rows(SecondaryKind::Threads, sample_rows(), &context);
                let before = items(&view);

                view.show_rows(SecondaryKind::Threads, sample_rows(), &context);
                assert_eq!(items(&view), before);

                let mut rows = sample_rows();
                if let SecondaryRow::Thread(row) = &mut rows[0] {
                    row.reply_count += 1;
                }
                view.show_rows(SecondaryKind::Threads, rows, &context);
                let after = items(&view);
                assert_ne!(after[0], before[0]);
                assert_eq!(after[1..], before[1..]);
            });
        }
    }

    #[test]
    fn splices_cover_everything_between_kept_items() {
        assert_eq!(splices_around(&[(0, 0), (1, 1)], 2, 2), []);
        assert_eq!(
            splices_around(&[(1, 2)], 3, 3),
            [(0, 1, 0..2), (2, 1, 3..3)]
        );
    }
}
