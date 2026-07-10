use std::cmp::Reverse;
use std::collections::HashMap;

use crate::models::SlackConversation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    DirectMessage,
    GroupDirectMessage,
    PrivateChannel,
    PublicChannel,
    Unknown,
}

impl ActivityKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::DirectMessage => "Direct message",
            Self::GroupDirectMessage => "Group DM",
            Self::PrivateChannel => "Private channel",
            Self::PublicChannel => "Channel",
            Self::Unknown => "Conversation",
        }
    }

    fn sort_rank(self) -> u8 {
        match self {
            Self::DirectMessage => 0,
            Self::GroupDirectMessage => 1,
            Self::PrivateChannel => 2,
            Self::PublicChannel => 3,
            Self::Unknown => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityItem {
    pub channel_id: String,
    pub title: String,
    pub kind: ActivityKind,
    pub unread: bool,
    pub unread_count: u64,
}

impl ActivityItem {
    pub fn unread_label(&self) -> String {
        match self.unread_count {
            0 if self.unread => "Unread activity".to_string(),
            0 => "No unread activity".to_string(),
            1 => "1 unread".to_string(),
            count => format!("{count} unread"),
        }
    }
}

pub fn build_activity_items(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
) -> Vec<ActivityItem> {
    let mut items = conversations
        .iter()
        .filter_map(|conversation| {
            let unread = conversation.has_unread_activity();
            let unread_count = conversation.unread_activity_count();
            unread.then(|| ActivityItem {
                channel_id: conversation.id.clone(),
                title: conversation.display_name_with_users(user_names),
                kind: activity_kind(conversation),
                unread,
                unread_count,
            })
        })
        .collect::<Vec<_>>();

    items.sort_by_key(|item| {
        (
            item.kind.sort_rank(),
            Reverse(item.unread_count),
            item.title.to_lowercase(),
        )
    });
    items
}

fn activity_kind(conversation: &SlackConversation) -> ActivityKind {
    if conversation.is_im.unwrap_or(false) {
        ActivityKind::DirectMessage
    } else if conversation.is_mpim.unwrap_or(false) {
        ActivityKind::GroupDirectMessage
    } else if conversation.is_private.unwrap_or(false) || conversation.is_group.unwrap_or(false) {
        ActivityKind::PrivateChannel
    } else if conversation.is_channel.unwrap_or(false) {
        ActivityKind::PublicChannel
    } else {
        ActivityKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_items_include_only_unread_conversations_and_sort_by_attention() {
        let public = SlackConversation {
            id: "C1".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            unread_count: Some(8),
            ..Default::default()
        };
        let dm = SlackConversation {
            id: "D1".to_string(),
            user: Some("U1".to_string()),
            is_im: Some(true),
            unread_count: Some(1),
            ..Default::default()
        };
        let read = SlackConversation {
            id: "C2".to_string(),
            name: Some("read".to_string()),
            is_channel: Some(true),
            unread_count: Some(0),
            ..Default::default()
        };
        let names = HashMap::from([("U1".to_string(), "Ada".to_string())]);

        let items = build_activity_items(&[public, dm, read], &names);

        assert_eq!(
            items,
            vec![
                ActivityItem {
                    channel_id: "D1".to_string(),
                    title: "Ada".to_string(),
                    kind: ActivityKind::DirectMessage,
                    unread: true,
                    unread_count: 1,
                },
                ActivityItem {
                    channel_id: "C1".to_string(),
                    title: "#general".to_string(),
                    kind: ActivityKind::PublicChannel,
                    unread: true,
                    unread_count: 8,
                },
            ]
        );
    }

    #[test]
    fn activity_items_use_extra_unread_fields() {
        let mut conversation = SlackConversation {
            id: "G1".to_string(),
            name: Some("group".to_string()),
            is_mpim: Some(true),
            ..Default::default()
        };
        conversation
            .extra
            .insert("has_unreads".to_string(), serde_json::json!(true));

        let items = build_activity_items(&[conversation], &HashMap::new());

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ActivityKind::GroupDirectMessage);
        assert!(items[0].unread);
        assert_eq!(items[0].unread_count, 0);
        assert_eq!(items[0].unread_label(), "Unread activity");
    }
}
