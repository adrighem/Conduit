use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use gettextrs::gettext;

use crate::sidebar::{
    self, ConversationPickerAction, ConversationPickerItem, ConversationPickerSections,
};

pub const PICKER_POPULATION_BATCH_SIZE: usize = 24;

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
    pub list: gtk::ListBox,
    pub search: gtk::SearchEntry,
    pub actions: Rc<RefCell<HashMap<i32, SidebarRowAction>>>,
    pub include_discovery: bool,
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

#[derive(Debug)]
pub struct ConversationPickerPopulation {
    pub generation: u64,
    pub entries: VecDeque<ConversationPickerListEntry>,
}

impl ConversationPickerPopulation {
    pub fn new(generation: u64, entries: VecDeque<ConversationPickerListEntry>) -> Self {
        Self {
            generation,
            entries,
        }
    }

    pub fn next_batch(
        &mut self,
        current_generation: u64,
    ) -> Option<Vec<ConversationPickerListEntry>> {
        if self.generation != current_generation {
            self.entries.clear();
            return None;
        }
        if self.entries.is_empty() {
            return None;
        }
        let batch_size = self.entries.len().min(PICKER_POPULATION_BATCH_SIZE);
        Some(self.entries.drain(..batch_size).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub fn sidebar_row_action_for_index(
    actions: &HashMap<i32, SidebarRowAction>,
    row_index: i32,
) -> Option<SidebarRowAction> {
    actions.get(&row_index).cloned()
}

pub fn picker_sections(
    include_discovery: bool,
    source: sidebar::ConversationPickerSource<'_>,
    query: &str,
) -> ConversationPickerSections {
    let sidebar::ConversationPickerSource {
        conversations,
        discovered_channels,
        discovered_users,
        user_names,
        current_user_id,
        known_user_search_aliases,
        user_full_names,
        user_statuses,
    } = source;
    let channels = if include_discovery {
        discovered_channels
    } else {
        &[]
    };
    let users = if include_discovery {
        discovered_users
    } else {
        &[]
    };
    sidebar::conversation_picker_sections_with_statuses(
        sidebar::ConversationPickerSource {
            conversations,
            discovered_channels: channels,
            discovered_users: users,
            user_names,
            current_user_id,
            known_user_search_aliases,
            user_full_names,
            user_statuses,
        },
        query,
    )
}

pub fn conversation_picker_population_entries(
    sections: &ConversationPickerSections,
) -> VecDeque<ConversationPickerListEntry> {
    let mut entries = VecDeque::new();
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
            entries.push_back(ConversationPickerListEntry::Header(title.to_string()));
            entries.extend(items.iter().cloned().map(ConversationPickerListEntry::Item));
        }
    }
    if entries.is_empty() {
        entries.push_back(ConversationPickerListEntry::Placeholder(gettext(
            "No matching conversations",
        )));
    }
    entries
}
