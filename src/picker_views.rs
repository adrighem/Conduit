use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use gettextrs::gettext;
use gtk::prelude::*;
use gtk::{gio, glib};

use crate::sidebar::{
    self, ConversationPickerAction, ConversationPickerItem, ConversationPickerSections,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRowAction {
    pub channel_id: String,
    pub title: String,
    pub action: ConversationPickerAction,
}

impl SidebarRowAction {
    pub fn from_picker_item(item: &ConversationPickerItem) -> Self {
        Self {
            channel_id: item.row.id.clone(),
            title: item.row.title.clone(),
            action: item.action,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConversationPickerView {
    pub list: gtk::ListView,
    pub store: gio::ListStore,
    pub search: gtk::SearchEntry,
    pub include_discovery: bool,
}

/// Decides when the open conversation picker rebuilds. A rebuild filters
/// and sorts every known and discovered conversation and person, so data
/// changes while the picker is not shown only mark it stale, and it rebuilds
/// once when it is shown again.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PickerRefreshGate {
    stale: bool,
}

impl PickerRefreshGate {
    /// Returns whether to rebuild now; a hidden picker is marked stale instead.
    pub fn request(&mut self, visible: bool) -> bool {
        self.stale = !visible;
        visible
    }

    /// Returns whether a picker that was just shown must rebuild.
    pub fn shown(&mut self) -> bool {
        std::mem::take(&mut self.stale)
    }
}

#[derive(Debug, Clone)]
pub struct PeoplePickerRow {
    pub user_id: String,
    pub searchable_name: String,
    pub check: gtk::CheckButton,
    pub row: gtk::ListBoxRow,
}

#[derive(Debug, Clone)]
pub struct PeoplePickerView {
    pub list: gtk::ListBox,
    pub search: gtk::SearchEntry,
    pub confirm: gtk::Button,
    pub rows: Rc<RefCell<Vec<PeoplePickerRow>>>,
    pub excluded_user_ids: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationPickerListEntry {
    Header(String),
    Item(ConversationPickerItem),
    Placeholder(String),
}

/// Builds the conversation picker list. Rows are virtualized: `widget`
/// runs only for the entries scrolled into view, so a rebuild over thousands
/// of discovered channels and people swaps model objects instead of creating
/// and destroying one widget per entry.
pub fn conversation_picker_list(
    widget: impl Fn(&ConversationPickerListEntry) -> gtk::Widget + 'static,
) -> (gtk::ListView, gio::ListStore) {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_bind(move |_, object| {
        let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(boxed) = list_item.item().and_downcast::<glib::BoxedAnyObject>() else {
            return;
        };
        let entry = boxed.borrow::<ConversationPickerListEntry>();
        let is_item = matches!(*entry, ConversationPickerListEntry::Item(_));
        list_item.set_selectable(is_item);
        list_item.set_activatable(is_item);
        list_item.set_child(Some(&widget(&entry)));
    });
    factory.connect_unbind(|_, object| {
        if let Some(list_item) = object.downcast_ref::<gtk::ListItem>() {
            list_item.set_child(None::<&gtk::Widget>);
        }
    });
    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    let list = gtk::ListView::new(Some(selection), Some(factory));
    list.set_single_click_activate(true);
    list.add_css_class("navigation-sidebar");
    (list, store)
}

/// Replaces every picker entry in one model change.
pub fn set_conversation_picker_entries(
    store: &gio::ListStore,
    entries: Vec<ConversationPickerListEntry>,
) {
    let objects = entries
        .into_iter()
        .map(glib::BoxedAnyObject::new)
        .collect::<Vec<_>>();
    store.splice(0, store.n_items(), &objects);
}

/// The action for the activatable picker entry at `position`, if any.
pub fn conversation_picker_action_at(
    store: &gio::ListStore,
    position: u32,
) -> Option<SidebarRowAction> {
    let boxed = store
        .item(position)
        .and_downcast::<glib::BoxedAnyObject>()?;
    let entry = boxed.borrow::<ConversationPickerListEntry>();
    match &*entry {
        ConversationPickerListEntry::Item(item) => Some(SidebarRowAction::from_picker_item(item)),
        ConversationPickerListEntry::Header(_) | ConversationPickerListEntry::Placeholder(_) => {
            None
        }
    }
}

pub fn picker_sections(
    include_discovery: bool,
    source: sidebar::ConversationPickerSource<'_>,
    query: &str,
) -> ConversationPickerSections {
    let (channels, users): (&[_], &[_]) = if include_discovery {
        (source.discovered_channels, source.discovered_users)
    } else {
        (&[], &[])
    };
    sidebar::conversation_picker_sections_with_statuses(
        sidebar::ConversationPickerSource {
            discovered_channels: channels,
            discovered_users: users,
            ..source
        },
        query,
    )
}

pub fn conversation_picker_population_entries(
    sections: &ConversationPickerSections,
) -> Vec<ConversationPickerListEntry> {
    let mut entries = Vec::new();
    if let Some(results) = sections.search_results.as_deref() {
        entries.extend(
            results
                .iter()
                .cloned()
                .map(ConversationPickerListEntry::Item),
        );
    } else {
        for (title, items) in [
            ("Conversations", sections.conversations.as_slice()),
            ("Channels you can join", sections.channels.as_slice()),
            ("People", sections.people.as_slice()),
        ] {
            if items.is_empty() {
                continue;
            }
            entries.push(ConversationPickerListEntry::Header(title.to_string()));
            entries.extend(items.iter().cloned().map(ConversationPickerListEntry::Item));
        }
    }
    if entries.is_empty() {
        entries.push(ConversationPickerListEntry::Placeholder(gettext(
            "No matching conversations",
        )));
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perf_guard_hidden_picker_does_no_rebuild_work_until_shown() {
        let mut gate = PickerRefreshGate::default();
        let rebuilds = (0..100).filter(|_| gate.request(false)).count();
        assert_eq!(rebuilds, 0, "data updates while hidden must not rebuild");

        assert!(gate.shown(), "a stale picker rebuilds once when shown");
        assert!(!gate.shown(), "the shown rebuild happens only once");
    }

    #[test]
    fn visible_picker_rebuilds_and_stays_fresh() {
        let mut gate = PickerRefreshGate::default();
        assert!(!gate.shown(), "a fresh picker needs no rebuild on show");
        assert!(gate.request(true));
        assert!(!gate.shown());

        assert!(!gate.request(false));
        assert!(gate.request(true), "a visible rebuild clears staleness");
        assert!(!gate.shown());
    }

    /// Tests that create GTK widgets. They run only when
    /// `CONDUIT_GRAPHICAL_TESTS` is set (see `run_gtk_test`).
    mod graphical {
        use std::cell::Cell;
        use std::collections::HashMap;

        use super::*;
        use crate::models::SlackConversation;
        use crate::sidebar::SidebarRowModel;
        use crate::timeline::tests::{pump_until, run_gtk_test};

        fn channel_entry(id: &str, name: &str) -> ConversationPickerListEntry {
            let channel = SlackConversation {
                id: id.to_string(),
                name: Some(name.to_string()),
                is_channel: Some(true),
                ..Default::default()
            };
            ConversationPickerListEntry::Item(ConversationPickerItem {
                row: SidebarRowModel::from_conversation(&channel, &HashMap::new(), None, None),
                action: ConversationPickerAction::JoinChannel,
            })
        }

        fn channel_entries(prefix: &str, count: usize) -> Vec<ConversationPickerListEntry> {
            std::iter::once(ConversationPickerListEntry::Header("Channels".to_string()))
                .chain((0..count).map(|index| {
                    channel_entry(&format!("C{index:05}"), &format!("{prefix}-{index}"))
                }))
                .collect()
        }

        #[test]
        fn perf_guard_open_picker_rebuild_binds_only_visible_rows_and_shows_new_data() {
            run_gtk_test(|| {
                let binds = Rc::new(Cell::new(0_usize));
                let bound_titles = Rc::new(RefCell::new(Vec::<String>::new()));
                let (list, store) = {
                    let binds = binds.clone();
                    let bound_titles = bound_titles.clone();
                    conversation_picker_list(move |entry| {
                        binds.set(binds.get() + 1);
                        let text = match entry {
                            ConversationPickerListEntry::Item(item) => item.row.title.clone(),
                            ConversationPickerListEntry::Header(text)
                            | ConversationPickerListEntry::Placeholder(text) => text.clone(),
                        };
                        bound_titles.borrow_mut().push(text.clone());
                        gtk::Label::new(Some(&text)).upcast()
                    })
                };
                let scroller = gtk::ScrolledWindow::new();
                scroller.set_child(Some(&list));
                let window = gtk::Window::builder()
                    .default_width(400)
                    .default_height(300)
                    .child(&scroller)
                    .build();

                set_conversation_picker_entries(&store, channel_entries("old", 5_000));
                window.present();
                pump_until(5000, || binds.get() > 0);
                assert!(binds.get() > 0, "the open picker never bound a row");
                // GtkListView keeps a couple of hundred rows realized; an
                // eagerly built list creates a widget for every entry.
                assert!(
                    binds.get() < 500,
                    "opening bound {} rows for 5000 entries",
                    binds.get()
                );

                binds.set(0);
                bound_titles.borrow_mut().clear();
                set_conversation_picker_entries(&store, channel_entries("fresh", 5_000));
                pump_until(5000, || {
                    bound_titles
                        .borrow()
                        .iter()
                        .any(|title| title.contains("fresh-0"))
                });

                assert!(
                    bound_titles
                        .borrow()
                        .iter()
                        .any(|title| title.contains("fresh-0")),
                    "the open picker did not show the rebuilt entries: {:?}",
                    bound_titles.borrow()
                );
                assert!(
                    binds.get() < 500,
                    "a rebuild bound {} rows for 5000 entries",
                    binds.get()
                );
                assert_eq!(store.n_items(), 5_001);
                assert_eq!(conversation_picker_action_at(&store, 0), None);
                assert_eq!(
                    conversation_picker_action_at(&store, 1).map(|action| action.channel_id),
                    Some("C00000".to_string())
                );
                window.close();
            });
        }
    }
}
