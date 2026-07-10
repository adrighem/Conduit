use std::cmp::Reverse;
use std::collections::HashMap;

use crate::models::SlackConversation;
use serde_json::Value;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarSectionKind {
    Unreads,
    Channels,
    DirectMessages,
    GroupDirectMessages,
    Other,
}

impl SidebarSectionKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Unreads => "Unreads",
            Self::Channels => "Channels",
            Self::DirectMessages => "Direct messages",
            Self::GroupDirectMessages => "Group direct messages",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SidebarBuildOptions<'a> {
    pub selected_channel: Option<&'a str>,
    pub query: &'a str,
    pub unread_only: bool,
    pub show_unreads_section: bool,
    pub show_all: bool,
    pub loading: bool,
    pub has_error: bool,
}

impl SidebarRowModel {
    pub fn from_conversation(
        conversation: &SlackConversation,
        user_names: &HashMap<String, String>,
        selected_channel: Option<&str>,
    ) -> Self {
        let kind = conversation_kind(conversation);
        Self {
            id: conversation.id.clone(),
            title: conversation.display_name_with_users(user_names),
            kind,
            unread: conversation.has_unread_activity(),
            unread_count: conversation.unread_activity_count(),
            selected: selected_channel == Some(conversation.id.as_str()),
            private: conversation.is_private.unwrap_or(false)
                || conversation.is_group.unwrap_or(false)
                || matches!(kind, ConversationKind::PrivateChannel),
            muted: conversation.is_muted_conversation(),
            external: conversation.is_external_conversation(),
        }
    }

    fn matches_query(&self, query: &str) -> bool {
        query.is_empty()
            || self.title.to_lowercase().contains(query)
            || self.id.to_lowercase().contains(query)
    }
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

    let query = normalized_query(options.query);
    let rows = conversations
        .iter()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
        .filter(|conversation| {
            options.show_all
                || conversation_visible_in_default_sidebar(conversation, options.selected_channel)
        })
        .map(|conversation| {
            SidebarRowModel::from_conversation(conversation, user_names, options.selected_channel)
        })
        .filter(|row| row.matches_query(&query) && (!options.unread_only || row.unread));

    let mut sections = build_sidebar_sections_from_rows(rows);
    if options.unread_only || !options.show_unreads_section {
        sections.retain(|section| section.kind != SidebarSectionKind::Unreads);
    }

    if sections.is_empty() {
        SidebarListModel::Placeholder(SidebarPlaceholder::NoMatches)
    } else {
        SidebarListModel::Sections(sections)
    }
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
                SidebarRowModel::from_conversation(conversation, user_names, selected_channel)
            }),
    )
}

pub fn conversation_switcher_items(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    query: &str,
) -> Vec<SidebarRowModel> {
    let query = normalized_query(query);
    let mut items = conversations
        .iter()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
        .map(|conversation| SidebarRowModel::from_conversation(conversation, user_names, None))
        .filter(|item| item.matches_query(&query))
        .collect::<Vec<_>>();

    sort_rows_by_title(&mut items);
    items
}

fn build_sidebar_sections_from_rows(
    rows: impl IntoIterator<Item = SidebarRowModel>,
) -> Vec<SidebarSectionModel> {
    let mut unreads = Vec::new();
    let mut channels = Vec::new();
    let mut direct_messages = Vec::new();
    let mut group_direct_messages = Vec::new();
    let mut other = Vec::new();

    for row in rows {
        if row.unread {
            unreads.push(row.clone());
        }

        match row.kind {
            ConversationKind::PublicChannel | ConversationKind::PrivateChannel => {
                channels.push(row)
            }
            ConversationKind::DirectMessage => direct_messages.push(row),
            ConversationKind::GroupDirectMessage => group_direct_messages.push(row),
            ConversationKind::Unknown => other.push(row),
        }
    }

    sort_unread_rows(&mut unreads);
    sort_rows_by_title(&mut channels);
    sort_rows_by_title(&mut direct_messages);
    sort_rows_by_title(&mut group_direct_messages);
    sort_rows_by_title(&mut other);

    [
        section(SidebarSectionKind::Unreads, unreads),
        section(SidebarSectionKind::Channels, channels),
        section(SidebarSectionKind::DirectMessages, direct_messages),
        section(
            SidebarSectionKind::GroupDirectMessages,
            group_direct_messages,
        ),
        section(SidebarSectionKind::Other, other),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn normalized_query(query: &str) -> String {
    query.trim().to_lowercase()
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

    if conversation.has_unread_activity() {
        return true;
    }

    if conversation_extra_bool(conversation, "is_user_deleted")
        || conversation_extra_bool(conversation, "is_dormant")
    {
        return false;
    }

    match conversation_kind(conversation) {
        ConversationKind::PublicChannel | ConversationKind::PrivateChannel => true,
        ConversationKind::DirectMessage | ConversationKind::GroupDirectMessage => {
            conversation_extra_bool(conversation, "is_open")
                || conversation_extra_number_positive(conversation, "priority")
                || conversation_extra_non_zero_string(conversation, "last_read")
        }
        ConversationKind::Unknown => {
            conversation_extra_bool(conversation, "is_open")
                || conversation_extra_number_positive(conversation, "priority")
                || conversation_extra_non_zero_string(conversation, "last_read")
        }
    }
}

fn section(kind: SidebarSectionKind, rows: Vec<SidebarRowModel>) -> Option<SidebarSectionModel> {
    (!rows.is_empty()).then_some(SidebarSectionModel {
        kind,
        title: kind.title(),
        rows,
    })
}

fn sort_rows_by_title(rows: &mut [SidebarRowModel]) {
    rows.sort_by_key(|row| (title_sort_key(&row.title), row.id.clone()));
}

fn sort_unread_rows(rows: &mut [SidebarRowModel]) {
    rows.sort_by_key(|row| {
        (
            Reverse(row.unread_count),
            title_sort_key(&row.title),
            row.id.clone(),
        )
    });
}

fn title_sort_key(title: &str) -> String {
    title.trim_start_matches('#').trim_start().to_lowercase()
}

fn conversation_extra_bool(conversation: &SlackConversation, key: &str) -> bool {
    conversation_extra_value(conversation, key)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn conversation_extra_number_positive(conversation: &SlackConversation, key: &str) -> bool {
    conversation_extra_value(conversation, key).is_some_and(|value| match value {
        Value::Number(number) => number.as_f64().unwrap_or_default() > 0.0,
        Value::String(value) => value.parse::<f64>().is_ok_and(|number| number > 0.0),
        _ => false,
    })
}

fn conversation_extra_non_zero_string(conversation: &SlackConversation, key: &str) -> bool {
    conversation_extra_value(conversation, key).is_some_and(|value| match value {
        Value::String(value) => {
            let value = value.trim();
            !value.is_empty() && value != "0" && value != "0.000000"
        }
        Value::Number(number) => number.as_f64().unwrap_or_default() > 0.0,
        _ => false,
    })
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
        }
    }

    fn list_placeholder(model: SidebarListModel) -> SidebarPlaceholder {
        match model {
            SidebarListModel::Placeholder(placeholder) => placeholder,
            SidebarListModel::Sections(_) => panic!("expected placeholder"),
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
    fn groups_channels_dms_and_group_dms_into_default_sections() {
        let mut user_names = HashMap::new();
        user_names.insert("U1".to_string(), "Ada Lovelace".to_string());

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
            vec!["Ada Lovelace"]
        );
        assert_eq!(
            titles(section(&sections, SidebarSectionKind::GroupDirectMessages)),
            vec!["triage"]
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
    fn default_sidebar_visibility_keeps_active_items_and_hides_dormant_dms() {
        let active_channel = channel("C1", "general");
        let active_dm: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "D1",
            "user": "U1",
            "is_im": true,
            "priority": 0.42
        }))
        .expect("failed to parse active DM");
        let dormant_dm: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "D2",
            "user": "U2",
            "is_im": true,
            "properties": {
                "is_dormant": true
            }
        }))
        .expect("failed to parse dormant DM");
        let unopened_dm = dm("D3", "U3");

        assert!(conversation_visible_in_default_sidebar(
            &active_channel,
            None
        ));
        assert!(conversation_visible_in_default_sidebar(&active_dm, None));
        assert!(!conversation_visible_in_default_sidebar(&dormant_dm, None));
        assert!(!conversation_visible_in_default_sidebar(&unopened_dm, None));
    }

    #[test]
    fn default_sidebar_visibility_keeps_unread_and_selected_hidden_items() {
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

        let sections = list_sections(build_sidebar_list(
            &[unread.clone(), read.clone(), badgeless_unread_dm.clone()],
            &user_names,
            SidebarBuildOptions {
                query: "ada",
                unread_only: true,
                ..Default::default()
            },
        ));

        assert!(sections
            .iter()
            .all(|section| section.kind != SidebarSectionKind::Unreads));
        assert_eq!(
            titles(section(&sections, SidebarSectionKind::DirectMessages)),
            vec!["Ada"]
        );
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

        let items = conversation_switcher_items(&[active, dormant_dm], &user_names, "ada");

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

        let title_match =
            conversation_switcher_items(&[general.clone(), random.clone()], &HashMap::new(), "gen");
        let id_match = conversation_switcher_items(&[general, random], &HashMap::new(), "456");

        assert_eq!(title_match[0].id, "C123");
        assert_eq!(id_match[0].id, "C456");
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

        let items = conversation_switcher_items(&[group_dm], &user_names, "grace");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Ada Lovelace, Grace Hopper");
    }
}
