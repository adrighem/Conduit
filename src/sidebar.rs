use std::cmp::Reverse;
use std::collections::HashMap;

use crate::models::SlackConversation;

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
            Self::PublicChannel => "channel-insecure-symbolic",
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
    pub unread_count: u64,
    pub selected: bool,
    pub private: bool,
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
        }
        if self.selected {
            label.push_str(", selected");
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

pub fn build_sidebar_sections(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    selected_channel: Option<&str>,
) -> Vec<SidebarSectionModel> {
    let mut unreads = Vec::new();
    let mut channels = Vec::new();
    let mut direct_messages = Vec::new();
    let mut group_direct_messages = Vec::new();
    let mut other = Vec::new();

    for conversation in conversations
        .iter()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
    {
        let row = SidebarRowModel {
            id: conversation.id.clone(),
            title: conversation.display_name_with_users(user_names),
            kind: conversation_kind(conversation),
            unread_count: conversation.unread_count.unwrap_or_default(),
            selected: selected_channel == Some(conversation.id.as_str()),
            private: conversation.is_private.unwrap_or(false)
                || conversation.is_group.unwrap_or(false)
                || matches!(
                    conversation_kind(conversation),
                    ConversationKind::PrivateChannel
                ),
        };

        if row.unread_count > 0 {
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

    fn row(title: &str, unread_count: u64, selected: bool) -> SidebarRowModel {
        SidebarRowModel {
            id: title.to_string(),
            title: title.to_string(),
            kind: ConversationKind::PublicChannel,
            unread_count,
            selected,
            private: false,
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
        assert_eq!(
            row("#general", 3, true).accessible_label(),
            "Public channel: #general, 3 unread, selected"
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
}
