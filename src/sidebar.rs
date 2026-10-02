use std::collections::{HashMap, HashSet};

use crate::models::{SlackConversation, SlackUser, SlackUserStatus};
use crate::search::{
    MatchScore, SearchField, SearchQuery, ID_FIELD_WEIGHT, PRIMARY_FIELD_WEIGHT,
    SECONDARY_FIELD_WEIGHT,
};
use serde_json::Value;

pub type UserSearchAliases = HashMap<String, Vec<String>>;
pub type UserStatuses = HashMap<String, SlackUserStatus>;

// Activity timestamps can remain on hundreds of old DMs, so keep only a small
// history projection while selected and explicitly active DMs stay uncapped.
const RECENT_HISTORY_DIRECT_MESSAGE_LIMIT: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationKind {
    PublicChannel,
    PrivateChannel,
    DirectMessage,
    GroupDirectMessage,
    Unknown,
}

impl ConversationKind {
    pub fn icon_name(self) -> &'static str {
        match self {
            Self::PublicChannel => "channel-public-symbolic",
            Self::PrivateChannel => "channel-secure-symbolic",
            Self::DirectMessage => "avatar-default-symbolic",
            Self::GroupDirectMessage => "system-users-symbolic",
            Self::Unknown => "dialog-question-symbolic",
        }
    }

    pub fn accessible_name(self) -> &'static str {
        match self {
            Self::PublicChannel => "Public channel",
            Self::PrivateChannel => "Private channel",
            Self::DirectMessage => "Direct message",
            Self::GroupDirectMessage => "Group direct message",
            Self::Unknown => "Conversation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SidebarSectionKind {
    Priority,
    Channels,
    DirectMessages,
    Other,
}

impl SidebarSectionKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Priority => "Priority",
            Self::Channels => "Channels",
            Self::DirectMessages => "Direct messages",
            Self::Other => "Other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarRowModel {
    pub id: String,
    pub title: String,
    pub kind: ConversationKind,
    pub unread: bool,
    pub unread_count: u64,
    pub has_mention: bool,
    pub mention_count: u64,
    pub selected: bool,
    pub starred: bool,
    pub private: bool,
    pub muted: bool,
    pub external: bool,
    pub huddle_active: bool,
    pub user_deleted: bool,
    pub search_aliases: Vec<String>,
    pub status: Option<SlackUserStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationPickerAction {
    OpenConversation,
    JoinChannel,
    OpenDirectMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationPickerItem {
    pub row: SidebarRowModel,
    pub action: ConversationPickerAction,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationPickerSections {
    pub conversations: Vec<ConversationPickerItem>,
    pub channels: Vec<ConversationPickerItem>,
    pub people: Vec<ConversationPickerItem>,
    pub search_results: Option<Vec<ConversationPickerItem>>,
}

impl SidebarRowModel {
    pub fn unread_badge_label(&self) -> Option<String> {
        if matches!(
            self.kind,
            ConversationKind::PublicChannel | ConversationKind::PrivateChannel
        ) {
            return None;
        }
        match self.unread_count {
            0 => None,
            1..=99 => Some(self.unread_count.to_string()),
            _ => Some("99+".to_string()),
        }
    }

    pub fn mention_badge_label(&self) -> Option<String> {
        if !self.has_mention && self.mention_count == 0 {
            return None;
        }
        match self.mention_count {
            0 => Some("@".to_string()),
            1..=99 => Some(self.mention_count.to_string()),
            _ => Some("99+".to_string()),
        }
    }

    pub fn accessible_label(&self) -> String {
        let mut label = format!("{}: {}", self.kind.accessible_name(), self.title);
        if self.mention_count == 1 {
            label.push_str(", 1 mention");
        } else if self.mention_count > 1 {
            label.push_str(&format!(", {} mentions", self.mention_count));
        } else if self.has_mention {
            label.push_str(", mentioned");
        }
        if self.unread_count == 1 {
            label.push_str(", 1 unread");
        } else if self.unread_count > 1 {
            label.push_str(&format!(", {} unread", self.unread_count));
        } else if self.unread {
            label.push_str(", unread");
        }
        if self.selected {
            label.push_str(", selected");
        }
        if self.starred {
            label.push_str(", starred");
        }
        if self.muted {
            label.push_str(", muted");
        }
        if self.external {
            label.push_str(", external");
        }
        if self.huddle_active {
            label.push_str(", huddle active");
        }
        if let Some(status) = self.status.as_ref() {
            label.push_str(&format!(", status: {}", status.accessible_text()));
        }
        label
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarSectionModel {
    pub kind: SidebarSectionKind,
    pub title: &'static str,
    pub rows: Vec<SidebarRowModel>,
}

impl SidebarSectionModel {
    pub fn display_title(&self) -> String {
        self.title.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SidebarPlaceholder {
    Loading,
    LoadFailed,
    Empty,
    NoMatches,
}

impl SidebarPlaceholder {
    pub fn label(self) -> &'static str {
        match self {
            Self::Loading => "Loading conversations",
            Self::LoadFailed => "Could not load conversations",
            Self::Empty => "No conversations",
            Self::NoMatches => "No matching conversations",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarListModel {
    Placeholder(SidebarPlaceholder),
    Sections(Vec<SidebarSectionModel>),
    Rows(Vec<SidebarRowModel>),
}

/// Stable identity for an item rendered in the conversation sidebar.
///
/// A conversation can occur in more than one section, so the section is part
/// of its identity. Search results have no section. Keeping this identity
/// separate from the row contents lets the UI update an existing widget when
/// only selection or status data changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SidebarItemKey {
    Placeholder(SidebarPlaceholder),
    SectionHeader(SidebarSectionKind),
    Conversation {
        section: Option<SidebarSectionKind>,
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarItemModel {
    Placeholder(SidebarPlaceholder),
    SectionHeader {
        kind: SidebarSectionKind,
        title: String,
        collapsed: bool,
    },
    Conversation(SidebarRowModel),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedSidebarItem {
    pub key: SidebarItemKey,
    pub model: SidebarItemModel,
}

/// Incremental operations over the projection's current keyed item sequence.
/// Positions refer to the new sequence returned by [`SidebarProjection::items`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarProjectionOperation {
    Reset,
    Splice {
        position: usize,
        removed: usize,
        inserted: usize,
    },
    Update {
        position: usize,
    },
}

/// Pure keyed projection consumed by the GTK sidebar model.
#[derive(Debug, Default)]
pub struct SidebarProjection {
    items: Vec<KeyedSidebarItem>,
    conversation_positions: HashMap<String, Vec<usize>>,
}

impl SidebarProjection {
    pub fn items(&self) -> &[KeyedSidebarItem] {
        &self.items
    }

    pub fn reconcile(&mut self, next: &[KeyedSidebarItem]) -> Vec<SidebarProjectionOperation> {
        if self.items == next {
            return Vec::new();
        }

        let operations = if self.items.is_empty()
            || !keyed_sidebar_items_are_unique(&self.items)
            || !keyed_sidebar_items_are_unique(next)
        {
            (!next.is_empty())
                .then_some(SidebarProjectionOperation::Reset)
                .into_iter()
                .collect()
        } else if self.items.len() == next.len()
            && self
                .items
                .iter()
                .zip(next)
                .all(|(previous, next)| previous.key == next.key)
        {
            self.items
                .iter()
                .zip(next)
                .enumerate()
                .filter(|(_, (previous, next))| previous.model != next.model)
                .map(|(position, _)| SidebarProjectionOperation::Update { position })
                .collect()
        } else {
            sidebar_projection_structural_operations(&self.items, next)
        };

        self.items = next.to_vec();
        self.rebuild_conversation_positions();
        operations
    }

    /// Replaces every visible occurrence of the supplied conversations when
    /// their section membership and ordering inputs are unchanged.
    ///
    /// Returning `None` asks the caller to rebuild the complete projection.
    pub fn update_conversation_rows(
        &mut self,
        next_rows: &[SidebarRowModel],
    ) -> Option<Vec<SidebarProjectionOperation>> {
        let mut requested_ids = HashSet::with_capacity(next_rows.len());
        let mut updates = Vec::new();
        for next in next_rows {
            if !requested_ids.insert(next.id.as_str()) {
                return None;
            }
            let positions = self.conversation_positions.get(next.id.as_str())?;
            for &position in positions {
                let item = self.items.get(position)?;
                let SidebarItemKey::Conversation { id, .. } = &item.key else {
                    return None;
                };
                let SidebarItemModel::Conversation(previous) = &item.model else {
                    return None;
                };
                if id != &next.id || !sidebar_row_can_update_in_place(previous, next) {
                    return None;
                }
                if previous != next {
                    updates.push((position, next));
                }
            }
        }

        let mut operations = Vec::with_capacity(updates.len());
        for (position, next) in updates {
            self.items[position].model = SidebarItemModel::Conversation(next.clone());
            operations.push(SidebarProjectionOperation::Update { position });
        }
        Some(operations)
    }

    fn rebuild_conversation_positions(&mut self) {
        self.conversation_positions.clear();
        for (position, item) in self.items.iter().enumerate() {
            if let SidebarItemKey::Conversation { id, .. } = &item.key {
                self.conversation_positions
                    .entry(id.clone())
                    .or_default()
                    .push(position);
            }
        }
    }
}

fn sidebar_row_can_update_in_place(previous: &SidebarRowModel, next: &SidebarRowModel) -> bool {
    previous.id == next.id
        && previous.title == next.title
        && previous.kind == next.kind
        && previous.selected == next.selected
        && previous.starred == next.starred
        && previous.user_deleted == next.user_deleted
        && previous.search_aliases == next.search_aliases
}

fn keyed_sidebar_items_are_unique(items: &[KeyedSidebarItem]) -> bool {
    let mut keys = HashSet::with_capacity(items.len());
    items.iter().all(|item| keys.insert(&item.key))
}

fn sidebar_projection_structural_operations(
    previous: &[KeyedSidebarItem],
    next: &[KeyedSidebarItem],
) -> Vec<SidebarProjectionOperation> {
    let prefix = previous
        .iter()
        .zip(next)
        .take_while(|(previous, next)| previous.key == next.key)
        .count();
    let suffix = previous[prefix..]
        .iter()
        .rev()
        .zip(next[prefix..].iter().rev())
        .take_while(|(previous, next)| previous.key == next.key)
        .count();
    let mut operations = vec![SidebarProjectionOperation::Splice {
        position: prefix,
        removed: previous.len() - prefix - suffix,
        inserted: next.len() - prefix - suffix,
    }];

    operations.extend(
        previous[..prefix]
            .iter()
            .zip(&next[..prefix])
            .enumerate()
            .filter(|(_, (previous, next))| previous.model != next.model)
            .map(|(position, _)| SidebarProjectionOperation::Update { position }),
    );

    let previous_suffix = &previous[previous.len() - suffix..];
    let next_suffix_start = next.len() - suffix;
    operations.extend(
        previous_suffix
            .iter()
            .zip(&next[next_suffix_start..])
            .enumerate()
            .filter(|(_, (previous, next))| previous.model != next.model)
            .map(|(offset, _)| SidebarProjectionOperation::Update {
                position: next_suffix_start + offset,
            }),
    );

    operations
}

impl SidebarListModel {
    #[cfg(test)]
    pub fn keyed_items(&self) -> Vec<KeyedSidebarItem> {
        self.keyed_items_with_collapsed_sections(&HashSet::new())
    }

    pub fn keyed_items_with_collapsed_sections(
        &self,
        collapsed_sections: &HashSet<SidebarSectionKind>,
    ) -> Vec<KeyedSidebarItem> {
        match self {
            Self::Placeholder(placeholder) => vec![KeyedSidebarItem {
                key: SidebarItemKey::Placeholder(*placeholder),
                model: SidebarItemModel::Placeholder(*placeholder),
            }],
            Self::Sections(sections) => sections
                .iter()
                .flat_map(|section| {
                    let header = KeyedSidebarItem {
                        key: SidebarItemKey::SectionHeader(section.kind),
                        model: SidebarItemModel::SectionHeader {
                            kind: section.kind,
                            title: section.display_title(),
                            collapsed: collapsed_sections.contains(&section.kind),
                        },
                    };
                    let rows = if collapsed_sections.contains(&section.kind) {
                        &[]
                    } else {
                        section.rows.as_slice()
                    };
                    std::iter::once(header).chain(rows.iter().cloned().map(|row| {
                        KeyedSidebarItem {
                            key: SidebarItemKey::Conversation {
                                section: Some(section.kind),
                                id: row.id.clone(),
                            },
                            model: SidebarItemModel::Conversation(row),
                        }
                    }))
                })
                .collect(),
            Self::Rows(rows) => rows
                .iter()
                .cloned()
                .map(|row| KeyedSidebarItem {
                    key: SidebarItemKey::Conversation {
                        section: None,
                        id: row.id.clone(),
                    },
                    model: SidebarItemModel::Conversation(row),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SidebarBuildOptions<'a> {
    pub selected_channel: Option<&'a str>,
    pub active_huddle_channel_id: Option<&'a str>,
    pub current_user_id: Option<&'a str>,
    pub query: &'a str,
    pub show_all: bool,
    pub loading: bool,
    pub has_error: bool,
    pub user_search_aliases: Option<&'a UserSearchAliases>,
    pub user_full_names: Option<&'a HashMap<String, String>>,
    pub user_statuses: Option<&'a UserStatuses>,
}

#[derive(Debug, Clone, Copy, Default)]
struct SidebarRowOptions<'a> {
    selected_channel: Option<&'a str>,
    current_user_id: Option<&'a str>,
    active_huddle_channel_id: Option<&'a str>,
    user_search_aliases: Option<&'a UserSearchAliases>,
    user_full_names: Option<&'a HashMap<String, String>>,
    user_statuses: Option<&'a UserStatuses>,
}

impl SidebarRowModel {
    pub fn from_conversation(
        conversation: &SlackConversation,
        user_names: &HashMap<String, String>,
        selected_channel: Option<&str>,
        current_user_id: Option<&str>,
    ) -> Self {
        Self::from_conversation_with_aliases(
            conversation,
            user_names,
            SidebarRowOptions {
                selected_channel,
                current_user_id,
                ..Default::default()
            },
        )
    }

    fn from_conversation_with_aliases(
        conversation: &SlackConversation,
        user_names: &HashMap<String, String>,
        options: SidebarRowOptions<'_>,
    ) -> Self {
        let kind = conversation_kind(conversation);
        let empty_full_names = HashMap::new();
        let user_full_names = options.user_full_names.unwrap_or(&empty_full_names);
        let search_aliases = conversation_user_ids(conversation, options.current_user_id)
            .into_iter()
            .filter_map(|user_id| options.user_search_aliases?.get(&user_id))
            .flatten()
            .cloned()
            .collect();
        let muted = conversation.is_muted_conversation();
        let unread = conversation.has_unread_activity() && !muted;
        let unread_count = if muted {
            0
        } else {
            conversation.unread_activity_count()
        };
        Self {
            id: conversation.id.clone(),
            title: conversation.navigation_name_with_users(
                user_names,
                user_full_names,
                options.current_user_id,
            ),
            kind,
            unread,
            unread_count,
            has_mention: conversation.has_mention_activity(),
            mention_count: conversation.mention_activity_count(),
            selected: options.selected_channel == Some(conversation.id.as_str()),
            starred: conversation.is_starred(),
            private: conversation.is_private.unwrap_or(false)
                || conversation.is_group.unwrap_or(false)
                || matches!(kind, ConversationKind::PrivateChannel),
            muted,
            external: conversation.is_external_conversation(),
            huddle_active: options.active_huddle_channel_id == Some(conversation.id.as_str()),
            user_deleted: kind == ConversationKind::DirectMessage && conversation.is_user_deleted(),
            search_aliases,
            status: (kind == ConversationKind::DirectMessage)
                .then_some(conversation.user.as_deref())
                .flatten()
                .and_then(|user_id| active_user_status(options.user_statuses, user_id)),
        }
    }

    fn match_score(&self, query: &SearchQuery) -> Option<MatchScore> {
        query.score(
            [
                SearchField::new(self.title.as_str(), PRIMARY_FIELD_WEIGHT),
                SearchField::new(self.id.as_str(), ID_FIELD_WEIGHT),
            ]
            .into_iter()
            .chain(
                self.search_aliases
                    .iter()
                    .map(|alias| SearchField::new(alias.as_str(), SECONDARY_FIELD_WEIGHT)),
            ),
        )
    }
}

pub(crate) fn sidebar_row_for_conversation(
    conversation: &SlackConversation,
    user_names: &HashMap<String, String>,
    options: SidebarBuildOptions<'_>,
) -> SidebarRowModel {
    SidebarRowModel::from_conversation_with_aliases(
        conversation,
        user_names,
        SidebarRowOptions {
            selected_channel: options.selected_channel,
            current_user_id: options.current_user_id,
            active_huddle_channel_id: options.active_huddle_channel_id,
            user_search_aliases: options.user_search_aliases,
            user_full_names: options.user_full_names,
            user_statuses: options.user_statuses,
        },
    )
}

pub fn user_search_aliases(users: &[SlackUser]) -> UserSearchAliases {
    users
        .iter()
        .filter_map(|user| Some((user.id.clone()?, user.search_aliases())))
        .collect()
}

fn conversation_user_ids(
    conversation: &SlackConversation,
    current_user_id: Option<&str>,
) -> Vec<String> {
    if conversation.is_im.unwrap_or(false) {
        return conversation.user.iter().cloned().collect();
    }
    if conversation.is_mpim.unwrap_or(false) {
        return conversation
            .group_direct_message_user_ids()
            .into_iter()
            .filter(|user_id| Some(user_id.as_str()) != current_user_id)
            .collect();
    }
    Vec::new()
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}

fn active_user_status(statuses: Option<&UserStatuses>, user_id: &str) -> Option<SlackUserStatus> {
    statuses?
        .get(user_id)
        .filter(|status| status.active_at(current_unix_seconds()))
        .cloned()
}

pub fn build_sidebar_list<'a, I>(
    conversations: I,
    user_names: &HashMap<String, String>,
    options: SidebarBuildOptions<'_>,
) -> SidebarListModel
where
    I: IntoIterator<Item = &'a SlackConversation>,
    I::IntoIter: Clone,
{
    let conversations = conversations.into_iter();
    let is_empty = conversations.clone().next().is_none();
    if options.loading && is_empty {
        return SidebarListModel::Placeholder(SidebarPlaceholder::Loading);
    }

    if options.has_error && is_empty {
        return SidebarListModel::Placeholder(SidebarPlaceholder::LoadFailed);
    }

    if is_empty {
        return SidebarListModel::Placeholder(SidebarPlaceholder::Empty);
    }

    let query = SearchQuery::parse(options.query);
    let recent_history_direct_messages = if options.show_all {
        HashSet::new()
    } else {
        recent_history_direct_message_ids(conversations.clone(), options.selected_channel)
    };
    let mut rows = conversations
        .clone()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
        .filter(|conversation| {
            options.show_all
                || options.selected_channel == Some(conversation.id.as_str())
                || conversation_kind(conversation) != ConversationKind::Unknown
        })
        .filter(|conversation| {
            options.show_all
                || conversation_visible_in_default_sidebar(
                    conversation,
                    options.selected_channel,
                    recent_history_direct_messages.contains(&conversation.id),
                )
        })
        .map(|conversation| sidebar_row_for_conversation(conversation, user_names, options))
        .filter(|row| row.match_score(&query).is_some())
        .collect::<Vec<_>>();

    if rows.is_empty() {
        return SidebarListModel::Placeholder(SidebarPlaceholder::NoMatches);
    }

    if !query.is_empty() {
        let participant_coverage = conversation_participant_coverage(
            conversations,
            user_names,
            options.current_user_id,
            &query,
            options.user_search_aliases,
        );
        sort_search_rows(&mut rows, &query, &participant_coverage);
        return SidebarListModel::Rows(rows);
    }

    SidebarListModel::Sections(build_sidebar_sections_from_rows(rows, None))
}

#[cfg(test)]
fn build_sidebar_sections(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    selected_channel: Option<&str>,
) -> Vec<SidebarSectionModel> {
    build_sidebar_sections_from_rows(
        conversations
            .iter()
            .filter(|conversation| !conversation.is_archived.unwrap_or(false))
            .map(|conversation| {
                SidebarRowModel::from_conversation(conversation, user_names, selected_channel, None)
            }),
        None,
    )
}

#[cfg(test)]
pub fn conversation_switcher_items(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
    query: &str,
) -> Vec<SidebarRowModel> {
    conversation_switcher_items_with_aliases(
        conversations,
        user_names,
        current_user_id,
        query,
        None,
        None,
        None,
    )
}

pub(crate) fn conversation_switcher_items_with_aliases(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
    query: &str,
    user_search_aliases: Option<&UserSearchAliases>,
    user_full_names: Option<&HashMap<String, String>>,
    user_statuses: Option<&UserStatuses>,
) -> Vec<SidebarRowModel> {
    let rows = conversation_switcher_rows_with_aliases(
        conversations,
        user_names,
        current_user_id,
        user_search_aliases,
        user_full_names,
        user_statuses,
    );
    filter_conversation_switcher_rows(
        &rows,
        conversations,
        user_names,
        current_user_id,
        query,
        user_search_aliases,
    )
}

pub(crate) fn conversation_switcher_rows_with_aliases(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
    user_search_aliases: Option<&UserSearchAliases>,
    user_full_names: Option<&HashMap<String, String>>,
    user_statuses: Option<&UserStatuses>,
) -> Vec<SidebarRowModel> {
    conversations
        .iter()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
        .map(|conversation| {
            SidebarRowModel::from_conversation_with_aliases(
                conversation,
                user_names,
                SidebarRowOptions {
                    current_user_id,
                    user_search_aliases,
                    user_full_names,
                    user_statuses,
                    ..Default::default()
                },
            )
        })
        .collect()
}

pub(crate) fn filter_conversation_switcher_rows(
    rows: &[SidebarRowModel],
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
    query: &str,
    user_search_aliases: Option<&UserSearchAliases>,
) -> Vec<SidebarRowModel> {
    let query = SearchQuery::parse(query);
    let mut items = rows
        .iter()
        .filter_map(|item| {
            let score = item.match_score(&query)?;
            let sort_key = title_sort_key(&item.title);
            Some((item.clone(), score, sort_key))
        })
        .collect::<Vec<_>>();

    let participant_coverage = if query.is_empty() {
        HashMap::new()
    } else {
        let matching_ids = items
            .iter()
            .map(|(item, _, _)| item.id.as_str())
            .collect::<HashSet<_>>();
        conversation_participant_coverage(
            conversations
                .iter()
                .filter(|conversation| matching_ids.contains(conversation.id.as_str())),
            user_names,
            current_user_id,
            &query,
            user_search_aliases,
        )
    };
    items.sort_by(
        |(left, left_score, left_sort_key), (right, right_score, right_sort_key)| {
            right_score
                .band()
                .cmp(&left_score.band())
                .then_with(|| {
                    compare_participant_coverage(left, right, Some(&participant_coverage))
                })
                .then_with(|| compare_user_deleted(left, right))
                .then_with(|| (left_sort_key, &left.id).cmp(&(right_sort_key, &right.id)))
        },
    );
    items.into_iter().map(|(item, _, _)| item).collect()
}

#[cfg(test)]
pub fn conversation_picker_sections(
    conversations: &[SlackConversation],
    discovered_channels: &[SlackConversation],
    discovered_users: &[SlackUser],
    user_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
    query: &str,
) -> ConversationPickerSections {
    conversation_picker_sections_with_aliases(
        conversations,
        discovered_channels,
        discovered_users,
        user_names,
        current_user_id,
        query,
        &HashMap::new(),
    )
}

#[cfg(test)]
pub fn conversation_picker_sections_with_aliases(
    conversations: &[SlackConversation],
    discovered_channels: &[SlackConversation],
    discovered_users: &[SlackUser],
    user_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
    query: &str,
    known_user_search_aliases: &UserSearchAliases,
) -> ConversationPickerSections {
    conversation_picker_sections_with_statuses(
        ConversationPickerSource {
            conversations,
            discovered_channels,
            discovered_users,
            user_names,
            current_user_id,
            known_user_search_aliases,
            user_full_names: &HashMap::new(),
            user_statuses: &HashMap::new(),
        },
        query,
    )
}

pub struct ConversationPickerSource<'a> {
    pub conversations: &'a [SlackConversation],
    pub discovered_channels: &'a [SlackConversation],
    pub discovered_users: &'a [SlackUser],
    pub user_names: &'a HashMap<String, String>,
    pub current_user_id: Option<&'a str>,
    pub known_user_search_aliases: &'a UserSearchAliases,
    pub user_full_names: &'a HashMap<String, String>,
    pub user_statuses: &'a UserStatuses,
}

pub fn conversation_picker_sections_with_statuses(
    source: ConversationPickerSource<'_>,
    query: &str,
) -> ConversationPickerSections {
    let ConversationPickerSource {
        conversations,
        discovered_channels,
        discovered_users,
        user_names,
        current_user_id,
        known_user_search_aliases,
        user_full_names,
        user_statuses,
    } = source;
    let search_query = SearchQuery::parse(query);
    let mut all_user_search_aliases = known_user_search_aliases.clone();
    all_user_search_aliases.extend(user_search_aliases(discovered_users));
    let mut participant_coverage = if search_query.is_empty() {
        HashMap::new()
    } else {
        conversation_participant_coverage(
            conversations,
            user_names,
            current_user_id,
            &search_query,
            Some(&all_user_search_aliases),
        )
    };
    if !search_query.is_empty() {
        for user in discovered_users {
            let Some(user_id) = user
                .id
                .as_deref()
                .filter(|user_id| !user_id.trim().is_empty())
            else {
                continue;
            };
            participant_coverage.insert(
                user_id.to_string(),
                ParticipantCoverage {
                    matched: usize::from(user_matches_query(
                        user_id,
                        user_names,
                        Some(&all_user_search_aliases),
                        &search_query,
                    )),
                    total: 1,
                },
            );
        }
    }
    let conversation_ids = conversations
        .iter()
        .map(|conversation| conversation.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let direct_message_users = conversations
        .iter()
        .filter(|conversation| conversation.is_im.unwrap_or(false))
        .filter_map(|conversation| conversation.user.as_deref())
        .collect::<std::collections::HashSet<_>>();

    let conversations: Vec<ConversationPickerItem> = conversation_switcher_items_with_aliases(
        conversations,
        user_names,
        current_user_id,
        query,
        Some(&all_user_search_aliases),
        Some(user_full_names),
        Some(user_statuses),
    )
    .into_iter()
    .map(|row| ConversationPickerItem {
        row,
        action: ConversationPickerAction::OpenConversation,
    })
    .collect();

    let mut channels = discovered_channels
        .iter()
        .filter(|channel| !conversation_ids.contains(channel.id.as_str()))
        .map(|channel| {
            SidebarRowModel::from_conversation(channel, user_names, None, current_user_id)
        })
        .filter(|row| row.match_score(&search_query).is_some())
        .map(|row| ConversationPickerItem {
            row,
            action: ConversationPickerAction::JoinChannel,
        })
        .collect::<Vec<_>>();
    sort_picker_items(&mut channels, Some(&search_query), None);

    let mut people = discovered_users
        .iter()
        .filter_map(|user| {
            let id = user.id.as_deref()?.trim();
            if id.is_empty()
                || Some(id) == current_user_id
                || direct_message_users.contains(id)
                || user.deleted.unwrap_or(false)
                || user.is_bot.unwrap_or(false)
            {
                return None;
            }
            let title = user.direct_message_name()?;
            let row = SidebarRowModel {
                id: id.to_string(),
                title,
                kind: ConversationKind::DirectMessage,
                unread: false,
                unread_count: 0,
                has_mention: false,
                mention_count: 0,
                selected: false,
                starred: false,
                private: true,
                muted: false,
                external: false,
                huddle_active: false,
                user_deleted: false,
                search_aliases: user.search_aliases(),
                status: user
                    .status()
                    .filter(|status| status.active_at(current_unix_seconds())),
            };
            row.match_score(&search_query)
                .is_some()
                .then_some(ConversationPickerItem {
                    row,
                    action: ConversationPickerAction::OpenDirectMessage,
                })
        })
        .collect::<Vec<_>>();
    sort_picker_items(&mut people, Some(&search_query), None);

    if !search_query.is_empty() {
        let mut search_results = conversations
            .into_iter()
            .chain(channels)
            .chain(people)
            .collect::<Vec<_>>();
        sort_picker_items(
            &mut search_results,
            Some(&search_query),
            Some(&participant_coverage),
        );
        return ConversationPickerSections {
            search_results: Some(search_results),
            ..Default::default()
        };
    }

    ConversationPickerSections {
        conversations,
        channels,
        people,
        search_results: None,
    }
}

fn sort_picker_items(
    items: &mut [ConversationPickerItem],
    query: Option<&SearchQuery>,
    participant_coverage: Option<&HashMap<String, ParticipantCoverage>>,
) {
    items.sort_by(|left, right| {
        compare_relevance(&left.row, &right.row, query)
            .then_with(|| compare_participant_coverage(&left.row, &right.row, participant_coverage))
            .then_with(|| compare_user_deleted(&left.row, &right.row))
            .then_with(|| {
                title_sort_key(&left.row.title)
                    .cmp(&title_sort_key(&right.row.title))
                    .then_with(|| left.row.id.cmp(&right.row.id))
            })
    });
}

fn build_sidebar_sections_from_rows(
    rows: impl IntoIterator<Item = SidebarRowModel>,
    query: Option<&SearchQuery>,
) -> Vec<SidebarSectionModel> {
    let mut priority_direct_messages = Vec::new();
    let mut priority_channels = Vec::new();
    let mut channels = Vec::new();
    let mut direct_messages = Vec::new();
    let mut other = Vec::new();

    for row in rows {
        if row.starred {
            match row.kind {
                ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage => {
                    priority_direct_messages.push(row.clone())
                }
                ConversationKind::PublicChannel | ConversationKind::PrivateChannel => {
                    priority_channels.push(row.clone())
                }
                ConversationKind::Unknown => {}
            }
        }

        match row.kind {
            ConversationKind::PublicChannel | ConversationKind::PrivateChannel => {
                channels.push(row)
            }
            ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage => {
                direct_messages.push(row)
            }
            ConversationKind::Unknown => other.push(row),
        }
    }

    sort_rows_by_title(&mut priority_direct_messages, query);
    sort_rows_by_title(&mut priority_channels, query);
    sort_rows_by_title(&mut channels, query);
    sort_rows_by_title(&mut direct_messages, query);
    sort_rows_by_title(&mut other, query);

    let priority = priority_direct_messages
        .into_iter()
        .chain(priority_channels)
        .collect();

    [
        section(SidebarSectionKind::Priority, priority),
        section(SidebarSectionKind::Channels, channels),
        section(SidebarSectionKind::DirectMessages, direct_messages),
        section(SidebarSectionKind::Other, other),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn conversation_kind(conversation: &SlackConversation) -> ConversationKind {
    if conversation.is_im.unwrap_or(false) {
        ConversationKind::DirectMessage
    } else if conversation.is_mpim.unwrap_or(false) {
        ConversationKind::GroupDirectMessage
    } else if conversation.is_private.unwrap_or(false) || conversation.is_group.unwrap_or(false) {
        ConversationKind::PrivateChannel
    } else if conversation.is_channel.unwrap_or(false) {
        ConversationKind::PublicChannel
    } else {
        ConversationKind::Unknown
    }
}

pub fn conversation_visible_in_default_sidebar(
    conversation: &SlackConversation,
    selected_channel: Option<&str>,
    recent_history_direct_message: bool,
) -> bool {
    if conversation.is_archived.unwrap_or(false) {
        return false;
    }

    if selected_channel == Some(conversation.id.as_str()) {
        return true;
    }

    if conversation.is_starred()
        && matches!(
            conversation_kind(conversation),
            ConversationKind::PublicChannel
                | ConversationKind::PrivateChannel
                | ConversationKind::DirectMessage
                | ConversationKind::GroupDirectMessage
        )
    {
        return true;
    }

    match conversation_kind(conversation) {
        ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage => {
            if conversation.has_unread_activity() || conversation.has_mention_activity() {
                return !conversation.is_user_deleted();
            }
            !conversation.is_user_deleted()
                && !conversation.is_dormant()
                && (conversation.has_active_direct_message_hint() || recent_history_direct_message)
        }
        ConversationKind::PublicChannel
        | ConversationKind::PrivateChannel
        | ConversationKind::Unknown => true,
    }
}

#[derive(Debug)]
struct RecentHistoryDirectMessageCandidate {
    id: String,
    activity: f64,
    title: String,
}

fn recent_history_direct_message_ids<'a>(
    conversations: impl IntoIterator<Item = &'a SlackConversation>,
    selected_channel: Option<&str>,
) -> HashSet<String> {
    let mut candidates = conversations
        .into_iter()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
        .filter(|conversation| {
            matches!(
                conversation_kind(conversation),
                ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage
            )
        })
        .filter(|conversation| selected_channel != Some(conversation.id.as_str()))
        .filter(|conversation| !conversation.is_starred())
        .filter(|conversation| !conversation.has_active_direct_message_hint())
        .filter(|conversation| !conversation.is_user_deleted() && !conversation.is_dormant())
        .filter_map(|conversation| {
            let activity = conversation_activity_score(conversation);
            (activity > 0.0).then(|| RecentHistoryDirectMessageCandidate {
                id: conversation.id.clone(),
                activity,
                title: conversation.display_name().to_lowercase(),
            })
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .activity
            .total_cmp(&left.activity)
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates
        .into_iter()
        .take(RECENT_HISTORY_DIRECT_MESSAGE_LIMIT)
        .map(|candidate| candidate.id)
        .collect()
}

fn conversation_activity_score(conversation: &SlackConversation) -> f64 {
    ["latest", "latest_ts"]
        .into_iter()
        .filter_map(|key| conversation_extra_value(conversation, key))
        .filter_map(conversation_activity_value)
        .fold(0.0, f64::max)
}

fn conversation_activity_value(value: &Value) -> Option<f64> {
    let value = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.trim().parse::<f64>().ok(),
        Value::Object(object) => object.get("ts").and_then(conversation_activity_value),
        _ => None,
    }?;
    value.is_finite().then_some(value)
}

fn conversation_extra_value<'a>(
    conversation: &'a SlackConversation,
    key: &str,
) -> Option<&'a Value> {
    conversation.extra.get(key).or_else(|| {
        conversation
            .extra
            .get("properties")
            .and_then(|properties| properties.get(key))
    })
}

fn section(kind: SidebarSectionKind, rows: Vec<SidebarRowModel>) -> Option<SidebarSectionModel> {
    (!rows.is_empty()).then_some(SidebarSectionModel {
        kind,
        title: kind.title(),
        rows,
    })
}

fn sort_rows_by_title(rows: &mut [SidebarRowModel], query: Option<&SearchQuery>) {
    rows.sort_by(|left, right| {
        compare_relevance(left, right, query).then_with(|| {
            (title_sort_key(&left.title), &left.id).cmp(&(title_sort_key(&right.title), &right.id))
        })
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParticipantCoverage {
    matched: usize,
    total: usize,
}

fn conversation_participant_coverage<'a>(
    conversations: impl IntoIterator<Item = &'a SlackConversation>,
    user_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
    query: &SearchQuery,
    user_search_aliases: Option<&UserSearchAliases>,
) -> HashMap<String, ParticipantCoverage> {
    conversations
        .into_iter()
        .filter_map(|conversation| {
            if conversation.is_im.unwrap_or(false) {
                let user_id = conversation.user.as_deref()?;
                return Some((
                    conversation.id.clone(),
                    ParticipantCoverage {
                        matched: usize::from(user_matches_query(
                            user_id,
                            user_names,
                            user_search_aliases,
                            query,
                        )),
                        total: 1,
                    },
                ));
            }
            if !conversation.is_mpim.unwrap_or(false) {
                return None;
            }
            let user_ids = conversation_user_ids(conversation, current_user_id);
            let total = user_ids.len();
            (total > 0).then(|| {
                let matched = user_ids
                    .iter()
                    .filter(|user_id| {
                        user_matches_query(user_id, user_names, user_search_aliases, query)
                    })
                    .count();
                (
                    conversation.id.clone(),
                    ParticipantCoverage { matched, total },
                )
            })
        })
        .collect()
}

fn user_matches_query(
    user_id: &str,
    user_names: &HashMap<String, String>,
    user_search_aliases: Option<&UserSearchAliases>,
    query: &SearchQuery,
) -> bool {
    user_names
        .get(user_id)
        .is_some_and(|name| query.matches_any_term(name))
        || query.matches_any_term(user_id)
        || user_search_aliases
            .and_then(|aliases| aliases.get(user_id))
            .is_some_and(|aliases| aliases.iter().any(|name| query.matches_any_term(name)))
}

fn sort_search_rows(
    rows: &mut [SidebarRowModel],
    query: &SearchQuery,
    participant_coverage: &HashMap<String, ParticipantCoverage>,
) {
    rows.sort_by(|left, right| {
        compare_relevance(left, right, Some(query))
            .then_with(|| compare_participant_coverage(left, right, Some(participant_coverage)))
            .then_with(|| compare_user_deleted(left, right))
            .then_with(|| {
                (title_sort_key(&left.title), &left.id)
                    .cmp(&(title_sort_key(&right.title), &right.id))
            })
    });
}

fn compare_participant_coverage(
    left: &SidebarRowModel,
    right: &SidebarRowModel,
    participant_coverage: Option<&HashMap<String, ParticipantCoverage>>,
) -> std::cmp::Ordering {
    let Some(participant_coverage) = participant_coverage else {
        return std::cmp::Ordering::Equal;
    };
    let left = participant_coverage
        .get(&left.id)
        .copied()
        .unwrap_or(ParticipantCoverage {
            matched: 0,
            total: 1,
        });
    let right = participant_coverage
        .get(&right.id)
        .copied()
        .unwrap_or(ParticipantCoverage {
            matched: 0,
            total: 1,
        });

    (right.matched * left.total).cmp(&(left.matched * right.total))
}

fn compare_user_deleted(left: &SidebarRowModel, right: &SidebarRowModel) -> std::cmp::Ordering {
    left.user_deleted.cmp(&right.user_deleted)
}

fn compare_relevance(
    left: &SidebarRowModel,
    right: &SidebarRowModel,
    query: Option<&SearchQuery>,
) -> std::cmp::Ordering {
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return std::cmp::Ordering::Equal;
    };
    let left_band = left.match_score(query).map_or(0, MatchScore::band);
    let right_band = right.match_score(query).map_or(0, MatchScore::band);
    right_band.cmp(&left_band)
}

fn title_sort_key(title: &str) -> String {
    title.trim_start_matches('#').trim_start().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: &str, name: &str) -> SlackConversation {
        SlackConversation {
            id: id.to_string(),
            name: Some(name.to_string()),
            is_channel: Some(true),
            ..Default::default()
        }
    }

    fn private_channel(id: &str, name: &str) -> SlackConversation {
        SlackConversation {
            id: id.to_string(),
            name: Some(name.to_string()),
            is_group: Some(true),
            is_private: Some(true),
            ..Default::default()
        }
    }

    fn dm(id: &str, user: &str) -> SlackConversation {
        SlackConversation {
            id: id.to_string(),
            user: Some(user.to_string()),
            is_im: Some(true),
            ..Default::default()
        }
    }

    fn active_dm(id: &str, user: &str) -> SlackConversation {
        let mut conversation = dm(id, user);
        conversation
            .extra
            .insert("is_open".to_string(), serde_json::json!(true));
        conversation
    }

    fn mpim(id: &str, name: &str) -> SlackConversation {
        SlackConversation {
            id: id.to_string(),
            name: Some(name.to_string()),
            is_mpim: Some(true),
            ..Default::default()
        }
    }

    fn section(sections: &[SidebarSectionModel], kind: SidebarSectionKind) -> &SidebarSectionModel {
        sections
            .iter()
            .find(|section| section.kind == kind)
            .expect("section should be present")
    }

    fn titles(section: &SidebarSectionModel) -> Vec<&str> {
        section.rows.iter().map(|row| row.title.as_str()).collect()
    }

    fn list_sections(model: SidebarListModel) -> Vec<SidebarSectionModel> {
        match model {
            SidebarListModel::Sections(sections) => sections,
            SidebarListModel::Placeholder(placeholder) => {
                panic!("expected sections, got {placeholder:?}")
            }
            SidebarListModel::Rows(_) => panic!("expected sections, got rows"),
        }
    }

    fn list_rows(model: SidebarListModel) -> Vec<SidebarRowModel> {
        match model {
            SidebarListModel::Rows(rows) => rows,
            SidebarListModel::Placeholder(placeholder) => {
                panic!("expected rows, got {placeholder:?}")
            }
            SidebarListModel::Sections(_) => panic!("expected rows, got sections"),
        }
    }

    fn list_placeholder(model: SidebarListModel) -> SidebarPlaceholder {
        match model {
            SidebarListModel::Placeholder(placeholder) => placeholder,
            SidebarListModel::Sections(_) => panic!("expected placeholder"),
            SidebarListModel::Rows(_) => panic!("expected placeholder"),
        }
    }

    fn row(title: &str, selected: bool) -> SidebarRowModel {
        SidebarRowModel {
            id: title.to_string(),
            title: title.to_string(),
            kind: ConversationKind::PublicChannel,
            unread: false,
            unread_count: 0,
            has_mention: false,
            mention_count: 0,
            selected,
            starred: false,
            private: false,
            muted: false,
            external: false,
            huddle_active: false,
            user_deleted: false,
            search_aliases: Vec::new(),
            status: None,
        }
    }

    #[test]
    fn borrowed_sidebar_iteration_is_independent_of_catalog_order() {
        let conversations = [channel("C2", "zebra"), channel("C1", "alpha")];
        let options = SidebarBuildOptions::default();

        let forward = build_sidebar_list(conversations.iter(), &HashMap::new(), options);
        let reverse = build_sidebar_list(conversations.iter().rev(), &HashMap::new(), options);

        assert_eq!(forward.keyed_items(), reverse.keyed_items());
    }

    #[test]
    fn classifies_conversation_types() {
        assert_eq!(
            conversation_kind(&channel("C1", "general")),
            ConversationKind::PublicChannel
        );
        assert_eq!(
            conversation_kind(&private_channel("G1", "secret")),
            ConversationKind::PrivateChannel
        );
        assert_eq!(
            conversation_kind(&dm("D1", "U1")),
            ConversationKind::DirectMessage
        );
        assert_eq!(
            conversation_kind(&mpim("M1", "project-chat")),
            ConversationKind::GroupDirectMessage
        );
    }

    #[test]
    fn active_huddle_marks_only_the_matching_sidebar_conversation() {
        let model = build_sidebar_list(
            &[channel("C1", "general"), channel("C2", "random")],
            &HashMap::new(),
            SidebarBuildOptions {
                active_huddle_channel_id: Some("C2"),
                ..Default::default()
            },
        );
        let rows = model
            .keyed_items()
            .into_iter()
            .filter_map(|item| match item.model {
                SidebarItemModel::Conversation(row) => Some(row),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(rows.iter().any(|row| row.id == "C2" && row.huddle_active));
        assert!(rows
            .iter()
            .find(|row| row.id == "C2")
            .unwrap()
            .accessible_label()
            .contains("huddle active"));
        assert!(rows.iter().all(|row| row.id == "C2" || !row.huddle_active));
    }

    #[test]
    fn groups_channels_and_all_dms_into_default_sections() {
        let mut user_names = HashMap::new();
        user_names.insert("U1".to_string(), "Zoe".to_string());

        let sections = build_sidebar_sections(
            &[
                channel("C1", "general"),
                dm("D1", "U1"),
                mpim("M1", "triage"),
                private_channel("G1", "leadership"),
            ],
            &user_names,
            None,
        );

        assert_eq!(
            titles(section(&sections, SidebarSectionKind::Channels)),
            vec!["#general", "#leadership"]
        );
        assert_eq!(
            titles(section(&sections, SidebarSectionKind::DirectMessages)),
            vec!["Group DM M1", "Zoe"]
        );
    }

    #[test]
    fn regular_sections_are_sorted_by_resolved_title() {
        let mut user_names = HashMap::new();
        user_names.insert("U1".to_string(), "Zoe".to_string());
        user_names.insert("U2".to_string(), "Ada".to_string());

        let sections = build_sidebar_sections(
            &[
                channel("C2", "zebra"),
                channel("C1", "alpha"),
                dm("D1", "U1"),
                dm("D2", "U2"),
            ],
            &user_names,
            None,
        );

        assert_eq!(
            titles(section(&sections, SidebarSectionKind::Channels)),
            vec!["#alpha", "#zebra"]
        );
        assert_eq!(
            titles(section(&sections, SidebarSectionKind::DirectMessages)),
            vec!["Ada", "Zoe"]
        );
    }

    #[test]
    fn priority_section_lists_vip_dms_before_starred_channels() {
        let mut channel_zebra = channel("C2", "zebra");
        channel_zebra.set_starred(true);
        let mut channel_alpha = channel("C1", "alpha");
        channel_alpha.set_starred(true);
        let mut dm_zoe = dm("D2", "U2");
        dm_zoe.set_starred(true);
        let mut dm_ada = dm("D1", "U1");
        dm_ada.set_starred(true);

        let user_names = HashMap::from([
            ("U1".to_string(), "Ada".to_string()),
            ("U2".to_string(), "Zoe".to_string()),
        ]);
        let sections = build_sidebar_sections(
            &[
                channel_zebra,
                channel("C3", "general"),
                dm_zoe,
                channel_alpha,
                dm_ada,
            ],
            &user_names,
            None,
        );

        assert_eq!(sections[0].kind, SidebarSectionKind::Priority);
        assert_eq!(
            titles(section(&sections, SidebarSectionKind::Priority)),
            vec!["Ada", "Zoe", "#alpha", "#zebra"]
        );
        assert_eq!(
            titles(section(&sections, SidebarSectionKind::Channels)),
            vec!["#alpha", "#general", "#zebra"]
        );
        assert_eq!(
            titles(section(&sections, SidebarSectionKind::DirectMessages)),
            vec!["Ada", "Zoe"]
        );
    }

    #[test]
    fn priority_section_uses_stable_ids_to_break_equal_title_ties() {
        let mut first_channel = channel("C1", "same");
        first_channel.set_starred(true);
        let mut second_channel = channel("C2", "same");
        second_channel.set_starred(true);
        let mut first_dm = dm("D1", "U1");
        first_dm.set_starred(true);
        let mut second_dm = dm("D2", "U2");
        second_dm.set_starred(true);
        let user_names = HashMap::from([
            ("U1".to_string(), "Same".to_string()),
            ("U2".to_string(), "Same".to_string()),
        ]);

        let sections = build_sidebar_sections(
            &[second_channel, second_dm, first_channel, first_dm],
            &user_names,
            None,
        );

        assert_eq!(
            section(&sections, SidebarSectionKind::Priority)
                .rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["D1", "D2", "C1", "C2"]
        );
    }

    #[test]
    fn priority_section_is_omitted_without_starred_conversations() {
        let sections = build_sidebar_sections(
            &[channel("C1", "general"), dm("D1", "U1")],
            &HashMap::from([("U1".to_string(), "Ada".to_string())]),
            None,
        );

        assert!(sections
            .iter()
            .all(|section| section.kind != SidebarSectionKind::Priority));
    }

    #[test]
    fn default_sidebar_visibility_keeps_recent_dms_and_hides_inactive_dms() {
        let active_channel = channel("C1", "general");
        let mut open_dm = dm("D_OPEN", "U_OPEN");
        open_dm
            .extra
            .insert("is_open".to_string(), serde_json::json!(true));
        let mut priority_dm = dm("D_PRIORITY", "U_PRIORITY");
        priority_dm
            .extra
            .insert("priority".to_string(), serde_json::json!(0.42));
        let mut recent_group_dm = mpim("M_RECENT", "recent");
        recent_group_dm
            .extra
            .insert("latest".to_string(), serde_json::json!("1710000000.000001"));
        let inactive_dm = dm("D_INACTIVE", "U_INACTIVE");
        let mut unopened_group_dm = mpim("M_UNOPENED", "unopened");
        unopened_group_dm
            .extra
            .insert("latest".to_string(), serde_json::json!("0.000000"));
        let conversations = vec![
            open_dm.clone(),
            priority_dm.clone(),
            recent_group_dm.clone(),
            inactive_dm.clone(),
            unopened_group_dm.clone(),
        ];
        let recent_history_direct_messages =
            recent_history_direct_message_ids(&conversations, None);

        assert!(conversation_visible_in_default_sidebar(
            &active_channel,
            None,
            false,
        ));
        assert!(conversation_visible_in_default_sidebar(
            &open_dm,
            None,
            recent_history_direct_messages.contains(&open_dm.id),
        ));
        assert!(conversation_visible_in_default_sidebar(
            &priority_dm,
            None,
            recent_history_direct_messages.contains(&priority_dm.id),
        ));
        assert!(conversation_visible_in_default_sidebar(
            &recent_group_dm,
            None,
            recent_history_direct_messages.contains(&recent_group_dm.id),
        ));
        assert!(!conversation_visible_in_default_sidebar(
            &inactive_dm,
            None,
            recent_history_direct_messages.contains(&inactive_dm.id),
        ));
        assert!(!conversation_visible_in_default_sidebar(
            &unopened_group_dm,
            None,
            recent_history_direct_messages.contains(&unopened_group_dm.id),
        ));
        assert!(conversation_visible_in_default_sidebar(
            &inactive_dm,
            Some("D_INACTIVE"),
            false,
        ));
    }

    #[test]
    fn default_sidebar_visibility_hides_dormant_deleted_and_archived_dms() {
        let mut dormant = dm("D1", "U1");
        dormant.extra.insert(
            "properties".to_string(),
            serde_json::json!({ "is_dormant": true }),
        );
        let selected_deleted: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "D2",
            "user": "U2",
            "is_im": true,
            "is_user_deleted": true,
            "priority": 0.75
        }))
        .expect("failed to parse deleted DM");
        let read_dormant: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "D3",
            "user": "U3",
            "is_im": true,
            "priority": 0.5,
            "properties": {
                "is_dormant": true
            }
        }))
        .expect("failed to parse dormant DM");
        let mut archived = dm("D4", "U4");
        archived.is_archived = Some(true);

        assert!(!conversation_visible_in_default_sidebar(
            &dormant, None, false,
        ));
        assert!(conversation_visible_in_default_sidebar(
            &selected_deleted,
            Some("D2"),
            false,
        ));
        assert!(!conversation_visible_in_default_sidebar(
            &selected_deleted,
            None,
            true,
        ));
        assert!(!conversation_visible_in_default_sidebar(
            &read_dormant,
            None,
            true,
        ));
        assert!(!conversation_visible_in_default_sidebar(
            &archived,
            Some("D4"),
            true,
        ));
    }

    #[test]
    fn row_state_includes_starred_muted_and_external_flags() {
        let mut alpha = channel("C1", "alpha");
        alpha.set_starred(true);
        alpha
            .extra
            .insert("is_muted".to_string(), serde_json::json!(true));
        alpha
            .extra
            .insert("is_ext_shared".to_string(), serde_json::json!(true));

        let sections = build_sidebar_sections(&[alpha], &HashMap::new(), None);
        let row = &section(&sections, SidebarSectionKind::Channels).rows[0];

        assert!(row.starred);
        assert!(row.muted);
        assert!(row.external);
    }

    #[test]
    fn category_1_channels_bold_when_unread_no_numeric_badge_muted_suppresses() {
        let mut alpha = channel("C1", "alpha");
        alpha.unread_count = Some(3);
        let row = SidebarRowModel::from_conversation(&alpha, &HashMap::new(), None, None);
        assert!(row.unread);
        assert_eq!(row.unread_badge_label(), None);

        let mut muted_alpha = channel("C2", "alpha-muted");
        muted_alpha.unread_count = Some(3);
        muted_alpha.extra.insert("is_muted".to_string(), serde_json::json!(true));
        let muted_row = SidebarRowModel::from_conversation(&muted_alpha, &HashMap::new(), None, None);
        assert!(!muted_row.unread);
        assert_eq!(muted_row.unread_badge_label(), None);
    }

    #[test]
    fn category_4_dms_bold_title_and_numeric_badge_and_unhide_dormant() {
        let mut unread_dm = dm("D1", "U1");
        unread_dm.unread_count = Some(5);
        unread_dm.extra.insert("is_dormant".to_string(), serde_json::json!(true));

        let row = SidebarRowModel::from_conversation(&unread_dm, &HashMap::new(), None, None);
        assert!(row.unread);
        assert_eq!(row.unread_count, 5);
        assert_eq!(row.unread_badge_label().as_deref(), Some("5"));
        assert!(conversation_visible_in_default_sidebar(&unread_dm, None, false));
    }

    #[test]
    fn category_5_mentions_override_mute_status() {
        let mut muted_channel = channel("C1", "general");
        muted_channel.extra.insert("is_muted".to_string(), serde_json::json!(true));
        muted_channel.extra.insert("has_mention".to_string(), serde_json::json!(true));
        muted_channel.extra.insert("mention_count".to_string(), serde_json::json!(2));

        let row = SidebarRowModel::from_conversation(&muted_channel, &HashMap::new(), None, None);
        assert!(row.muted);
        assert!(!row.unread);
        assert!(row.has_mention);
        assert_eq!(row.mention_count, 2);
        assert_eq!(row.mention_badge_label().as_deref(), Some("2"));
    }

    #[test]
    fn selected_channel_is_marked_in_all_matching_rows() {
        let general = channel("C1", "general");

        let sections = build_sidebar_sections(&[general], &HashMap::new(), Some("C1"));

        assert!(
            section(&sections, SidebarSectionKind::Channels).rows[0].selected,
            "regular row should be selected"
        );
    }

    #[test]
    fn accessible_label_includes_type_and_selected_state() {
        let mut selected_row = row("#general", true);
        selected_row.muted = true;
        selected_row.external = true;

        assert_eq!(
            selected_row.accessible_label(),
            "Public channel: #general, selected, muted, external"
        );
    }

    #[test]
    fn direct_dm_rows_include_status_without_changing_title_or_group_dms() {
        let direct = active_dm("D1", "U1");
        let group: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G1",
            "is_mpim": true,
            "is_open": true,
            "members": ["U1", "U2"]
        }))
        .expect("failed to parse group DM");
        let names = HashMap::from([
            ("U1".to_string(), "Ada".to_string()),
            ("U2".to_string(), "Grace".to_string()),
        ]);
        let statuses = HashMap::from([(
            "U1".to_string(),
            SlackUserStatus {
                text: "Heads down".to_string(),
                emoji: ":brain:".to_string(),
                expiration: i64::MAX,
            },
        )]);

        let rows = list_rows(build_sidebar_list(
            &[group, direct],
            &names,
            SidebarBuildOptions {
                query: "a",
                user_statuses: Some(&statuses),
                ..Default::default()
            },
        ));
        let direct = rows.iter().find(|row| row.id == "D1").unwrap();
        let group = rows.iter().find(|row| row.id == "G1").unwrap();

        assert_eq!(direct.title, "Ada");
        assert_eq!(direct.status.as_ref().unwrap().text, "Heads down");
        assert!(direct.accessible_label().contains("status: Heads down"));
        assert!(group.status.is_none());
    }

    #[test]
    fn sidebar_list_uses_loading_error_and_empty_placeholders() {
        assert_eq!(
            list_placeholder(build_sidebar_list(
                &[],
                &HashMap::new(),
                SidebarBuildOptions {
                    loading: true,
                    ..Default::default()
                },
            )),
            SidebarPlaceholder::Loading
        );
        assert_eq!(
            list_placeholder(build_sidebar_list(
                &[],
                &HashMap::new(),
                SidebarBuildOptions {
                    has_error: true,
                    ..Default::default()
                },
            )),
            SidebarPlaceholder::LoadFailed
        );
        assert_eq!(
            list_placeholder(build_sidebar_list(
                &[],
                &HashMap::new(),
                SidebarBuildOptions::default(),
            )),
            SidebarPlaceholder::Empty
        );
        assert_eq!(
            list_placeholder(build_sidebar_list(
                &[channel("C1", "general")],
                &HashMap::new(),
                SidebarBuildOptions {
                    query: "missing",
                    ..Default::default()
                },
            )),
            SidebarPlaceholder::NoMatches
        );
    }

    #[test]
    fn sidebar_list_applies_query_and_default_visibility_filters() {
        let general = SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        let random = SlackConversation {
            id: "C456".to_string(),
            name: Some("random".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        let mut open_dm = SlackConversation {
            id: "D123".to_string(),
            user: Some("U123".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        open_dm
            .extra
            .insert("is_open".to_string(), serde_json::json!(true));
        let user_names = HashMap::from([("U123".to_string(), "Ada".to_string())]);

        let rows = list_rows(build_sidebar_list(
            &[general.clone(), random.clone(), open_dm.clone()],
            &user_names,
            SidebarBuildOptions {
                query: "ada",
                ..Default::default()
            },
        ));

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Ada");
        assert_eq!(
            list_placeholder(build_sidebar_list(
                &[general, random, open_dm],
                &user_names,
                SidebarBuildOptions {
                    query: "nothing-matches-this",
                    ..Default::default()
                },
            )),
            SidebarPlaceholder::NoMatches
        );
    }

    #[test]
    fn sidebar_list_default_keeps_selected_inactive_dms() {
        let inactive_dm = dm("D_INACTIVE", "U_INACTIVE");
        let selected_group_dm = mpim("M_SELECTED", "selected");
        let conversations = [inactive_dm, selected_group_dm];

        let sections = list_sections(build_sidebar_list(
            &conversations,
            &HashMap::new(),
            SidebarBuildOptions {
                selected_channel: Some("M_SELECTED"),
                ..Default::default()
            },
        ));
        let rows = &section(&sections, SidebarSectionKind::DirectMessages).rows;

        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["M_SELECTED"]
        );
    }

    #[test]
    fn sidebar_list_caps_recent_history_without_spending_slots_on_selected_dms() {
        let mut conversations = (0..RECENT_HISTORY_DIRECT_MESSAGE_LIMIT + 3)
            .map(|index| {
                let mut conversation = dm(
                    &format!("D_RECENT_{index:02}"),
                    &format!("U_RECENT_{index:02}"),
                );
                conversation.extra.insert(
                    "latest".to_string(),
                    serde_json::json!(format!("{}.000001", index + 1)),
                );
                conversation
            })
            .collect::<Vec<_>>();
        let mut selected_dormant = dm("D_SELECTED", "U_SELECTED");
        selected_dormant.extra.insert(
            "properties".to_string(),
            serde_json::json!({ "is_dormant": true }),
        );
        conversations.push(selected_dormant);

        let sections = list_sections(build_sidebar_list(
            &conversations,
            &HashMap::new(),
            SidebarBuildOptions {
                selected_channel: Some("D_SELECTED"),
                ..Default::default()
            },
        ));
        let rows = &section(&sections, SidebarSectionKind::DirectMessages).rows;
        let ids = rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<HashSet<_>>();

        assert_eq!(rows.len(), RECENT_HISTORY_DIRECT_MESSAGE_LIMIT + 1);
        assert!(ids.contains("D_SELECTED"));
        assert!(ids.contains("D_RECENT_22"));
        assert!(!ids.contains("D_RECENT_00"));
        assert!(!ids.contains("D_RECENT_01"));
        assert!(!ids.contains("D_RECENT_02"));
    }

    #[test]
    fn sidebar_list_keeps_explicitly_active_dms_outside_the_recent_history_cap() {
        let conversations = (0..RECENT_HISTORY_DIRECT_MESSAGE_LIMIT + 5)
            .map(|index| {
                let mut conversation = dm(
                    &format!("D_ACTIVE_{index:02}"),
                    &format!("U_ACTIVE_{index:02}"),
                );
                if index % 2 == 0 {
                    conversation
                        .extra
                        .insert("is_open".to_string(), serde_json::json!(true));
                } else {
                    conversation.extra.insert(
                        "priority".to_string(),
                        serde_json::json!((index + 1) as f64),
                    );
                }
                conversation
            })
            .collect::<Vec<_>>();

        let sections = list_sections(build_sidebar_list(
            &conversations,
            &HashMap::new(),
            SidebarBuildOptions::default(),
        ));
        let rows = &section(&sections, SidebarSectionKind::DirectMessages).rows;

        assert_eq!(rows.len(), RECENT_HISTORY_DIRECT_MESSAGE_LIMIT + 5);
    }

    #[test]
    fn sidebar_list_starred_dms_do_not_consume_recent_history_slots() {
        let mut conversations = (0..RECENT_HISTORY_DIRECT_MESSAGE_LIMIT)
            .map(|index| {
                let mut conversation = dm(
                    &format!("D_STARRED_{index:02}"),
                    &format!("U_STARRED_{index:02}"),
                );
                conversation.is_starred = Some(true);
                conversation.extra.insert(
                    "latest".to_string(),
                    serde_json::json!(format!("{}.000001", index + 100)),
                );
                conversation
            })
            .collect::<Vec<_>>();
        conversations.extend((0..RECENT_HISTORY_DIRECT_MESSAGE_LIMIT + 1).map(|index| {
            let mut conversation = dm(
                &format!("D_RECENT_{index:02}"),
                &format!("U_RECENT_{index:02}"),
            );
            conversation.extra.insert(
                "latest".to_string(),
                serde_json::json!(format!("{}.000001", index + 1)),
            );
            conversation
        }));

        let sections = list_sections(build_sidebar_list(
            &conversations,
            &HashMap::new(),
            SidebarBuildOptions::default(),
        ));
        let rows = &section(&sections, SidebarSectionKind::DirectMessages).rows;
        let ids = rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<HashSet<_>>();

        assert_eq!(rows.len(), RECENT_HISTORY_DIRECT_MESSAGE_LIMIT * 2);
        assert!(ids.contains("D_RECENT_20"));
        assert!(!ids.contains("D_RECENT_00"));
    }

    #[test]
    fn sidebar_list_show_all_includes_inactive_and_unknown_but_not_archived_conversations() {
        let read_dm = dm("D_READ", "U_READ");
        let mut dormant_dm = dm("D_DORMANT", "U_DORMANT");
        dormant_dm.extra.insert(
            "properties".to_string(),
            serde_json::json!({ "is_dormant": true }),
        );
        let mut archived_dm = dm("D_ARCHIVED", "U_ARCHIVED");
        archived_dm.is_archived = Some(true);
        let unknown = SlackConversation {
            id: "X_UNKNOWN".to_string(),
            ..Default::default()
        };

        let sections = list_sections(build_sidebar_list(
            &[read_dm, dormant_dm, archived_dm, unknown],
            &HashMap::new(),
            SidebarBuildOptions {
                show_all: true,
                ..Default::default()
            },
        ));
        let ids = section(&sections, SidebarSectionKind::DirectMessages)
            .rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"D_READ"));
        assert!(ids.contains(&"D_DORMANT"));
        assert!(!ids.contains(&"D_ARCHIVED"));
        assert_eq!(
            section(&sections, SidebarSectionKind::Other).rows[0].id,
            "X_UNKNOWN"
        );
    }

    #[test]
    fn show_all_still_respects_the_query_filter() {
        let mut dormant_dm = dm("D_DORMANT", "U_DORMANT");
        dormant_dm.extra.insert(
            "properties".to_string(),
            serde_json::json!({ "is_dormant": true }),
        );
        let alerts_channel = SlackConversation {
            id: "C_ALERTS".to_string(),
            name: Some("alerts".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        let user_names = HashMap::from([("U_DORMANT".to_string(), "Ada".to_string())]);
        let conversations = [dormant_dm, alerts_channel];

        assert_eq!(
            list_placeholder(build_sidebar_list(
                &conversations,
                &user_names,
                SidebarBuildOptions {
                    query: "ada",
                    ..Default::default()
                },
            )),
            SidebarPlaceholder::NoMatches
        );
        let queried = list_rows(build_sidebar_list(
            &conversations,
            &user_names,
            SidebarBuildOptions {
                query: "ada",
                show_all: true,
                ..Default::default()
            },
        ));
        assert_eq!(queried[0].id, "D_DORMANT");

        let shown = list_sections(build_sidebar_list(
            &conversations,
            &user_names,
            SidebarBuildOptions {
                show_all: true,
                ..Default::default()
            },
        ));
        let ids = shown
            .iter()
            .flat_map(|section| section.rows.iter())
            .map(|row| row.id.as_str())
            .collect::<HashSet<_>>();
        assert!(ids.contains("C_ALERTS"));
        assert!(ids.contains("D_DORMANT"));
    }

    #[test]
    fn conversation_switcher_items_search_all_loaded_conversations() {
        let active = SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        let dormant_dm: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "D123",
            "user": "U123",
            "is_im": true,
            "properties": {
                "is_dormant": true
            }
        }))
        .expect("failed to parse dormant DM");
        let user_names = HashMap::from([("U123".to_string(), "Ada Lovelace".to_string())]);

        let items = conversation_switcher_items(&[active, dormant_dm], &user_names, None, "ada");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "D123");
        assert_eq!(items[0].title, "Ada Lovelace");
    }

    #[test]
    fn sidebar_and_switcher_use_full_dm_names() {
        let dm = dm("D1", "U1");
        let display_names = HashMap::from([("U1".to_string(), "ada".to_string())]);
        let full_names = HashMap::from([("U1".to_string(), "Ada Lovelace".to_string())]);

        let sidebar_rows = list_rows(build_sidebar_list(
            std::slice::from_ref(&dm),
            &display_names,
            SidebarBuildOptions {
                selected_channel: Some("D1"),
                query: "ada",
                user_full_names: Some(&full_names),
                ..Default::default()
            },
        ));
        let switcher_rows = conversation_switcher_items_with_aliases(
            std::slice::from_ref(&dm),
            &display_names,
            None,
            "",
            None,
            Some(&full_names),
            None,
        );
        let empty_conversations = Vec::new();
        let empty_users = Vec::new();
        let empty_aliases = HashMap::new();
        let empty_statuses = HashMap::new();
        let forward_picker = conversation_picker_sections_with_statuses(
            ConversationPickerSource {
                conversations: &[dm],
                discovered_channels: &empty_conversations,
                discovered_users: &empty_users,
                user_names: &display_names,
                current_user_id: None,
                known_user_search_aliases: &empty_aliases,
                user_full_names: &full_names,
                user_statuses: &empty_statuses,
            },
            "",
        );

        assert_eq!(sidebar_rows[0].title, "Ada Lovelace (ada)");
        assert_eq!(switcher_rows[0].title, "Ada Lovelace (ada)");
        assert_eq!(
            forward_picker.conversations[0].row.title,
            "Ada Lovelace (ada)"
        );
    }

    #[test]
    fn conversation_switcher_items_match_title_and_id() {
        let general = SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        let random = SlackConversation {
            id: "C456".to_string(),
            name: Some("random".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };

        let title_match = conversation_switcher_items(
            &[general.clone(), random.clone()],
            &HashMap::new(),
            None,
            "gen",
        );
        let id_match =
            conversation_switcher_items(&[general, random], &HashMap::new(), None, "456");

        assert_eq!(title_match[0].id, "C123");
        assert_eq!(id_match[0].id, "C456");
    }

    #[test]
    fn conversation_filters_match_all_substring_terms_in_any_order() {
        let conversations = [channel("C123", "broker-orange-support")];

        let matches =
            conversation_switcher_items(&conversations, &HashMap::new(), None, "  SUPP   bro ");
        let misses =
            conversation_switcher_items(&conversations, &HashMap::new(), None, "bro sales");

        assert_eq!(matches[0].id, "C123");
        assert!(misses.is_empty());
    }

    #[test]
    fn conversation_switcher_prioritizes_relevance_bands_then_alphabet() {
        let conversations = [
            channel("C1", "alpha-support"),
            channel("C2", "zebra-supp"),
            channel("C3", "beta-supple"),
        ];

        let items = conversation_switcher_items(&conversations, &HashMap::new(), None, "supp");

        assert_eq!(
            items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            // Exact "supp" wins. The other two are in the same ten-point band,
            // so their existing alphabetical ordering remains intact.
            vec!["C2", "C1", "C3"]
        );
    }

    #[test]
    fn conversation_switcher_sorts_direct_and_group_dms_together() {
        let conversations = [dm("D1", "U1"), mpim("M1", "triage")];
        let user_names = HashMap::from([("U1".to_string(), "Zoe".to_string())]);

        let items = conversation_switcher_items(&conversations, &user_names, None, "");

        assert_eq!(
            items
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Group DM M1", "Zoe"]
        );
    }

    #[test]
    fn deactivated_csaba_dm_is_final_priority_before_alphabetic_ordering() {
        let active = dm("CONV_ZOE", "U_ZOE");
        let mut csaba = dm("CONV_CSABA", "U_CSABA");
        csaba
            .extra
            .insert("is_user_deleted".to_string(), serde_json::json!(true));
        let user_names = HashMap::from([
            ("U_CSABA".to_string(), "Csaba Karpati".to_string()),
            ("U_ZOE".to_string(), "Zoe Adams".to_string()),
        ]);
        let conversations = [csaba, active];

        let sidebar_rows = list_rows(build_sidebar_list(
            &conversations,
            &user_names,
            SidebarBuildOptions {
                query: "conv",
                show_all: true,
                ..Default::default()
            },
        ));
        let switcher_rows = conversation_switcher_items(&conversations, &user_names, None, "conv");
        let picker_rows =
            conversation_picker_sections(&conversations, &[], &[], &user_names, None, "conv")
                .search_results
                .expect("search should retain deactivated direct messages")
                .into_iter()
                .map(|item| item.row)
                .collect::<Vec<_>>();

        for rows in [sidebar_rows, switcher_rows, picker_rows] {
            assert_eq!(
                rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
                vec!["CONV_ZOE", "CONV_CSABA"]
            );
        }
    }

    #[test]
    fn sidebar_relevance_band_precedes_alphabetic_sort() {
        let alphabetical = channel("C1", "alpha-support");
        let relevant = channel("C2", "zebra-supp");

        let rows = list_rows(build_sidebar_list(
            &[alphabetical, relevant],
            &HashMap::new(),
            SidebarBuildOptions {
                query: "supp",
                ..Default::default()
            },
        ));

        assert_eq!(
            rows.iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            vec!["#zebra-supp", "#alpha-support"]
        );
    }

    #[test]
    fn sidebar_search_flattens_sections_and_ranks_group_dms_globally() {
        let group_dm: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G1",
            "is_mpim": true,
            "is_open": true,
            "members": ["U1", "U2"]
        }))
        .expect("failed to parse group direct message");
        let channel = channel("C1", "fatness-robust");
        let user_names = HashMap::from([
            ("U1".to_string(), "Fatima".to_string()),
            ("U2".to_string(), "Robey".to_string()),
        ]);

        let rows = list_rows(build_sidebar_list(
            &[channel, group_dm],
            &user_names,
            SidebarBuildOptions {
                query: "fat rob",
                ..Default::default()
            },
        ));

        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["G1", "C1"]
        );
    }

    #[test]
    fn sidebar_search_ranks_matching_direct_dm_above_matching_group_dm() {
        let direct = active_dm("D_RICHARD", "U_RICHARD");
        let group: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G_RICHARD",
            "is_mpim": true,
            "is_open": true,
            "members": ["U_SELF", "U_RICHARD", "U_OTHER"]
        }))
        .expect("failed to parse group DM");
        let user_names = HashMap::from([
            ("U_SELF".to_string(), "Vincent".to_string()),
            ("U_RICHARD".to_string(), "Richard".to_string()),
            ("U_OTHER".to_string(), "Ada".to_string()),
        ]);

        let rows = list_rows(build_sidebar_list(
            &[group, direct],
            &user_names,
            SidebarBuildOptions {
                current_user_id: Some("U_SELF"),
                query: "richard",
                ..Default::default()
            },
        ));

        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["D_RICHARD", "G_RICHARD"]
        );
    }

    #[test]
    fn picker_ranks_existing_and_prospective_dms_above_group_dms() {
        let direct = dm("D_EVANS", "U_EVANS");
        let group: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G_RICHARD",
            "is_mpim": true,
            "members": ["U_SELF", "U_HEKKERS", "U_OTHER"]
        }))
        .expect("failed to parse group DM");
        let discovered_person = SlackUser {
            id: Some("U_SUNDLOF".to_string()),
            real_name: Some("Richard Sundlöf".to_string()),
            ..Default::default()
        };
        let user_names = HashMap::from([
            ("U_SELF".to_string(), "Vincent".to_string()),
            ("U_EVANS".to_string(), "Richard Evans".to_string()),
            ("U_HEKKERS".to_string(), "Richard Hekkers".to_string()),
            ("U_OTHER".to_string(), "Ada".to_string()),
        ]);

        let results = conversation_picker_sections(
            &[group, direct],
            &[],
            &[discovered_person],
            &user_names,
            Some("U_SELF"),
            "richard",
        )
        .search_results
        .expect("search should produce flat results");

        assert_eq!(
            results
                .iter()
                .map(|item| item.row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["D_EVANS", "U_SUNDLOF", "G_RICHARD"]
        );
    }

    #[test]
    fn forward_picker_uses_singular_dm_coverage_but_keeps_relevance_primary() {
        let alias_dm = dm("D_SVEN", "U_SVEN");
        let exact_group: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G_RICHARD",
            "is_mpim": true,
            "members": ["U_SELF", "U_RICHARD", "U_OTHER"]
        }))
        .expect("failed to parse group DM");
        let user_names = HashMap::from([
            ("U_SELF".to_string(), "Vincent".to_string()),
            ("U_SVEN".to_string(), "Sven".to_string()),
            ("U_RICHARD".to_string(), "Richard".to_string()),
            ("U_OTHER".to_string(), "Ada".to_string()),
        ]);
        let aliases = HashMap::from([(
            "U_SVEN".to_string(),
            vec!["Sven Richard Samdal".to_string()],
        )]);

        let results = conversation_picker_sections_with_aliases(
            &[alias_dm, exact_group],
            &[],
            &[],
            &user_names,
            Some("U_SELF"),
            "richard",
            &aliases,
        )
        .search_results
        .expect("search should produce flat results");

        assert_eq!(
            results
                .iter()
                .map(|item| item.row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["G_RICHARD", "D_SVEN"]
        );
    }

    #[test]
    fn group_dm_search_ranks_by_matching_participant_coverage_and_excludes_self() {
        let full_match: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G_FULL",
            "is_mpim": true,
            "is_open": true,
            "members": ["U_SELF", "U_FAT", "U_ROB"]
        }))
        .expect("failed to parse full-match group DM");
        let partial_match: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G_PARTIAL",
            "is_mpim": true,
            "is_open": true,
            "members": ["U_SELF", "U_AARON", "U_BOTH"]
        }))
        .expect("failed to parse partial-match group DM");
        let user_names = HashMap::from([
            ("U_SELF".to_string(), "Vincent".to_string()),
            ("U_FAT".to_string(), "Fatima".to_string()),
            ("U_ROB".to_string(), "Robey".to_string()),
            ("U_AARON".to_string(), "Aaron".to_string()),
            ("U_BOTH".to_string(), "Fatima Robey".to_string()),
        ]);
        let conversations = [partial_match, full_match];

        let sidebar_rows = list_rows(build_sidebar_list(
            &conversations,
            &user_names,
            SidebarBuildOptions {
                current_user_id: Some("U_SELF"),
                query: "fat rob",
                ..Default::default()
            },
        ));
        let switcher_rows =
            conversation_switcher_items(&conversations, &user_names, Some("U_SELF"), "fat rob");
        let picker_rows = conversation_picker_sections(
            &conversations,
            &[],
            &[],
            &user_names,
            Some("U_SELF"),
            "fat rob",
        )
        .search_results
        .expect("expected flat picker results")
        .into_iter()
        .map(|item| item.row)
        .collect::<Vec<_>>();

        for rows in [sidebar_rows, switcher_rows, picker_rows] {
            assert_eq!(
                rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
                vec!["G_FULL", "G_PARTIAL"]
            );
            assert_eq!(rows[0].title, "Fatima, Robey");
            assert!(!rows.iter().any(|row| row.title.contains("Vincent")));
        }
    }

    #[test]
    fn empty_query_sorts_conversations_by_title() {
        let conversations = [channel("C2", "zebra"), channel("C1", "alpha")];

        let items = conversation_switcher_items(&conversations, &HashMap::new(), None, "  ");

        assert_eq!(
            items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["C1", "C2"]
        );
    }

    #[test]
    fn conversation_switcher_searches_resolved_group_dm_member_names() {
        let group_dm: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G123",
            "name": "mpdm-old-slack-name",
            "is_mpim": true,
            "members": ["U2", "U1"]
        }))
        .expect("failed to parse group direct message");
        let user_names = HashMap::from([
            ("U1".to_string(), "Grace Hopper".to_string()),
            ("U2".to_string(), "Ada Lovelace".to_string()),
        ]);

        let items = conversation_switcher_items(&[group_dm], &user_names, None, "grace");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Ada Lovelace, Grace Hopper");
    }

    #[test]
    fn sidebar_filter_finds_existing_dm_by_user_alias() {
        let conversation = active_dm("D_ZILVINAS", "U_ZILVINAS");
        let user_names = HashMap::from([("U_ZILVINAS".to_string(), "Žilvinas".to_string())]);
        let aliases = HashMap::from([(
            "U_ZILVINAS".to_string(),
            vec!["Žilvinas Kuusas".to_string(), "zilvinas.kuusas".to_string()],
        )]);

        let rows = list_rows(build_sidebar_list(
            &[conversation],
            &user_names,
            SidebarBuildOptions {
                query: "Kuusas",
                user_search_aliases: Some(&aliases),
                ..Default::default()
            },
        ));

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "D_ZILVINAS");
        assert_eq!(rows[0].title, "Žilvinas");
    }

    #[test]
    fn conversation_picker_separates_new_channels_and_people_from_existing_conversations() {
        let general = channel("C1", "general");
        let existing_dm = SlackConversation {
            id: "D1".to_string(),
            user: Some("U1".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        let discovered_channels = vec![general.clone(), channel("C2", "random")];
        let users = vec![
            SlackUser {
                id: Some("U1".to_string()),
                real_name: Some("Ada Lovelace".to_string()),
                ..Default::default()
            },
            SlackUser {
                id: Some("U2".to_string()),
                real_name: Some("Grace Hopper".to_string()),
                ..Default::default()
            },
            SlackUser {
                id: Some("U_SELF".to_string()),
                real_name: Some("Current User".to_string()),
                ..Default::default()
            },
        ];

        let sections = conversation_picker_sections(
            &[general, existing_dm],
            &discovered_channels,
            &users,
            &HashMap::from([("U1".to_string(), "Ada Lovelace".to_string())]),
            Some("U_SELF"),
            "",
        );

        assert_eq!(sections.conversations.len(), 2);
        assert!(sections.search_results.is_none());
        assert_eq!(sections.channels.len(), 1);
        assert_eq!(sections.channels[0].row.title, "#random");
        assert_eq!(
            sections.channels[0].action,
            ConversationPickerAction::JoinChannel
        );
        assert_eq!(sections.people.len(), 1);
        assert_eq!(sections.people[0].row.title, "Grace Hopper");
        assert_eq!(
            sections.people[0].action,
            ConversationPickerAction::OpenDirectMessage
        );
    }

    #[test]
    fn conversation_picker_finds_existing_dm_by_real_normalized_and_username_aliases() {
        let existing_dm = SlackConversation {
            id: "D_ZILVINAS".to_string(),
            user: Some("U_ZILVINAS".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        let user = SlackUser {
            id: Some("U_ZILVINAS".to_string()),
            name: Some("zilvinas.kuusas".to_string()),
            real_name: Some("Žilvinas Kuusas".to_string()),
            profile: Some(crate::models::SlackUserProfile {
                display_name: Some("Žilvinas".to_string()),
                display_name_normalized: Some("Zilvinas".to_string()),
                real_name: Some("Žilvinas Kuusas".to_string()),
                real_name_normalized: Some("Zilvinas Kuusas".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let user_names = HashMap::from([("U_ZILVINAS".to_string(), "Žilvinas".to_string())]);
        let cached_aliases = user_search_aliases(std::slice::from_ref(&user));

        for query in ["Zilvinas Kuusas", "Kuusas", "zilvinas.kuusas"] {
            let results = conversation_picker_sections_with_aliases(
                std::slice::from_ref(&existing_dm),
                &[],
                &[],
                &user_names,
                None,
                query,
                &cached_aliases,
            )
            .search_results
            .expect("search should produce flat results");

            assert_eq!(results.len(), 1, "query: {query}");
            assert_eq!(results[0].row.id, "D_ZILVINAS");
            assert_eq!(results[0].row.title, "Žilvinas");
            assert_eq!(
                results[0].action,
                ConversationPickerAction::OpenConversation
            );
        }
    }

    #[test]
    fn group_dm_alias_search_uses_participant_coverage_and_excludes_self() {
        let focused: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G_FOCUSED",
            "is_mpim": true,
            "members": ["U_SELF", "U_ZILVINAS"]
        }))
        .expect("failed to parse focused group DM");
        let broad: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G_BROAD",
            "is_mpim": true,
            "members": ["U_SELF", "U_ZILVINAS", "U_ADA"]
        }))
        .expect("failed to parse broad group DM");
        let user_names = HashMap::from([
            ("U_SELF".to_string(), "Vincent".to_string()),
            ("U_ZILVINAS".to_string(), "Žilvinas".to_string()),
            ("U_ADA".to_string(), "Ada".to_string()),
        ]);
        let aliases = HashMap::from([
            (
                "U_SELF".to_string(),
                vec!["Vincent SecretSurname".to_string()],
            ),
            (
                "U_ZILVINAS".to_string(),
                vec!["Žilvinas Kuusas".to_string()],
            ),
        ]);

        let results = conversation_picker_sections_with_aliases(
            &[broad.clone(), focused],
            &[],
            &[],
            &user_names,
            Some("U_SELF"),
            "Kuusas",
            &aliases,
        )
        .search_results
        .expect("search should produce flat results");
        assert_eq!(
            results
                .iter()
                .map(|item| item.row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["G_FOCUSED", "G_BROAD"]
        );

        let self_results = conversation_picker_sections_with_aliases(
            &[broad],
            &[],
            &[],
            &user_names,
            Some("U_SELF"),
            "SecretSurname",
            &aliases,
        )
        .search_results
        .expect("search should produce flat results");
        assert!(self_results.is_empty());
    }

    #[test]
    fn conversation_picker_searches_across_all_sections() {
        let sections = conversation_picker_sections(
            &[channel("C1", "general")],
            &[channel("C2", "project-rainbow")],
            &[SlackUser {
                id: Some("U2".to_string()),
                real_name: Some("Rainbow Dash".to_string()),
                ..Default::default()
            }],
            &HashMap::new(),
            None,
            "rainbow",
        );

        let results = sections
            .search_results
            .expect("expected flat search results");
        assert_eq!(
            results
                .iter()
                .map(|item| item.row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["U2", "C2"]
        );
        assert!(sections.conversations.is_empty());
        assert!(sections.channels.is_empty());
        assert!(sections.people.is_empty());
    }

    #[test]
    fn conversation_picker_matches_terms_across_title_and_id() {
        let sections = conversation_picker_sections(
            &[],
            &[channel("C-RAINBOW", "project-rainbow")],
            &[],
            &HashMap::new(),
            None,
            "rain c-r",
        );

        assert_eq!(
            sections.search_results.expect("expected flat results")[0]
                .row
                .id,
            "C-RAINBOW"
        );
    }

    #[test]
    fn conversation_picker_query_is_flat_without_discovery_results() {
        let sections = conversation_picker_sections(
            &[channel("C1", "alpha-support"), channel("C2", "zebra-supp")],
            &[],
            &[],
            &HashMap::new(),
            None,
            "supp",
        );

        let results = sections.search_results.expect("expected flat results");
        assert_eq!(
            results
                .iter()
                .map(|item| item.row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["C2", "C1"]
        );
        assert!(results
            .iter()
            .all(|item| item.action == ConversationPickerAction::OpenConversation));
        assert!(sections.conversations.is_empty());
    }

    #[test]
    fn conversation_picker_ranks_all_search_results_globally() {
        let sections = conversation_picker_sections(
            &[],
            &[channel("C1", "alpha-support"), channel("C2", "zebra-supp")],
            &[
                SlackUser {
                    id: Some("U1".to_string()),
                    real_name: Some("Alpha Support".to_string()),
                    ..Default::default()
                },
                SlackUser {
                    id: Some("U2".to_string()),
                    real_name: Some("Zebra Supp".to_string()),
                    ..Default::default()
                },
            ],
            &HashMap::new(),
            None,
            "supp",
        );

        assert_eq!(
            sections
                .search_results
                .expect("expected flat search results")
                .iter()
                .map(|item| item.row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["U2", "C2", "U1", "C1"]
        );
    }

    #[test]
    fn conversation_picker_ignores_channel_hash_during_alphabetic_fallback() {
        let sections = conversation_picker_sections(
            &[],
            &[channel("C1", "zebra-team")],
            &[SlackUser {
                id: Some("U1".to_string()),
                real_name: Some("Alpha Team".to_string()),
                ..Default::default()
            }],
            &HashMap::new(),
            None,
            "team",
        );

        assert_eq!(
            sections
                .search_results
                .expect("expected flat search results")
                .iter()
                .map(|item| item.row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["U1", "C1"]
        );
    }

    #[test]
    fn keyed_sidebar_items_distinguish_duplicate_conversation_placements() {
        let conversation = row("C1", false);
        let model = SidebarListModel::Sections(vec![
            SidebarSectionModel {
                kind: SidebarSectionKind::Priority,
                title: SidebarSectionKind::Priority.title(),
                rows: vec![conversation.clone()],
            },
            SidebarSectionModel {
                kind: SidebarSectionKind::DirectMessages,
                title: SidebarSectionKind::DirectMessages.title(),
                rows: vec![conversation.clone()],
            },
            SidebarSectionModel {
                kind: SidebarSectionKind::Channels,
                title: SidebarSectionKind::Channels.title(),
                rows: vec![conversation],
            },
        ]);

        let items = model.keyed_items();
        assert_eq!(items.len(), 6);
        assert_eq!(
            items[1].key,
            SidebarItemKey::Conversation {
                section: Some(SidebarSectionKind::Priority),
                id: "C1".to_string(),
            }
        );
        assert_eq!(
            items[3].key,
            SidebarItemKey::Conversation {
                section: Some(SidebarSectionKind::DirectMessages),
                id: "C1".to_string(),
            }
        );
        assert_eq!(
            items[5].key,
            SidebarItemKey::Conversation {
                section: Some(SidebarSectionKind::Channels),
                id: "C1".to_string(),
            }
        );
    }

    #[test]
    fn collapsing_priority_keeps_the_regular_conversation_row() {
        let conversation = row("C1", false);
        let model = SidebarListModel::Sections(vec![
            SidebarSectionModel {
                kind: SidebarSectionKind::Priority,
                title: SidebarSectionKind::Priority.title(),
                rows: vec![conversation.clone()],
            },
            SidebarSectionModel {
                kind: SidebarSectionKind::Channels,
                title: SidebarSectionKind::Channels.title(),
                rows: vec![conversation],
            },
        ]);

        let items = model
            .keyed_items_with_collapsed_sections(&HashSet::from([SidebarSectionKind::Priority]));

        assert_eq!(items.len(), 3);
        assert_eq!(
            items[2].key,
            SidebarItemKey::Conversation {
                section: Some(SidebarSectionKind::Channels),
                id: "C1".to_string(),
            }
        );
    }

    #[test]
    fn keyed_sidebar_items_hide_only_collapsed_section_rows() {
        let model = SidebarListModel::Sections(vec![
            SidebarSectionModel {
                kind: SidebarSectionKind::Channels,
                title: SidebarSectionKind::Channels.title(),
                rows: vec![row("C1", false), row("C2", false)],
            },
            SidebarSectionModel {
                kind: SidebarSectionKind::DirectMessages,
                title: SidebarSectionKind::DirectMessages.title(),
                rows: vec![row("D1", false)],
            },
        ]);

        let collapsed = HashSet::from([SidebarSectionKind::Channels]);
        let items = model.keyed_items_with_collapsed_sections(&collapsed);

        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0].model,
            SidebarItemModel::SectionHeader {
                kind: SidebarSectionKind::Channels,
                title: "Channels".to_string(),
                collapsed: true,
            }
        );
        assert_eq!(
            items[1].model,
            SidebarItemModel::SectionHeader {
                kind: SidebarSectionKind::DirectMessages,
                title: "Direct messages".to_string(),
                collapsed: false,
            }
        );
        assert_eq!(
            items[2].key,
            SidebarItemKey::Conversation {
                section: Some(SidebarSectionKind::DirectMessages),
                id: "D1".to_string(),
            }
        );
    }

    #[test]
    fn collapsed_sections_do_not_change_flat_rows_or_placeholders() {
        let collapsed = HashSet::from([SidebarSectionKind::Channels]);
        let rows = SidebarListModel::Rows(vec![row("C1", false)]);
        let placeholder = SidebarListModel::Placeholder(SidebarPlaceholder::NoMatches);

        assert_eq!(
            rows.keyed_items_with_collapsed_sections(&collapsed),
            rows.keyed_items()
        );
        assert_eq!(
            placeholder.keyed_items_with_collapsed_sections(&collapsed),
            placeholder.keyed_items()
        );
    }

    #[test]
    fn sidebar_projection_resets_once_then_ignores_identical_models() {
        let items = SidebarListModel::Rows(vec![row("C1", false)]).keyed_items();
        let mut projection = SidebarProjection::default();

        assert_eq!(
            projection.reconcile(&items),
            vec![SidebarProjectionOperation::Reset]
        );
        assert_eq!(projection.items(), items);
        assert!(projection.reconcile(&items).is_empty());
    }

    #[test]
    fn sidebar_projection_updates_one_row_in_a_large_workspace() {
        let rows = (0..1_430)
            .map(|index| row(&format!("C{index}"), false))
            .collect::<Vec<_>>();
        let mut next = SidebarListModel::Rows(rows).keyed_items();
        let mut projection = SidebarProjection::default();
        assert_eq!(
            projection.reconcile(&next),
            vec![SidebarProjectionOperation::Reset]
        );

        let SidebarItemModel::Conversation(changed) = &mut next[713].model else {
            panic!("expected conversation row");
        };
        changed.muted = true;

        assert_eq!(
            projection.reconcile(&next),
            vec![SidebarProjectionOperation::Update { position: 713 }]
        );
    }

    #[test]
    fn sidebar_projection_updates_one_row_without_rebuilding_a_large_model() {
        let rows = (0..1_430)
            .map(|index| row(&format!("C{index}"), false))
            .collect::<Vec<_>>();
        let initial = SidebarListModel::Rows(rows).keyed_items();
        let mut projection = SidebarProjection::default();
        projection.reconcile(&initial);
        let mut changed = row("C713", false);
        changed.muted = true;

        assert_eq!(
            projection.update_conversation_rows(&[changed]),
            Some(vec![SidebarProjectionOperation::Update { position: 713 }])
        );
    }

    #[test]
    fn sidebar_projection_updates_every_duplicate_conversation_placement() {
        let mut duplicated = row("C1", false);
        duplicated.starred = true;
        let items = SidebarListModel::Sections(vec![
            SidebarSectionModel {
                kind: SidebarSectionKind::Priority,
                title: SidebarSectionKind::Priority.title(),
                rows: vec![duplicated.clone()],
            },
            SidebarSectionModel {
                kind: SidebarSectionKind::Channels,
                title: SidebarSectionKind::Channels.title(),
                rows: vec![duplicated.clone()],
            },
        ])
        .keyed_items();
        let mut projection = SidebarProjection::default();
        projection.reconcile(&items);
        duplicated.muted = true;

        assert_eq!(
            projection.update_conversation_rows(&[duplicated]),
            Some(vec![
                SidebarProjectionOperation::Update { position: 1 },
                SidebarProjectionOperation::Update { position: 3 },
            ])
        );
    }

    #[test]
    fn sidebar_projection_rebuilds_when_section_membership_or_order_can_change() {
        let starred = row("C1", false);
        let items = SidebarListModel::Sections(vec![SidebarSectionModel {
            kind: SidebarSectionKind::Channels,
            title: SidebarSectionKind::Channels.title(),
            rows: vec![starred.clone()],
        }])
        .keyed_items();
        let mut projection = SidebarProjection::default();
        projection.reconcile(&items);

        let mut reordered = starred.clone();
        reordered.title = "#renamed".to_string();
        assert_eq!(projection.update_conversation_rows(&[reordered]), None);

        let mut membership_changed = starred;
        membership_changed.starred = true;
        assert_eq!(
            projection.update_conversation_rows(&[membership_changed]),
            None
        );
    }

    #[test]
    fn sidebar_projection_targeted_updates_are_atomic_and_require_existing_rows() {
        let mut first = row("C1", false);
        let mut second = row("C2", false);
        let items = SidebarListModel::Rows(vec![first.clone(), second.clone()]).keyed_items();
        let mut projection = SidebarProjection::default();
        projection.reconcile(&items);

        first.muted = true;
        second.starred = true;
        assert_eq!(
            projection.update_conversation_rows(&[first.clone(), second]),
            None
        );
        assert_eq!(projection.items(), items);

        assert_eq!(
            projection.update_conversation_rows(&[row("missing", false)]),
            None
        );
        assert_eq!(projection.items(), items);
    }

    #[test]
    fn collapsed_sections_allow_regular_row_updates() {
        let mut starred = row("C1", false);
        starred.starred = true;
        let model = SidebarListModel::Sections(vec![
            SidebarSectionModel {
                kind: SidebarSectionKind::Priority,
                title: SidebarSectionKind::Priority.title(),
                rows: vec![starred.clone()],
            },
            SidebarSectionModel {
                kind: SidebarSectionKind::Channels,
                title: SidebarSectionKind::Channels.title(),
                rows: vec![starred.clone()],
            },
        ]);
        let items = model
            .keyed_items_with_collapsed_sections(&HashSet::from([SidebarSectionKind::Priority]));
        let mut projection = SidebarProjection::default();
        projection.reconcile(&items);
        starred.muted = true;

        assert_eq!(
            projection.update_conversation_rows(&[starred]),
            Some(vec![SidebarProjectionOperation::Update { position: 2 }])
        );
    }

    #[test]
    fn targeted_update_uses_positions_rebuilt_after_structural_reconciliation() {
        let initial =
            SidebarListModel::Rows(vec![row("C1", false), row("C2", false)]).keyed_items();
        let reordered =
            SidebarListModel::Rows(vec![row("C2", false), row("C1", false)]).keyed_items();
        let mut projection = SidebarProjection::default();
        projection.reconcile(&initial);
        projection.reconcile(&reordered);
        let mut changed = row("C1", false);
        changed.muted = true;

        assert_eq!(
            projection.update_conversation_rows(&[changed]),
            Some(vec![SidebarProjectionOperation::Update { position: 1 }])
        );
    }

    #[test]
    fn sidebar_projection_splices_only_a_local_structural_change() {
        let rows = (0..1_430)
            .map(|index| row(&format!("C{index}"), false))
            .collect::<Vec<_>>();
        let initial = SidebarListModel::Rows(rows).keyed_items();
        let mut next = initial.clone();
        let inserted = SidebarListModel::Rows(vec![row("C-new", false)])
            .keyed_items()
            .pop()
            .unwrap();
        next.insert(713, inserted);
        let mut projection = SidebarProjection::default();
        projection.reconcile(&initial);

        assert_eq!(
            projection.reconcile(&next),
            vec![SidebarProjectionOperation::Splice {
                position: 713,
                removed: 0,
                inserted: 1,
            }]
        );
    }
}
