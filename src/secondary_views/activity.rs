//! The Activity sidebar page: a filter header over a virtualized list.
//!
//! Rows are keyed ([`ActivityKey`]): a refresh keeps the widgets of rows that
//! did not change and splices only the rest, so scroll position and the
//! selected row survive.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;
use gtk::glib::subclass::prelude::*;
use gtk::{gio, glib, pango};

use super::model::{ActivityRow, SecondaryAction};
use super::rows::Dispatch;
use super::{dispatch_action, splices_around, ActionHandler};
use crate::activity::{ActivityFilter, ActivityKey};
use crate::message_markup::MessageRenderContext;
use crate::timeline::cache::decode;
use crate::timeline_message_widget::{resolve_cached_asset_path, TIMELINE_AVATAR_SIZE};

const LIST_PAGE: &str = "list";
const EMPTY_PAGE: &str = "empty";
const SNIPPET_LINES: i32 = 2;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ActivityRowObject {
        pub(crate) row: RefCell<Option<Rc<ActivityRow>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ActivityRowObject {
        const NAME: &'static str = "ConduitActivityRowObject";
        type Type = super::ActivityRowObject;
        type ParentType = glib::Object;
    }

    impl ObjectImpl for ActivityRowObject {}
}

glib::wrapper! {
    pub struct ActivityRowObject(ObjectSubclass<imp::ActivityRowObject>);
}

impl ActivityRowObject {
    fn new(row: ActivityRow) -> Self {
        let object: Self = glib::Object::builder().build();
        *object.imp().row.borrow_mut() = Some(Rc::new(row));
        object
    }

    fn row(&self) -> Option<Rc<ActivityRow>> {
        self.imp().row.borrow().clone()
    }
}

/// Pairs of old and new indices whose old row keeps its widget: same key, in
/// the same relative order, same data, and an avatar that did not change.
fn kept_pairs(
    old: &[Rc<ActivityRow>],
    new: &[ActivityRow],
    avatar_unchanged: &dyn Fn(&ActivityRow) -> bool,
) -> Vec<(usize, usize)> {
    let positions: HashMap<&ActivityKey, usize> = old
        .iter()
        .enumerate()
        .map(|(index, row)| (&row.key, index))
        .collect();
    let mut kept = Vec::new();
    let mut last = None;
    for (new_index, row) in new.iter().enumerate() {
        let Some(&old_index) = positions.get(&row.key) else {
            continue;
        };
        if last.is_none_or(|last| old_index > last)
            && *old[old_index] == *row
            && avatar_unchanged(row)
        {
            kept.push((old_index, new_index));
            last = Some(old_index);
        }
    }
    kept
}

fn register_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            r#"
            .activity-row { padding: 8px 6px; }
            .activity-reaction-chip {
                border-radius: 9999px;
                padding: 0 6px;
                background-color: alpha(currentColor, 0.1);
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

fn caption(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(pango::EllipsizeMode::End);
    label.add_css_class("caption");
    label.add_css_class("dim-label");
    label
}

/// The sender's cached picture when there is one, else initials.
fn avatar(row: &ActivityRow, context: &MessageRenderContext) -> adw::Avatar {
    let avatar = adw::Avatar::new(TIMELINE_AVATAR_SIZE, Some(&row.author), true);
    avatar.set_valign(gtk::Align::Start);
    // Only cached downloads: the URL comes from Slack and must never be
    // opened as a local path.
    if let Some(path) = avatar_path(row, context) {
        decode::set_avatar_image_when_mapped(&avatar, &path);
    }
    avatar
}

fn avatar_path(row: &ActivityRow, context: &MessageRenderContext) -> Option<std::path::PathBuf> {
    let url = context.user_avatar_urls.get(row.author_id.as_deref()?)?;
    resolve_cached_asset_path(url, context)
}

fn build_row(row: &ActivityRow, context: &MessageRenderContext) -> gtk::Widget {
    register_css();
    let card = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    card.add_css_class("activity-row");
    card.append(&avatar(row, context));

    let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
    body.set_hexpand(true);
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let author = gtk::Label::new(Some(&row.author));
    author.set_xalign(0.0);
    author.set_ellipsize(pango::EllipsizeMode::End);
    author.add_css_class("heading");
    header.append(&author);
    if row.others > 0 {
        header.append(&caption(&format!("+{}", row.others)));
    }
    let time = caption(&row.time);
    time.set_hexpand(true);
    time.set_xalign(1.0);
    header.append(&time);
    body.append(&header);

    let snippet = gtk::Label::new(Some(&row.snippet));
    snippet.set_xalign(0.0);
    snippet.set_wrap(true);
    snippet.set_wrap_mode(pango::WrapMode::WordChar);
    snippet.set_lines(SNIPPET_LINES);
    snippet.set_ellipsize(pango::EllipsizeMode::End);
    body.append(&snippet);

    let context_line = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    if row.private {
        let lock = gtk::Image::from_icon_name("channel-secure-symbolic");
        lock.set_pixel_size(12);
        lock.add_css_class("dim-label");
        context_line.append(&lock);
    }
    let place = caption(&row.context);
    place.set_hexpand(true);
    context_line.append(&place);
    if let Some((emoji, count)) = &row.reaction {
        let chip = caption(&format!("{emoji} {count}"));
        chip.add_css_class("activity-reaction-chip");
        context_line.append(&chip);
    }
    body.append(&context_line);
    card.append(&body);

    let trailing: gtk::Widget = if row.unread > 0 {
        let pill = gtk::Label::new(Some(&row.unread.min(99).to_string()));
        pill.add_css_class("mention-badge-pill");
        pill.set_tooltip_text(Some(&gettext("Unread")));
        pill.upcast()
    } else {
        let done = gtk::Image::from_icon_name("object-select-symbolic");
        done.add_css_class("dim-label");
        done.set_tooltip_text(Some(&gettext("Read")));
        done.upcast()
    };
    trailing.set_valign(gtk::Align::Center);
    card.append(&trailing);
    card.upcast()
}

type FilterHandler = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

#[derive(Clone)]
pub(crate) struct ActivityView {
    root: gtk::Box,
    pub(super) mentions_toggle: gtk::ToggleButton,
    pub(super) threads_toggle: gtk::ToggleButton,
    pub(super) unread_switch: gtk::Switch,
    pub(super) mention_count: gtk::Label,
    pub(super) thread_count: gtk::Label,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) list_view: gtk::ListView,
    stack: gtk::Stack,
    pub(super) store: gio::ListStore,
    context: Rc<RefCell<MessageRenderContext>>,
    on_action: ActionHandler,
    on_filter_changed: FilterHandler,
    /// Set while the toggles are updated in code, so they raise no change.
    syncing: Rc<Cell<bool>>,
}

impl std::fmt::Debug for ActivityView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActivityView")
            .field("rows", &self.store.n_items())
            .finish()
    }
}

impl ActivityView {
    pub(crate) fn new() -> Self {
        let store = gio::ListStore::new::<ActivityRowObject>();
        let context = Rc::new(RefCell::new(MessageRenderContext::default()));
        let on_action: ActionHandler = Rc::new(RefCell::new(None));
        let dispatch: Dispatch = {
            let on_action = on_action.clone();
            Rc::new(move |action| dispatch_action(&on_action, action))
        };

        let title = gtk::Label::new(Some(&gettext("Activity")));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.add_css_class("title-4");
        let (mentions_toggle, mention_count) = count_toggle(&gtk::Label::new(Some("@")));
        let (threads_toggle, thread_count) =
            count_toggle(&gtk::Image::from_icon_name("mail-replied-symbolic"));
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.append(&title);
        header.append(&mentions_toggle);
        header.append(&threads_toggle);

        let unread_label = gtk::Label::new(Some(&gettext("Unread messages")));
        unread_label.set_xalign(0.0);
        unread_label.set_hexpand(true);
        let unread_switch = gtk::Switch::new();
        unread_switch.set_valign(gtk::Align::Center);
        unread_label.set_mnemonic_widget(Some(&unread_switch));
        unread_switch.update_relation(&[gtk::accessible::Relation::LabelledBy(&[
            unread_label.upcast_ref()
        ])]);
        let unread_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        unread_row.append(&unread_label);
        unread_row.append(&unread_switch);

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                item.set_child(Some(&adw::Bin::new()));
            }
        });
        {
            let context = context.clone();
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(row) = item
                    .item()
                    .and_downcast::<ActivityRowObject>()
                    .and_then(|object| object.row())
                else {
                    return;
                };
                if let Some(bin) = item.child().and_downcast::<adw::Bin>() {
                    item.set_accessible_label(&row.accessible_label());
                    bin.set_child(Some(&build_row(&row, &context.borrow())));
                }
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
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let list_view = gtk::ListView::new(Some(selection), Some(factory));
        list_view.set_single_click_activate(true);
        list_view.set_tab_behavior(gtk::ListTabBehavior::Item);
        list_view.add_css_class("navigation-sidebar");
        list_view.update_property(&[gtk::accessible::Property::Label(&gettext("Activity"))]);
        {
            let store = store.clone();
            list_view.connect_activate(move |_, position| {
                let row = store
                    .item(position)
                    .and_downcast::<ActivityRowObject>()
                    .and_then(|object| object.row());
                if let Some(row) = row {
                    dispatch(row.action());
                }
            });
        }
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list_view)
            .build();
        let empty = adw::StatusPage::builder()
            .icon_name("emblem-important-symbolic")
            .title(gettext("No Activity"))
            .description(gettext("Mentions, reactions and threads will appear here"))
            .vexpand(true)
            .build();
        empty.add_css_class("compact");
        let stack = gtk::Stack::new();
        stack.add_named(&scrolled, Some(LIST_PAGE));
        stack.add_named(&empty, Some(EMPTY_PAGE));
        stack.set_visible_child_name(EMPTY_PAGE);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.append(&header);
        root.append(&unread_row);
        root.append(&stack);

        let view = Self {
            root,
            mentions_toggle,
            threads_toggle,
            unread_switch,
            mention_count,
            thread_count,
            list_view,
            stack,
            store,
            context,
            on_action,
            on_filter_changed: Rc::new(RefCell::new(None)),
            syncing: Rc::new(Cell::new(false)),
        };
        view.connect_filter_signals();
        view
    }

    /// The two toggles are exclusive but may both be off; the switch is
    /// independent. Any user change reports through the filter callback.
    fn connect_filter_signals(&self) {
        for (toggle, other) in [
            (&self.mentions_toggle, &self.threads_toggle),
            (&self.threads_toggle, &self.mentions_toggle),
        ] {
            let other = other.clone();
            let view = self.clone();
            toggle.connect_toggled(move |toggle| {
                if view.syncing.get() {
                    return;
                }
                if toggle.is_active() {
                    view.syncing.set(true);
                    other.set_active(false);
                    view.syncing.set(false);
                }
                view.filter_changed();
            });
        }
        let view = self.clone();
        self.unread_switch
            .connect_active_notify(move |_| view.filter_changed());
    }

    fn filter_changed(&self) {
        let handler = self.on_filter_changed.borrow().clone();
        if let Some(handler) = handler {
            handler();
        }
    }

    pub(crate) fn widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }

    pub(crate) fn set_on_action<F: Fn(SecondaryAction) + 'static>(&self, handler: F) {
        *self.on_action.borrow_mut() = Some(Rc::new(handler));
    }

    pub(crate) fn set_on_filter_changed<F: Fn() + 'static>(&self, handler: F) {
        *self.on_filter_changed.borrow_mut() = Some(Rc::new(handler));
    }

    pub(crate) fn filter(&self) -> ActivityFilter {
        if self.mentions_toggle.is_active() {
            ActivityFilter::Mentions
        } else if self.threads_toggle.is_active() {
            ActivityFilter::Threads
        } else {
            ActivityFilter::All
        }
    }

    pub(crate) fn unread_only(&self) -> bool {
        self.unread_switch.is_active()
    }

    /// Unread mention and thread counts on the header toggles.
    pub(crate) fn set_counts(&self, mentions: usize, threads: usize) {
        for (toggle, label, count, name) in [
            (
                &self.mentions_toggle,
                &self.mention_count,
                mentions,
                gettext("Mentions"),
            ),
            (
                &self.threads_toggle,
                &self.thread_count,
                threads,
                gettext("Threads"),
            ),
        ] {
            label.set_visible(count > 0);
            label.set_label(&count.to_string());
            let description = if count > 0 {
                gettext("{name}, {count} unread")
                    .replace("{name}", &name)
                    .replace("{count}", &count.to_string())
            } else {
                name
            };
            toggle.set_tooltip_text(Some(&description));
            toggle.update_property(&[gtk::accessible::Property::Label(&description)]);
        }
    }

    /// Replaces the rows with `rows`, touching only those that changed.
    pub(crate) fn show_rows(&self, rows: Vec<ActivityRow>, context: &MessageRenderContext) {
        let previous = self.context.replace(context.clone());
        let old = (0..self.store.n_items())
            .filter_map(|index| self.store.item(index).and_downcast::<ActivityRowObject>())
            .filter_map(|object| object.row())
            .collect::<Vec<_>>();
        let avatar_unchanged =
            |row: &ActivityRow| avatar_path(row, &previous) == avatar_path(row, context);
        let kept = kept_pairs(&old, &rows, &avatar_unchanged);
        let objects = rows
            .into_iter()
            .map(ActivityRowObject::new)
            .collect::<Vec<_>>();
        for (position, removed, range) in splices_around(&kept, old.len(), objects.len())
            .into_iter()
            .rev()
        {
            self.store
                .splice(position as u32, removed as u32, &objects[range]);
        }
        self.stack
            .set_visible_child_name(if self.store.n_items() == 0 {
                EMPTY_PAGE
            } else {
                LIST_PAGE
            });
    }
}

/// A flat toggle with `icon` and a count label that is hidden at zero.
fn count_toggle(icon: &impl IsA<gtk::Widget>) -> (gtk::ToggleButton, gtk::Label) {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    content.append(icon);
    let count = gtk::Label::new(None);
    count.set_visible(false);
    count.add_css_class("caption-heading");
    content.append(&count);
    let toggle = gtk::ToggleButton::new();
    toggle.set_child(Some(&content));
    toggle.add_css_class("flat");
    (toggle, count)
}

#[cfg(test)]
pub(super) mod tests;
