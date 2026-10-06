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

use crate::message_html::MessageHtmlContext;
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
    context: Rc<RefCell<MessageHtmlContext>>,
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
        let context = Rc::new(RefCell::new(MessageHtmlContext::default()));
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
            kind: Rc::new(Cell::new(None)),
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
        context: &MessageHtmlContext,
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
        *self.context.borrow_mut() = context.clone();
        self.kind.set(Some(kind));
        let objects = rows
            .into_iter()
            .map(SecondaryRowObject::new)
            .collect::<Vec<_>>();
        self.store.splice(0, self.store.n_items(), &objects);
        self.stack.set_visible_child_name(LIST_PAGE);
        self.list_view
            .update_property(&[gtk::accessible::Property::Label(&kind.title())]);
        glib::idle_add_local_once(move || {
            adjustment.set_value(preserved_offset.unwrap_or(0.0));
        });
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

            find_button(&widgets[1], "Open in Slack")
                .expect("search card links to Slack")
                .emit_clicked();
            find_button(&widgets[3], "Mark Complete")
                .expect("saved card can be completed")
                .emit_clicked();
            let actions = actions.borrow();
            assert!(
                matches!(&actions[0], SecondaryAction::OpenExternal(url) if url.ends_with("/p3"))
            );
            assert!(matches!(
                &actions[1],
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
}
