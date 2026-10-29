use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;
use gtk::{gio, glib};

use crate::models::SlackUser;
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
    pub model: ConversationPickerModel,
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

/// A person the people picker lists; the lowercased name is computed once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeoplePickerPerson {
    pub user_id: String,
    pub name: String,
    searchable_name: String,
}

impl PeoplePickerPerson {
    pub fn new(user_id: String, name: String) -> Self {
        let searchable_name = name.to_lowercase();
        Self {
            user_id,
            name,
            searchable_name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeoplePickerEntry {
    Person(PeoplePickerPerson),
    Placeholder(String),
}

impl PeoplePickerEntry {
    fn key(&self) -> PickerEntryKey<'_> {
        match self {
            Self::Person(person) => PickerEntryKey::Item(&person.user_id),
            Self::Placeholder(text) => PickerEntryKey::Placeholder(text),
        }
    }
}

/// The entries the people picker shows for `people` and the lowercased,
/// trimmed search `query`: the people whose name contains it, or a
/// placeholder while there is nobody to list.
pub fn people_picker_list_entries(
    people: &[PeoplePickerPerson],
    discovery_complete: bool,
    query: &str,
) -> Vec<PeoplePickerEntry> {
    if people.is_empty() {
        return vec![PeoplePickerEntry::Placeholder(gettext(
            if discovery_complete {
                "No people available"
            } else {
                "Loading people…"
            },
        ))];
    }
    people
        .iter()
        .filter(|person| query.is_empty() || person.searchable_name.contains(query))
        .cloned()
        .map(PeoplePickerEntry::Person)
        .collect()
}

#[derive(Debug, Clone)]
pub struct PeoplePickerView {
    pub model: PeoplePickerModel,
    pub search: gtk::SearchEntry,
    pub confirm: gtk::Button,
    pub excluded_user_ids: HashSet<String>,
    pub discovery_complete: Rc<std::cell::Cell<bool>>,
    /// Everyone currently listed (before the search filter), so an unchanged
    /// directory refresh skips the update and a search filters without a
    /// directory rescan.
    pub shown: Rc<RefCell<Vec<PeoplePickerPerson>>>,
}

/// The `(user id, name)` rows the people picker lists, sorted by name. Each
/// name is lowercased once for the sort, not once per comparison.
pub fn people_picker_entries(
    users: &[SlackUser],
    current_user_id: Option<&str>,
    excluded_user_ids: &HashSet<String>,
) -> Vec<(String, String)> {
    let mut people = users
        .iter()
        .filter(|user| !user.deleted.unwrap_or(false) && !user.is_bot.unwrap_or(false))
        .filter_map(|user| {
            let id = user.id.as_ref()?.trim();
            let name = user.direct_message_name()?;
            (!id.is_empty() && Some(id) != current_user_id && !excluded_user_ids.contains(id))
                .then(|| (id.to_string(), name))
        })
        .collect::<Vec<_>>();
    people.sort_by_cached_key(|(id, name)| (name.to_lowercase(), id.clone()));
    people
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationPickerListEntry {
    Header(String),
    Item(ConversationPickerItem),
    Placeholder(String),
}

impl ConversationPickerListEntry {
    /// Identity of the entry across rebuilds; rows keep it when their
    /// content (unread state, status, avatar) changes.
    fn key(&self) -> PickerEntryKey<'_> {
        match self {
            Self::Header(title) => PickerEntryKey::Header(title),
            Self::Item(item) => PickerEntryKey::Item(&item.row.id),
            Self::Placeholder(text) => PickerEntryKey::Placeholder(text),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PickerEntryKey<'a> {
    Header(&'a str),
    Item(&'a str),
    Placeholder(&'a str),
}

/// Keyed edits that turn the previous picker entries into the next ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PickerEdits {
    /// Structural store edits, applied in order: remove `removed` entries at
    /// `position`, then insert `next[inserted]` there.
    pub splices: Vec<PickerSplice>,
    /// Positions in `next` whose row kept its key and place but changed
    /// content; they are updated in place after the splices.
    pub updates: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerSplice {
    pub position: usize,
    pub removed: usize,
    pub inserted: std::ops::Range<usize>,
}

impl PickerSplice {
    fn end(&self) -> usize {
        self.position + self.inserted.len()
    }
}

fn push_removal(splices: &mut Vec<PickerSplice>, position: usize) {
    match splices.last_mut() {
        Some(last) if last.end() == position => last.removed += 1,
        _ => splices.push(PickerSplice {
            position,
            removed: 1,
            inserted: 0..0,
        }),
    }
}

fn push_insertion(splices: &mut Vec<PickerSplice>, position: usize, index: usize) {
    match splices.last_mut() {
        Some(last) if last.end() == position && last.inserted.end == index => {
            last.inserted.end += 1;
        }
        _ => splices.push(PickerSplice {
            position,
            removed: 0,
            inserted: index..index + 1,
        }),
    }
}

/// Keyed edits that turn `previous` into `next`, touching only entries that
/// were added, removed, moved or changed. Rows that keep their key stay in
/// the store, so the list keeps its scroll anchor and selection. Returns
/// `None` when keys are not unique.
pub fn picker_edits(
    previous: &[ConversationPickerListEntry],
    next: &[ConversationPickerListEntry],
) -> Option<PickerEdits> {
    keyed_edits(previous, next, ConversationPickerListEntry::key)
}

/// [`picker_edits`] for any list whose entries have a stable key.
fn keyed_edits<'a, T: PartialEq, K: Copy + Eq + std::hash::Hash>(
    previous: &'a [T],
    next: &'a [T],
    key: impl Fn(&'a T) -> K,
) -> Option<PickerEdits> {
    let previous_keys = previous.iter().map(&key).collect::<Vec<_>>();
    let next_keys = next.iter().map(&key).collect::<Vec<_>>();
    let next_set = next_keys.iter().copied().collect::<HashSet<_>>();
    let mut remaining = previous_keys.iter().copied().collect::<HashSet<_>>();
    if next_set.len() != next_keys.len() || remaining.len() != previous_keys.len() {
        return None;
    }

    let mut emitted = HashSet::with_capacity(next_keys.len());
    let mut edits = PickerEdits::default();
    let (mut old, mut new, mut position) = (0, 0, 0);
    while old < previous.len() || new < next.len() {
        if let Some(key) = previous_keys.get(old) {
            if !next_set.contains(key) || emitted.contains(key) {
                remaining.remove(key);
                push_removal(&mut edits.splices, position);
                old += 1;
                continue;
            }
        }
        if let Some(key) = next_keys.get(new) {
            if !remaining.contains(key) {
                emitted.insert(*key);
                push_insertion(&mut edits.splices, position, new);
                position += 1;
                new += 1;
                continue;
            }
        }
        // Both entries survive: keep a match, or drop a moved entry here and
        // insert it again where `next` places it.
        let (previous_key, next_key) = (previous_keys[old], next_keys[new]);
        remaining.remove(&previous_key);
        old += 1;
        if previous_key != next_key {
            push_removal(&mut edits.splices, position);
            continue;
        }
        if previous[old - 1] != next[new] {
            edits.updates.push(new);
        }
        emitted.insert(next_key);
        position += 1;
        new += 1;
    }
    Some(edits)
}

type PickerRowWidget = Rc<dyn Fn(&ConversationPickerListEntry) -> gtk::Widget>;

/// The conversation picker list. Rows are virtualized: the row widget is
/// built only for entries scrolled into view, so a rebuild over thousands of
/// discovered channels and people edits model objects instead of creating
/// and destroying one widget per entry.
#[derive(Clone)]
pub struct ConversationPickerModel {
    pub list: gtk::ListView,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    entries: Rc<RefCell<Vec<ConversationPickerListEntry>>>,
    query: Rc<RefCell<Option<String>>>,
    widget: PickerRowWidget,
    /// Containers of the currently bound item rows, by conversation or user
    /// id, so a content update rebuilds just that row's widget.
    bound: Rc<RefCell<HashMap<String, adw::Bin>>>,
}

impl std::fmt::Debug for ConversationPickerModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConversationPickerModel")
            .field("list", &self.list)
            .field("entries", &self.entries.borrow().len())
            .finish_non_exhaustive()
    }
}

impl ConversationPickerModel {
    pub fn new(widget: impl Fn(&ConversationPickerListEntry) -> gtk::Widget + 'static) -> Self {
        let widget: PickerRowWidget = Rc::new(widget);
        let bound = Rc::new(RefCell::new(HashMap::<String, adw::Bin>::new()));
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, object| {
            if let Some(list_item) = object.downcast_ref::<gtk::ListItem>() {
                list_item.set_child(Some(&adw::Bin::new()));
            }
        });
        {
            let widget = widget.clone();
            let bound = bound.clone();
            factory.connect_bind(move |_, object| {
                let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(boxed) = list_item.item().and_downcast::<glib::BoxedAnyObject>() else {
                    return;
                };
                let Some(bin) = list_item.child().and_downcast::<adw::Bin>() else {
                    return;
                };
                let entry = boxed.borrow::<ConversationPickerListEntry>();
                let item_id = match &*entry {
                    ConversationPickerListEntry::Item(item) => Some(item.row.id.clone()),
                    ConversationPickerListEntry::Header(_)
                    | ConversationPickerListEntry::Placeholder(_) => None,
                };
                list_item.set_selectable(item_id.is_some());
                list_item.set_activatable(item_id.is_some());
                bin.set_child(Some(&widget(&entry)));
                if let Some(item_id) = item_id {
                    bound.borrow_mut().insert(item_id, bin);
                }
            });
        }
        {
            let bound = bound.clone();
            factory.connect_unbind(move |_, object| {
                let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(bin) = list_item.child().and_downcast::<adw::Bin>() else {
                    return;
                };
                bin.set_child(None::<&gtk::Widget>);
                bound.borrow_mut().retain(|_, bound_bin| *bound_bin != bin);
            });
        }
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
        list.set_single_click_activate(true);
        list.add_css_class("navigation-sidebar");
        Self {
            list,
            store,
            selection,
            entries: Rc::default(),
            query: Rc::default(),
            widget,
            bound,
        }
    }

    /// Shows `next` for `query`. A new query replaces the list and returns
    /// to the top; a data refresh for the same query edits only the changed
    /// entries and keeps the scroll position and the selected row.
    pub fn show(&self, query: &str, next: Vec<ConversationPickerListEntry>) {
        let same_query = self.query.borrow().as_deref() == Some(query);
        *self.query.borrow_mut() = Some(query.to_string());
        let edits = same_query
            .then(|| picker_edits(&self.entries.borrow(), &next))
            .flatten();
        match edits {
            Some(edits) => self.apply(&edits, next),
            None => self.replace(next),
        }
    }

    fn replace(&self, next: Vec<ConversationPickerListEntry>) {
        let objects = next
            .iter()
            .cloned()
            .map(glib::BoxedAnyObject::new)
            .collect::<Vec<_>>();
        self.store.splice(0, self.store.n_items(), &objects);
        *self.entries.borrow_mut() = next;
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        if self.store.n_items() > 0 {
            self.list.scroll_to(0, gtk::ListScrollFlags::NONE, None);
        }
    }

    fn apply(&self, edits: &PickerEdits, next: Vec<ConversationPickerListEntry>) {
        let selected_id = self.selected_id();
        for splice in &edits.splices {
            let objects = next[splice.inserted.clone()]
                .iter()
                .cloned()
                .map(glib::BoxedAnyObject::new)
                .collect::<Vec<_>>();
            self.store
                .splice(splice.position as u32, splice.removed as u32, &objects);
        }
        // Content changes edit the stored entry and rebuild only a bound
        // row's widget: a store replacement would drop the row the list is
        // anchored on and shift the scroll position.
        for &position in &edits.updates {
            let entry = &next[position];
            if let Some(boxed) = self
                .store
                .item(position as u32)
                .and_downcast::<glib::BoxedAnyObject>()
            {
                *boxed.borrow_mut::<ConversationPickerListEntry>() = entry.clone();
            }
            if let ConversationPickerListEntry::Item(item) = entry {
                if let Some(bin) = self.bound.borrow().get(&item.row.id) {
                    bin.set_child(Some(&(self.widget)(entry)));
                }
            }
        }
        *self.entries.borrow_mut() = next;
        let selected = selected_id
            .and_then(|id| self.position_of(&id))
            .unwrap_or(gtk::INVALID_LIST_POSITION);
        if self.selection.selected() != selected {
            self.selection.set_selected(selected);
        }
    }

    fn selected_id(&self) -> Option<String> {
        let position = usize::try_from(self.selection.selected()).ok()?;
        match self.entries.borrow().get(position)? {
            ConversationPickerListEntry::Item(item) => Some(item.row.id.clone()),
            ConversationPickerListEntry::Header(_)
            | ConversationPickerListEntry::Placeholder(_) => None,
        }
    }

    /// Position of the item row for conversation or user `id`.
    pub fn position_of(&self, id: &str) -> Option<u32> {
        self.entries
            .borrow()
            .iter()
            .position(|entry| entry.key() == PickerEntryKey::Item(id))
            .and_then(|position| u32::try_from(position).ok())
    }

    /// The action for the activatable picker entry at `position`, if any.
    pub fn action_at(&self, position: u32) -> Option<SidebarRowAction> {
        match self.entries.borrow().get(usize::try_from(position).ok()?)? {
            ConversationPickerListEntry::Item(item) => {
                Some(SidebarRowAction::from_picker_item(item))
            }
            ConversationPickerListEntry::Header(_)
            | ConversationPickerListEntry::Placeholder(_) => None,
        }
    }

    #[cfg(test)]
    pub fn selection(&self) -> &gtk::SingleSelection {
        &self.selection
    }

    #[cfg(test)]
    pub fn len(&self) -> u32 {
        self.store.n_items()
    }
}

/// The people picker list. Like the conversation picker it is a virtualized
/// `GtkListView`: a refresh splices only the added and removed people, and
/// the checked people live in a set, not in row widgets, so they stay checked
/// across refreshes, scrolling and searches.
#[derive(Clone)]
pub struct PeoplePickerModel {
    pub list: gtk::ListView,
    store: gio::ListStore,
    entries: Rc<RefCell<Vec<PeoplePickerEntry>>>,
    query: Rc<RefCell<Option<String>>>,
    checked: Rc<RefCell<HashSet<String>>>,
    /// Containers of the currently bound person rows, by user id.
    bound: Rc<RefCell<HashMap<String, adw::Bin>>>,
}

impl std::fmt::Debug for PeoplePickerModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PeoplePickerModel")
            .field("list", &self.list)
            .field("entries", &self.entries.borrow().len())
            .finish_non_exhaustive()
    }
}

fn people_picker_row_widget(
    entry: &PeoplePickerEntry,
    checked: &Rc<RefCell<HashSet<String>>>,
    on_checked_changed: &Rc<dyn Fn(bool)>,
) -> gtk::Widget {
    match entry {
        PeoplePickerEntry::Person(person) => {
            let check = gtk::CheckButton::with_label(&person.name);
            check.set_margin_top(6);
            check.set_margin_bottom(6);
            check.set_margin_start(9);
            check.set_margin_end(9);
            check.set_active(checked.borrow().contains(&person.user_id));
            let (checked, on_checked_changed) = (checked.clone(), on_checked_changed.clone());
            let user_id = person.user_id.clone();
            check.connect_toggled(move |check| {
                if check.is_active() {
                    checked.borrow_mut().insert(user_id.clone());
                } else {
                    checked.borrow_mut().remove(&user_id);
                }
                on_checked_changed(!checked.borrow().is_empty());
            });
            check.upcast()
        }
        PeoplePickerEntry::Placeholder(text) => {
            let label = gtk::Label::new(Some(text));
            label.set_margin_top(12);
            label.set_margin_bottom(12);
            label.set_margin_start(12);
            label.set_margin_end(12);
            label.set_xalign(0.0);
            label.add_css_class("dim-label");
            label.upcast()
        }
    }
}

impl PeoplePickerModel {
    /// `on_checked_changed(any_checked)` runs after a person is checked or
    /// unchecked.
    pub fn new(on_checked_changed: impl Fn(bool) + 'static) -> Self {
        let on_checked_changed: Rc<dyn Fn(bool)> = Rc::new(on_checked_changed);
        let checked = Rc::new(RefCell::new(HashSet::<String>::new()));
        let bound = Rc::new(RefCell::new(HashMap::<String, adw::Bin>::new()));
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, object| {
            if let Some(list_item) = object.downcast_ref::<gtk::ListItem>() {
                list_item.set_selectable(false);
                list_item.set_activatable(false);
                list_item.set_child(Some(&adw::Bin::new()));
            }
        });
        {
            let checked = checked.clone();
            let bound = bound.clone();
            let on_checked_changed = on_checked_changed.clone();
            factory.connect_bind(move |_, object| {
                let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(boxed) = list_item.item().and_downcast::<glib::BoxedAnyObject>() else {
                    return;
                };
                let Some(bin) = list_item.child().and_downcast::<adw::Bin>() else {
                    return;
                };
                let entry = boxed.borrow::<PeoplePickerEntry>();
                bin.set_child(Some(&people_picker_row_widget(
                    &entry,
                    &checked,
                    &on_checked_changed,
                )));
                if let PeoplePickerEntry::Person(person) = &*entry {
                    bound.borrow_mut().insert(person.user_id.clone(), bin);
                }
            });
        }
        {
            let bound = bound.clone();
            factory.connect_unbind(move |_, object| {
                let Some(list_item) = object.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(bin) = list_item.child().and_downcast::<adw::Bin>() else {
                    return;
                };
                bin.set_child(None::<&gtk::Widget>);
                bound.borrow_mut().retain(|_, bound_bin| *bound_bin != bin);
            });
        }
        let list = gtk::ListView::new(
            Some(gtk::NoSelection::new(Some(store.clone()))),
            Some(factory),
        );
        list.add_css_class("navigation-sidebar");
        Self {
            list,
            store,
            entries: Rc::default(),
            query: Rc::default(),
            checked,
            bound,
        }
    }

    /// Shows `next` for the search `query`. A new query replaces the list
    /// and returns to the top; a data refresh for the same query splices
    /// only the people added or removed and keeps the scroll position.
    pub fn show(&self, query: &str, next: Vec<PeoplePickerEntry>) {
        let same_query = self.query.borrow().as_deref() == Some(query);
        *self.query.borrow_mut() = Some(query.to_string());
        let edits = same_query
            .then(|| keyed_edits(&self.entries.borrow(), &next, PeoplePickerEntry::key))
            .flatten();
        match edits {
            Some(edits) => self.apply(&edits, next),
            None => self.replace(next),
        }
    }

    fn replace(&self, next: Vec<PeoplePickerEntry>) {
        let objects = next
            .iter()
            .cloned()
            .map(glib::BoxedAnyObject::new)
            .collect::<Vec<_>>();
        self.store.splice(0, self.store.n_items(), &objects);
        *self.entries.borrow_mut() = next;
        if self.store.n_items() > 0 {
            self.list.scroll_to(0, gtk::ListScrollFlags::NONE, None);
        }
    }

    fn apply(&self, edits: &PickerEdits, next: Vec<PeoplePickerEntry>) {
        for splice in &edits.splices {
            let objects = next[splice.inserted.clone()]
                .iter()
                .cloned()
                .map(glib::BoxedAnyObject::new)
                .collect::<Vec<_>>();
            self.store
                .splice(splice.position as u32, splice.removed as u32, &objects);
        }
        // A renamed person edits the stored entry and the bound row's label
        // in place: replacing the store item would shift the scroll anchor.
        for &position in &edits.updates {
            let entry = &next[position];
            if let Some(boxed) = self
                .store
                .item(position as u32)
                .and_downcast::<glib::BoxedAnyObject>()
            {
                *boxed.borrow_mut::<PeoplePickerEntry>() = entry.clone();
            }
            if let PeoplePickerEntry::Person(person) = entry {
                let check = self
                    .bound
                    .borrow()
                    .get(&person.user_id)
                    .and_then(|bin| bin.child())
                    .and_downcast::<gtk::CheckButton>();
                if let Some(check) = check {
                    check.set_label(Some(&person.name));
                }
            }
        }
        *self.entries.borrow_mut() = next;
    }

    /// Forgets checked people who are no longer listed.
    pub fn retain_checked(&self, listed: &HashSet<String>) {
        self.checked.borrow_mut().retain(|id| listed.contains(id));
    }

    pub fn has_checked(&self) -> bool {
        !self.checked.borrow().is_empty()
    }

    /// The checked people among `people`, in list order.
    pub fn checked_in(&self, people: &[PeoplePickerPerson]) -> Vec<String> {
        let checked = self.checked.borrow();
        people
            .iter()
            .filter(|person| checked.contains(&person.user_id))
            .map(|person| person.user_id.clone())
            .collect()
    }

    #[cfg(test)]
    pub fn bound_check(&self, user_id: &str) -> Option<gtk::CheckButton> {
        let bin = self.bound.borrow().get(user_id).cloned()?;
        bin.child().and_downcast::<gtk::CheckButton>()
    }

    #[cfg(test)]
    pub fn len(&self) -> u32 {
        self.store.n_items()
    }

    #[cfg(test)]
    pub fn bound_count(&self) -> usize {
        self.bound.borrow().len()
    }

    #[cfg(test)]
    pub fn bound_ids(&self) -> Vec<String> {
        let mut ids = self.bound.borrow().keys().cloned().collect::<Vec<_>>();
        ids.sort();
        ids
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
    use crate::models::SlackConversation;
    use crate::sidebar::SidebarRowModel;

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

    fn channel_entries(
        prefix: &str,
        ids: impl IntoIterator<Item = usize>,
    ) -> Vec<ConversationPickerListEntry> {
        std::iter::once(ConversationPickerListEntry::Header("Channels".to_string()))
            .chain(
                ids.into_iter().map(|index| {
                    channel_entry(&format!("C{index:05}"), &format!("{prefix}-{index}"))
                }),
            )
            .collect()
    }

    fn apply_edits(
        previous: &[ConversationPickerListEntry],
        next: &[ConversationPickerListEntry],
        edits: &PickerEdits,
    ) -> Vec<ConversationPickerListEntry> {
        let mut entries = previous.to_vec();
        for splice in &edits.splices {
            entries.splice(
                splice.position..splice.position + splice.removed,
                next[splice.inserted.clone()].iter().cloned(),
            );
        }
        for &position in &edits.updates {
            assert_eq!(entries[position].key(), next[position].key());
            entries[position] = next[position].clone();
        }
        entries
    }

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

    #[test]
    fn perf_guard_picker_refresh_edits_only_changed_entries() {
        let previous = channel_entries("team", 0..2_000);
        let mut next = channel_entries("team", (0..2_000).filter(|index| index % 500 != 7));
        next.insert(300, channel_entry("C90000", "new-a"));
        next.insert(1_200, channel_entry("C90001", "new-b"));
        let renamed = next
            .iter()
            .position(|entry| entry.key() == PickerEntryKey::Item("C00998"))
            .expect("row survives");
        next[renamed] = channel_entry("C00998", "renamed");

        let edits = picker_edits(&previous, &next).expect("keys are unique");
        assert_eq!(apply_edits(&previous, &next, &edits), next);
        let touched = edits
            .splices
            .iter()
            .map(|splice| splice.removed + splice.inserted.len())
            .sum::<usize>();
        assert_eq!(touched, 6, "four removals and two insertions: {edits:?}");
        assert_eq!(edits.updates, vec![renamed]);
    }

    #[test]
    fn people_picker_entries_sort_by_name_and_skip_unlistable_users() {
        let user = |id: &str, name: &str| SlackUser {
            id: Some(id.to_string()),
            real_name: Some(name.to_string()),
            ..Default::default()
        };
        let users = vec![
            user("U3", "carol"),
            user("U1", "Bob"),
            user("U_SELF", "Me"),
            user("U_OUT", "Already there"),
            SlackUser {
                is_bot: Some(true),
                ..user("U_BOT", "Robot")
            },
            SlackUser {
                deleted: Some(true),
                ..user("U_GONE", "Gone")
            },
            user("U2", "alice"),
        ];
        let excluded = HashSet::from(["U_OUT".to_string()]);

        let ids = people_picker_entries(&users, Some("U_SELF"), &excluded)
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["U2", "U1", "U3"]);
    }

    #[test]
    fn picker_edits_handle_moves_and_full_turnover() {
        let previous = channel_entries("team", 0..6);
        let mut next = previous.clone();
        next.swap(1, 5);
        let edits = picker_edits(&previous, &next).expect("keys are unique");
        assert_eq!(apply_edits(&previous, &next, &edits), next);

        let next = channel_entries("other", 10..14);
        let edits = picker_edits(&previous, &next).expect("keys are unique");
        assert_eq!(apply_edits(&previous, &next, &edits), next);

        assert_eq!(
            picker_edits(&previous, &[]).map(|edits| apply_edits(&previous, &[], &edits)),
            Some(Vec::new())
        );
        let duplicate = vec![channel_entry("C1", "a"), channel_entry("C1", "b")];
        assert_eq!(picker_edits(&previous, &duplicate), None);
    }

    fn person(index: usize, name: &str) -> PeoplePickerPerson {
        PeoplePickerPerson::new(format!("U{index:05}"), format!("{name} {index:05}"))
    }

    fn people(name: &str, ids: impl IntoIterator<Item = usize>) -> Vec<PeoplePickerPerson> {
        ids.into_iter().map(|index| person(index, name)).collect()
    }

    fn person_entries(people: &[PeoplePickerPerson], query: &str) -> Vec<PeoplePickerEntry> {
        people_picker_list_entries(people, true, query)
    }

    #[test]
    fn perf_guard_people_refresh_splices_only_changed_people() {
        let previous = person_entries(&people("Team", 0..2_000), "");
        let mut next_people = people("Team", (0..2_000).filter(|index| index % 500 != 7));
        next_people.insert(300, person(90_000, "New"));
        next_people.insert(1_200, person(90_001, "New"));
        let renamed = next_people
            .iter()
            .position(|person| person.user_id == "U00998")
            .expect("person survives");
        next_people[renamed] = PeoplePickerPerson::new("U00998".to_string(), "Renamed".to_string());
        let next = person_entries(&next_people, "");

        let edits = keyed_edits(&previous, &next, PeoplePickerEntry::key).expect("keys are unique");
        let mut applied = previous.clone();
        for splice in &edits.splices {
            applied.splice(
                splice.position..splice.position + splice.removed,
                next[splice.inserted.clone()].iter().cloned(),
            );
        }
        for &position in &edits.updates {
            applied[position] = next[position].clone();
        }
        assert_eq!(applied, next);
        let touched = edits
            .splices
            .iter()
            .map(|splice| splice.removed + splice.inserted.len())
            .sum::<usize>();
        assert_eq!(touched, 6, "four removals and two insertions: {edits:?}");
        assert_eq!(edits.updates, vec![renamed]);
    }

    #[test]
    fn people_picker_list_filters_by_name_and_explains_an_empty_directory() {
        let everyone = vec![
            PeoplePickerPerson::new("U1".to_string(), "Ada Lovelace".to_string()),
            PeoplePickerPerson::new("U2".to_string(), "Grace Hopper".to_string()),
            PeoplePickerPerson::new("U3".to_string(), "Zoë Ada".to_string()),
        ];
        let ids = |query: &str| {
            person_entries(&everyone, query)
                .into_iter()
                .filter_map(|entry| match entry {
                    PeoplePickerEntry::Person(person) => Some(person.user_id),
                    PeoplePickerEntry::Placeholder(_) => None,
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(ids(""), ["U1", "U2", "U3"]);
        assert_eq!(ids("ada"), ["U1", "U3"]);
        assert_eq!(ids("zoë"), ["U3"]);
        assert!(ids("nobody").is_empty());
        assert!(matches!(
            people_picker_list_entries(&[], false, "ada").as_slice(),
            [PeoplePickerEntry::Placeholder(_)]
        ));
        assert_ne!(
            people_picker_list_entries(&[], false, ""),
            people_picker_list_entries(&[], true, ""),
            "loading and empty read differently"
        );
    }

    /// Tests that create GTK widgets. They run only when
    /// `CONDUIT_GRAPHICAL_TESTS` is set (see `run_gtk_test`).
    mod graphical {
        use std::cell::Cell;

        use super::*;
        use crate::timeline::tests::{pump, pump_until, run_gtk_test};

        fn label_model(
            binds: Rc<Cell<usize>>,
            bound_titles: Rc<RefCell<Vec<String>>>,
        ) -> ConversationPickerModel {
            ConversationPickerModel::new(move |entry| {
                binds.set(binds.get() + 1);
                let text = match entry {
                    ConversationPickerListEntry::Item(item) => item.row.title.clone(),
                    ConversationPickerListEntry::Header(text)
                    | ConversationPickerListEntry::Placeholder(text) => text.clone(),
                };
                bound_titles.borrow_mut().push(text.clone());
                gtk::Label::new(Some(&text)).upcast()
            })
        }

        fn picker_window(model: &ConversationPickerModel) -> (gtk::Window, gtk::ScrolledWindow) {
            let scroller = gtk::ScrolledWindow::new();
            scroller.set_child(Some(&model.list));
            let window = gtk::Window::builder()
                .default_width(400)
                .default_height(300)
                .child(&scroller)
                .build();
            (window, scroller)
        }

        #[test]
        fn perf_guard_open_picker_rebuild_binds_only_visible_rows_and_shows_new_data() {
            run_gtk_test(|| {
                let binds = Rc::new(Cell::new(0_usize));
                let bound_titles = Rc::new(RefCell::new(Vec::<String>::new()));
                let model = label_model(binds.clone(), bound_titles.clone());
                let (window, _) = picker_window(&model);

                model.show("", channel_entries("old", 0..5_000));
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
                model.show("", channel_entries("fresh", 0..5_000));
                let fresh_bound = || {
                    bound_titles
                        .borrow()
                        .iter()
                        .any(|title| title.contains("fresh-0"))
                };
                pump_until(5000, fresh_bound);

                assert!(
                    fresh_bound(),
                    "the open picker did not show the rebuilt entries: {:?}",
                    bound_titles.borrow()
                );
                assert!(
                    binds.get() < 500,
                    "a rebuild bound {} rows for 5000 entries",
                    binds.get()
                );
                assert_eq!(model.len(), 5_001);
                assert_eq!(model.action_at(0), None);
                assert_eq!(
                    model.action_at(1).map(|action| action.channel_id),
                    Some("C00000".to_string())
                );
                window.close();
            });
        }

        #[test]
        fn open_picker_data_refresh_keeps_scroll_anchor_and_selection() {
            run_gtk_test(|| {
                let bound_titles = Rc::new(RefCell::new(Vec::<String>::new()));
                let model = label_model(Rc::default(), bound_titles.clone());
                let (window, scroller) = picker_window(&model);
                model.show("", channel_entries("team", 0..2_000));
                window.present();
                pump_until(5000, || window.height() > 0);

                let selected_id = "C01000";
                let selected = model.position_of(selected_id).expect("row exists");
                model
                    .list
                    .scroll_to(selected, gtk::ListScrollFlags::SELECT, None);
                let adjustment = scroller.vadjustment();
                pump_until(5000, || adjustment.value() > 0.0);
                pump(50);
                let value_before = adjustment.value();
                assert!(value_before > 0.0, "the picker never scrolled");
                assert_eq!(model.selection().selected(), selected);

                // Rows come and go above and below the viewport (net zero
                // above it), and the selected row itself changes content.
                let mut next = channel_entries(
                    "team",
                    (0..2_000).filter(|index| ![10, 20, 1_900].contains(index)),
                );
                next.insert(5, channel_entry("C90000", "new-a"));
                next.insert(15, channel_entry("C90001", "new-b"));
                next.insert(1_950, channel_entry("C90002", "new-c"));
                let renamed = next
                    .iter()
                    .position(|entry| entry.key() == PickerEntryKey::Item(selected_id))
                    .expect("selected row survives");
                next[renamed] = channel_entry(selected_id, "renamed");
                model.show("", next);
                pump(100);

                let position = model.position_of(selected_id).expect("row survives");
                assert_eq!(model.selection().selected(), position, "selection lost");
                assert!(
                    bound_titles
                        .borrow()
                        .iter()
                        .any(|title| title.contains("renamed")),
                    "the visible renamed row was not rebuilt"
                );
                assert!(
                    (adjustment.value() - value_before).abs() < 1.0,
                    "scroll moved from {value_before} to {}",
                    adjustment.value()
                );

                model.show("team-1", channel_entries("team", 100..200));
                pump(100);
                assert_eq!(adjustment.value(), 0.0, "a new query starts at the top");
                window.close();
            });
        }

        fn people_picker_window(model: &PeoplePickerModel) -> (gtk::Window, gtk::ScrolledWindow) {
            let scroller = gtk::ScrolledWindow::new();
            scroller.set_child(Some(&model.list));
            let window = gtk::Window::builder()
                .default_width(400)
                .default_height(300)
                .child(&scroller)
                .build();
            (window, scroller)
        }

        #[test]
        fn people_picker_refresh_keeps_checked_people_and_scroll_position() {
            run_gtk_test(|| {
                let any_checked = Rc::new(Cell::new(false));
                let model = PeoplePickerModel::new({
                    let any_checked = any_checked.clone();
                    move |any| any_checked.set(any)
                });
                let (window, scroller) = people_picker_window(&model);
                let everyone = people("Team", 0..2_000);
                model.show("", person_entries(&everyone, ""));
                window.present();
                pump_until(5000, || window.height() > 0);
                pump_until(5000, || model.bound_check("U00001").is_some());
                assert!(
                    model.bound_count() < 500,
                    "{} rows bound for 2000 people",
                    model.bound_count()
                );

                model
                    .bound_check("U00001")
                    .expect("top row is bound")
                    .set_active(true);
                assert!(any_checked.get());
                let adjustment = scroller.vadjustment();
                model
                    .list
                    .scroll_to(1_000, gtk::ListScrollFlags::NONE, None);
                pump_until(5000, || model.bound_check("U00001").is_none());
                pump(50);
                let value_before = adjustment.value();
                assert!(value_before > 0.0, "the picker never scrolled");
                let ids = model.bound_ids();
                let target = ids[ids.len() / 2].clone();
                model
                    .bound_check(&target)
                    .expect("a mid-list row is bound")
                    .set_active(true);

                // People come and go above and below the viewport (net zero
                // above it), and one visible person is renamed.
                let mut next = people(
                    "Team",
                    (0..2_000).filter(|index| ![10, 20, 1_900].contains(index)),
                );
                next.insert(5, person(90_000, "New"));
                next.insert(15, person(90_001, "New"));
                next.insert(1_950, person(90_002, "New"));
                let renamed = next
                    .iter()
                    .position(|person| person.user_id == target)
                    .expect("checked person survives");
                next[renamed] = PeoplePickerPerson::new(target.clone(), "Renamed".to_string());
                model.show("", person_entries(&next, ""));
                pump(100);

                assert_eq!(model.len() as usize, next.len());
                assert!(
                    (adjustment.value() - value_before).abs() < 1.0,
                    "scroll moved from {value_before} to {}",
                    adjustment.value()
                );
                let mut expected = vec!["U00001".to_string(), target.clone()];
                expected.sort();
                assert_eq!(
                    model.checked_in(&next),
                    expected,
                    "checked people were lost"
                );
                let check = model.bound_check(&target).expect("row stays bound");
                assert!(check.is_active(), "the checkbox lost its state");
                assert_eq!(check.label().as_deref(), Some("Renamed"));

                // A person who leaves is no longer checked.
                let remaining = people("Team", 2..2_000);
                model.retain_checked(
                    &remaining
                        .iter()
                        .map(|person| person.user_id.clone())
                        .collect(),
                );
                assert_eq!(model.checked_in(&remaining), std::slice::from_ref(&target));

                // A search replaces the list and a cleared search brings the
                // checked people back checked.
                model.show("team 00001", person_entries(&everyone, "team 00001"));
                pump(100);
                assert_eq!(adjustment.value(), 0.0, "a new query starts at the top");
                model.show("", person_entries(&everyone, ""));
                pump(100);
                let position = everyone
                    .iter()
                    .position(|person| person.user_id == target)
                    .expect("target is listed");
                model
                    .list
                    .scroll_to(position as u32, gtk::ListScrollFlags::NONE, None);
                pump_until(5000, || model.bound_check(&target).is_some());
                assert!(
                    model
                        .bound_check(&target)
                        .is_some_and(|check| check.is_active()),
                    "a person checked before a search is checked after it"
                );
                window.close();
            });
        }
    }
}
