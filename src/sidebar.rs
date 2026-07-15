use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use crate::models::{SlackConversation, SlackUser, SlackUserStatus};
use crate::search::{
    MatchScore, SearchField, SearchQuery, ID_FIELD_WEIGHT, PRIMARY_FIELD_WEIGHT,
    SECONDARY_FIELD_WEIGHT,
};

pub type UserSearchAliases = HashMap<String, Vec<String>>;
pub type UserStatuses = HashMap<String, SlackUserStatus>;

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
    Unreads,
    Channels,
    DirectMessages,
    Other,
}

impl SidebarSectionKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Unreads => "Unreads",
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
    pub selected: bool,
    pub private: bool,
    pub muted: bool,
    pub external: bool,
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
        match self.unread_count {
            0 => None,
            1..=99 => Some(self.unread_count.to_string()),
            _ => Some("99+".to_string()),
        }
    }

    pub fn accessible_label(&self) -> String {
        let mut label = format!("{}: {}", self.kind.accessible_name(), self.title);
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
        if self.muted {
            label.push_str(", muted");
        }
        if self.external {
            label.push_str(", external");
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
        match self.kind {
            SidebarSectionKind::Unreads => format!("{} ({})", self.title, self.rows.len()),
            _ => self.title.to_string(),
        }
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
/// A conversation can occur both in the unread section and in its regular
/// section, so the section is part of its identity. Search results have no
/// section. Keeping this identity separate from the row contents lets the UI
/// update an existing widget when only unread, selection, or status data
/// changes.
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
    },
    Conversation(SidebarRowModel),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedSidebarItem {
    pub key: SidebarItemKey,
    pub model: SidebarItemModel,
}

/// The minimal set of keyed changes needed to reconcile two sidebar models.
/// Positions refer to the new model. Updated entries retain their widget
/// identity; inserted and removed entries require widget changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SidebarModelDiff {
    pub removed: Vec<SidebarItemKey>,
    pub inserted: Vec<(usize, SidebarItemKey)>,
    pub moved: Vec<(SidebarItemKey, usize)>,
    pub updated: Vec<(SidebarItemKey, usize)>,
}

impl SidebarListModel {
    pub fn keyed_items(&self) -> Vec<KeyedSidebarItem> {
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
                        },
                    };
                    std::iter::once(header).chain(section.rows.iter().cloned().map(|row| {
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

pub fn diff_keyed_sidebar_items(
    previous: &[KeyedSidebarItem],
    next: &[KeyedSidebarItem],
) -> SidebarModelDiff {
    let previous_by_key: HashMap<_, _> = previous
        .iter()
        .enumerate()
        .map(|(index, item)| (&item.key, (index, &item.model)))
        .collect();
    let next_keys: HashSet<_> = next.iter().map(|item| &item.key).collect();

    let removed = previous
        .iter()
        .filter(|item| !next_keys.contains(&item.key))
        .map(|item| item.key.clone())
        .collect();
    let mut inserted = Vec::new();
    let mut moved = Vec::new();
    let mut updated = Vec::new();

    for (next_index, item) in next.iter().enumerate() {
        let Some((previous_index, previous_model)) = previous_by_key.get(&item.key) else {
            inserted.push((next_index, item.key.clone()));
            continue;
        };
        if *previous_index != next_index {
            moved.push((item.key.clone(), next_index));
        }
        if *previous_model != &item.model {
            updated.push((item.key.clone(), next_index));
        }
    }

    SidebarModelDiff {
        removed,
        inserted,
        moved,
        updated,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SidebarBuildOptions<'a> {
    pub selected_channel: Option<&'a str>,
    pub current_user_id: Option<&'a str>,
    pub query: &'a str,
    pub unread_only: bool,
    pub show_unreads_section: bool,
    pub loading: bool,
    pub has_error: bool,
    pub user_search_aliases: Option<&'a UserSearchAliases>,
    pub user_statuses: Option<&'a UserStatuses>,
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
            selected_channel,
            current_user_id,
            None,
            None,
        )
    }

    fn from_conversation_with_aliases(
        conversation: &SlackConversation,
        user_names: &HashMap<String, String>,
        selected_channel: Option<&str>,
        current_user_id: Option<&str>,
        user_search_aliases: Option<&UserSearchAliases>,
        user_statuses: Option<&UserStatuses>,
    ) -> Self {
        let kind = conversation_kind(conversation);
        let search_aliases = conversation_user_ids(conversation, current_user_id)
            .into_iter()
            .filter_map(|user_id| user_search_aliases?.get(&user_id))
            .flatten()
            .cloned()
            .collect();
        Self {
            id: conversation.id.clone(),
            title: conversation.display_name_with_users(user_names, current_user_id),
            kind,
            unread: conversation.has_unread_activity(),
            unread_count: conversation.unread_activity_count(),
            selected: selected_channel == Some(conversation.id.as_str()),
            private: conversation.is_private.unwrap_or(false)
                || conversation.is_group.unwrap_or(false)
                || matches!(kind, ConversationKind::PrivateChannel),
            muted: conversation.is_muted_conversation(),
            external: conversation.is_external_conversation(),
            search_aliases,
            status: (kind == ConversationKind::DirectMessage)
                .then_some(conversation.user.as_deref())
                .flatten()
                .and_then(|user_id| active_user_status(user_statuses, user_id)),
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

pub fn build_sidebar_list(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    options: SidebarBuildOptions<'_>,
) -> SidebarListModel {
    if options.loading && conversations.is_empty() {
        return SidebarListModel::Placeholder(SidebarPlaceholder::Loading);
    }

    if options.has_error && conversations.is_empty() {
        return SidebarListModel::Placeholder(SidebarPlaceholder::LoadFailed);
    }

    if conversations.is_empty() {
        return SidebarListModel::Placeholder(SidebarPlaceholder::Empty);
    }

    let query = SearchQuery::parse(options.query);
    let mut rows = conversations
        .iter()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
        .filter(|conversation| {
            options.selected_channel == Some(conversation.id.as_str())
                || conversation_kind(conversation) != ConversationKind::Unknown
        })
        .filter(|conversation| {
            conversation_visible_in_default_sidebar(conversation, options.selected_channel)
        })
        .map(|conversation| {
            SidebarRowModel::from_conversation_with_aliases(
                conversation,
                user_names,
                options.selected_channel,
                options.current_user_id,
                options.user_search_aliases,
                options.user_statuses,
            )
        })
        .filter(|row| row.match_score(&query).is_some() && (!options.unread_only || row.unread))
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

    let mut sections = build_sidebar_sections_from_rows(rows, None);
    if options.unread_only || !options.show_unreads_section {
        sections.retain(|section| section.kind != SidebarSectionKind::Unreads);
    }

    SidebarListModel::Sections(sections)
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
    )
}

pub(crate) fn conversation_switcher_items_with_aliases(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
    query: &str,
    user_search_aliases: Option<&UserSearchAliases>,
    user_statuses: Option<&UserStatuses>,
) -> Vec<SidebarRowModel> {
    let query = SearchQuery::parse(query);
    let mut items = conversations
        .iter()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
        .map(|conversation| {
            SidebarRowModel::from_conversation_with_aliases(
                conversation,
                user_names,
                None,
                current_user_id,
                user_search_aliases,
                user_statuses,
            )
        })
        .filter(|item| item.match_score(&query).is_some())
        .collect::<Vec<_>>();

    let participant_coverage = conversation_participant_coverage(
        conversations,
        user_names,
        current_user_id,
        &query,
        user_search_aliases,
    );
    sort_search_rows(&mut items, &query, &participant_coverage);
    items
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
        user_statuses,
    } = source;
    let search_query = SearchQuery::parse(query);
    let mut all_user_search_aliases = known_user_search_aliases.clone();
    all_user_search_aliases.extend(user_search_aliases(discovered_users));
    let mut participant_coverage = conversation_participant_coverage(
        conversations,
        user_names,
        current_user_id,
        &search_query,
        Some(&all_user_search_aliases),
    );
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
            let title = user.display_name()?;
            let row = SidebarRowModel {
                id: id.to_string(),
                title,
                kind: ConversationKind::DirectMessage,
                unread: false,
                unread_count: 0,
                selected: false,
                private: true,
                muted: false,
                external: false,
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
    let mut unreads = Vec::new();
    let mut channels = Vec::new();
    let mut direct_messages = Vec::new();
    let mut other = Vec::new();

    for row in rows {
        if row.unread {
            unreads.push(row.clone());
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

    sort_unread_rows(&mut unreads, query);
    sort_rows_by_title(&mut channels, query);
    sort_rows_by_title(&mut direct_messages, query);
    sort_rows_by_title(&mut other, query);

    [
        section(SidebarSectionKind::Unreads, unreads),
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
) -> bool {
    if selected_channel == Some(conversation.id.as_str()) {
        return true;
    }

    if conversation.is_archived.unwrap_or(false) {
        return false;
    }

    match conversation_kind(conversation) {
        ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage => {
            conversation.has_unread_activity()
        }
        ConversationKind::PublicChannel
        | ConversationKind::PrivateChannel
        | ConversationKind::Unknown => true,
    }
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

fn conversation_participant_coverage(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
    query: &SearchQuery,
    user_search_aliases: Option<&UserSearchAliases>,
) -> HashMap<String, ParticipantCoverage> {
    conversations
        .iter()
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

fn sort_unread_rows(rows: &mut [SidebarRowModel], query: Option<&SearchQuery>) {
    rows.sort_by(|left, right| {
        compare_relevance(left, right, query).then_with(|| {
            (
                Reverse(left.unread_count),
                title_sort_key(&left.title),
                &left.id,
            )
                .cmp(&(
                    Reverse(right.unread_count),
                    title_sort_key(&right.title),
                    &right.id,
                ))
        })
    });
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

    fn row(title: &str, unread_count: u64, selected: bool) -> SidebarRowModel {
        SidebarRowModel {
            id: title.to_string(),
            title: title.to_string(),
            kind: ConversationKind::PublicChannel,
            unread: unread_count > 0,
            unread_count,
            selected,
            private: false,
            muted: false,
            external: false,
            search_aliases: Vec::new(),
            status: None,
        }
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
    fn unread_section_duplicates_unread_conversations_and_sorts_by_count() {
        let mut alpha = channel("C1", "alpha");
        alpha.unread_count = Some(2);
        let mut beta = dm("D1", "U1");
        beta.unread_count = Some(7);
        let mut gamma = channel("C2", "gamma");
        gamma.unread_count = Some(7);

        let mut user_names = HashMap::new();
        user_names.insert("U1".to_string(), "Beta".to_string());

        let sections = build_sidebar_sections(&[alpha, beta, gamma], &user_names, None);

        assert_eq!(
            titles(section(&sections, SidebarSectionKind::Unreads)),
            vec!["Beta", "#gamma", "#alpha"]
        );
        assert_eq!(
            titles(section(&sections, SidebarSectionKind::Channels)),
            vec!["#alpha", "#gamma"]
        );
        assert_eq!(
            titles(section(&sections, SidebarSectionKind::DirectMessages)),
            vec!["Beta"]
        );
    }

    #[test]
    fn unread_section_uses_extra_unread_properties() {
        let mut alpha = channel("C1", "alpha");
        alpha
            .extra
            .insert("unread_count_display".to_string(), serde_json::json!(5));

        let sections = build_sidebar_sections(&[alpha], &HashMap::new(), None);
        let unread_row = &section(&sections, SidebarSectionKind::Unreads).rows[0];

        assert_eq!(unread_row.title, "#alpha");
        assert_eq!(unread_row.unread_count, 5);
        assert!(unread_row.unread);
    }

    #[test]
    fn unread_section_keeps_badgeless_unread_conversations() {
        let mut alpha = channel("C1", "alpha");
        alpha
            .extra
            .insert("has_unreads".to_string(), serde_json::json!(true));

        let sections = build_sidebar_sections(&[alpha], &HashMap::new(), None);
        let unread_row = &section(&sections, SidebarSectionKind::Unreads).rows[0];

        assert_eq!(unread_row.title, "#alpha");
        assert_eq!(unread_row.unread_count, 0);
        assert!(unread_row.unread);
        assert_eq!(unread_row.unread_badge_label(), None);
    }

    #[test]
    fn default_sidebar_visibility_hides_read_inactive_dms() {
        let active_channel = channel("C1", "general");
        let read_dm = dm("D1", "U1");
        let read_group_dm = mpim("M1", "triage");

        assert!(conversation_visible_in_default_sidebar(
            &active_channel,
            None
        ));
        assert!(!conversation_visible_in_default_sidebar(&read_dm, None));
        assert!(!conversation_visible_in_default_sidebar(
            &read_group_dm,
            None
        ));
        assert!(conversation_visible_in_default_sidebar(
            &read_dm,
            Some("D1")
        ));
        assert!(conversation_visible_in_default_sidebar(
            &read_group_dm,
            Some("M1")
        ));
    }

    #[test]
    fn default_sidebar_visibility_keeps_unread_dms_but_hides_read_deleted_dms() {
        let mut unread_dormant = dm("D1", "U1");
        unread_dormant.unread_count = Some(2);
        unread_dormant.extra.insert(
            "properties".to_string(),
            serde_json::json!({ "is_dormant": true }),
        );
        let selected_deleted: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "D2",
            "user": "U2",
            "is_im": true,
            "is_user_deleted": true
        }))
        .expect("failed to parse deleted DM");

        assert!(conversation_visible_in_default_sidebar(
            &unread_dormant,
            None
        ));
        assert!(conversation_visible_in_default_sidebar(
            &selected_deleted,
            Some("D2")
        ));
        assert!(!conversation_visible_in_default_sidebar(
            &selected_deleted,
            None
        ));
    }

    #[test]
    fn row_state_includes_muted_and_external_flags() {
        let mut alpha = channel("C1", "alpha");
        alpha
            .extra
            .insert("is_muted".to_string(), serde_json::json!(true));
        alpha
            .extra
            .insert("is_ext_shared".to_string(), serde_json::json!(true));

        let sections = build_sidebar_sections(&[alpha], &HashMap::new(), None);
        let row = &section(&sections, SidebarSectionKind::Channels).rows[0];

        assert!(row.muted);
        assert!(row.external);
    }

    #[test]
    fn selected_channel_is_marked_in_all_matching_rows() {
        let mut general = channel("C1", "general");
        general.unread_count = Some(1);

        let sections = build_sidebar_sections(&[general], &HashMap::new(), Some("C1"));

        assert!(
            section(&sections, SidebarSectionKind::Unreads).rows[0].selected,
            "unread duplicate should be selected"
        );
        assert!(
            section(&sections, SidebarSectionKind::Channels).rows[0].selected,
            "regular row should be selected"
        );
    }

    #[test]
    fn unread_badge_is_capped_for_large_counts() {
        assert_eq!(row("#general", 0, false).unread_badge_label(), None);
        assert_eq!(
            row("#general", 12, false).unread_badge_label().as_deref(),
            Some("12")
        );
        assert_eq!(
            row("#general", 120, false).unread_badge_label().as_deref(),
            Some("99+")
        );
    }

    #[test]
    fn accessible_label_includes_type_unreads_and_selected_state() {
        let mut unread_row = row("#general", 3, true);
        unread_row.muted = true;
        unread_row.external = true;

        assert_eq!(
            unread_row.accessible_label(),
            "Public channel: #general, 3 unread, selected, muted, external"
        );

        let mut badgeless = row("#general", 0, false);
        badgeless.unread = true;
        assert_eq!(
            badgeless.accessible_label(),
            "Public channel: #general, unread"
        );
    }

    #[test]
    fn direct_dm_rows_include_status_without_changing_title_or_group_dms() {
        let mut direct = dm("D1", "U1");
        direct.unread_count = Some(1);
        let mut group: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G1",
            "is_mpim": true,
            "members": ["U1", "U2"]
        }))
        .expect("failed to parse group DM");
        group.unread_count = Some(1);
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
    fn unread_section_display_title_includes_row_count() {
        let section = SidebarSectionModel {
            kind: SidebarSectionKind::Unreads,
            title: "Unreads",
            rows: vec![row("#general", 1, false), row("#random", 1, false)],
        };

        assert_eq!(section.display_title(), "Unreads (2)");
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
    fn sidebar_list_applies_query_unread_and_default_visibility_filters() {
        let unread = SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            unread_count: Some(2),
            ..Default::default()
        };
        let read = SlackConversation {
            id: "C456".to_string(),
            name: Some("random".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        let mut badgeless_unread_dm = SlackConversation {
            id: "D123".to_string(),
            user: Some("U123".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        badgeless_unread_dm
            .extra
            .insert("has_unreads".to_string(), serde_json::json!(true));
        let user_names = HashMap::from([("U123".to_string(), "Ada".to_string())]);

        let rows = list_rows(build_sidebar_list(
            &[unread.clone(), read.clone(), badgeless_unread_dm.clone()],
            &user_names,
            SidebarBuildOptions {
                query: "ada",
                unread_only: true,
                ..Default::default()
            },
        ));

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Ada");
        assert_eq!(
            list_placeholder(build_sidebar_list(
                &[unread, read, badgeless_unread_dm],
                &user_names,
                SidebarBuildOptions {
                    query: "random",
                    unread_only: true,
                    ..Default::default()
                },
            )),
            SidebarPlaceholder::NoMatches
        );
    }

    #[test]
    fn sidebar_list_only_keeps_unread_or_selected_dms() {
        let read_dm = dm("D_READ", "U_READ");
        let selected_group_dm = mpim("M_SELECTED", "selected");
        let mut unread_group_dm = mpim("M_UNREAD", "unread");
        unread_group_dm.unread_count = Some(1);
        let conversations = [read_dm, selected_group_dm, unread_group_dm];

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
            vec!["M_SELECTED", "M_UNREAD"]
        );
    }

    #[test]
    fn sidebar_list_can_hide_unreads_section() {
        let unread = SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            unread_count: Some(2),
            ..Default::default()
        };

        let sections = list_sections(build_sidebar_list(
            &[unread],
            &HashMap::new(),
            SidebarBuildOptions {
                show_unreads_section: false,
                ..Default::default()
            },
        ));

        assert!(sections
            .iter()
            .all(|section| section.kind != SidebarSectionKind::Unreads));
        assert_eq!(
            section(&sections, SidebarSectionKind::Channels).rows.len(),
            1
        );
    }

    #[test]
    fn sidebar_list_can_show_unreads_section() {
        let unread = SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            unread_count: Some(2),
            ..Default::default()
        };

        let sections = list_sections(build_sidebar_list(
            &[unread],
            &HashMap::new(),
            SidebarBuildOptions {
                show_unreads_section: true,
                ..Default::default()
            },
        ));

        assert_eq!(
            section(&sections, SidebarSectionKind::Unreads).rows.len(),
            1
        );
        assert_eq!(
            section(&sections, SidebarSectionKind::Channels).rows.len(),
            1
        );
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
    fn sidebar_relevance_band_precedes_existing_unread_count_sort() {
        let mut alphabetical = channel("C1", "alpha-support");
        alphabetical.unread_count = Some(10);
        let mut relevant = channel("C2", "zebra-supp");
        relevant.unread_count = Some(1);

        let rows = list_rows(build_sidebar_list(
            &[alphabetical, relevant],
            &HashMap::new(),
            SidebarBuildOptions {
                query: "supp",
                show_unreads_section: true,
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
            "members": ["U1", "U2"],
            "unread_count": 2
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
                show_unreads_section: true,
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
        let mut direct = dm("D_RICHARD", "U_RICHARD");
        direct.unread_count = Some(1);
        let mut group: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G_RICHARD",
            "is_mpim": true,
            "members": ["U_SELF", "U_RICHARD", "U_OTHER"]
        }))
        .expect("failed to parse group DM");
        group.unread_count = Some(1);
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
        let mut full_match: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G_FULL",
            "is_mpim": true,
            "members": ["U_SELF", "U_FAT", "U_ROB"]
        }))
        .expect("failed to parse full-match group DM");
        full_match.unread_count = Some(1);
        let mut partial_match: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G_PARTIAL",
            "is_mpim": true,
            "members": ["U_SELF", "U_AARON", "U_BOTH"]
        }))
        .expect("failed to parse partial-match group DM");
        partial_match.unread_count = Some(1);
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
    fn empty_query_preserves_existing_conversation_order() {
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
        let mut conversation = dm("D_ZILVINAS", "U_ZILVINAS");
        conversation.unread_count = Some(1);
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
    fn conversation_picker_lists_new_channels_and_people_after_existing_conversations() {
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
        let conversation = row("C1", 2, false);
        let model = SidebarListModel::Sections(vec![
            SidebarSectionModel {
                kind: SidebarSectionKind::Unreads,
                title: SidebarSectionKind::Unreads.title(),
                rows: vec![conversation.clone()],
            },
            SidebarSectionModel {
                kind: SidebarSectionKind::Channels,
                title: SidebarSectionKind::Channels.title(),
                rows: vec![conversation],
            },
        ]);

        let items = model.keyed_items();
        assert_eq!(items.len(), 4);
        assert_eq!(
            items[1].key,
            SidebarItemKey::Conversation {
                section: Some(SidebarSectionKind::Unreads),
                id: "C1".to_string(),
            }
        );
        assert_eq!(
            items[3].key,
            SidebarItemKey::Conversation {
                section: Some(SidebarSectionKind::Channels),
                id: "C1".to_string(),
            }
        );
    }

    #[test]
    fn keyed_sidebar_diff_retains_identity_for_content_updates_and_moves() {
        let previous =
            SidebarListModel::Rows(vec![row("C1", 0, false), row("C2", 0, false)]).keyed_items();
        let next = SidebarListModel::Rows(vec![
            row("C2", 3, true),
            row("C3", 0, false),
            row("C1", 0, false),
        ])
        .keyed_items();

        let diff = diff_keyed_sidebar_items(&previous, &next);
        assert!(diff.removed.is_empty());
        assert_eq!(
            diff.inserted,
            vec![(
                1,
                SidebarItemKey::Conversation {
                    section: None,
                    id: "C3".to_string(),
                }
            )]
        );
        assert_eq!(diff.moved.len(), 2);
        assert_eq!(
            diff.updated,
            vec![(
                SidebarItemKey::Conversation {
                    section: None,
                    id: "C2".to_string(),
                },
                0
            )]
        );
    }

    #[test]
    fn keyed_sidebar_diff_removes_obsolete_placeholder() {
        let previous = SidebarListModel::Placeholder(SidebarPlaceholder::Loading).keyed_items();
        let next = SidebarListModel::Rows(vec![row("C1", 0, false)]).keyed_items();

        let diff = diff_keyed_sidebar_items(&previous, &next);
        assert_eq!(
            diff.removed,
            vec![SidebarItemKey::Placeholder(SidebarPlaceholder::Loading)]
        );
        assert_eq!(diff.inserted.len(), 1);
        assert!(diff.moved.is_empty());
        assert!(diff.updated.is_empty());
    }
}
