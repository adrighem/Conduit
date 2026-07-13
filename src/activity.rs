use std::cmp::Reverse;
use std::collections::HashMap;

use gettextrs::{gettext, ngettext};

use crate::models::SlackConversation;
use crate::thread_catalog::{ThreadRecord, ThreadUnreadState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    DirectMessage,
    GroupDirectMessage,
    PrivateChannel,
    PublicChannel,
    Thread,
    Unknown,
}

impl ActivityKind {
    pub fn label(self) -> String {
        match self {
            Self::DirectMessage => gettext("Direct message"),
            Self::GroupDirectMessage => gettext("Group DM"),
            Self::PrivateChannel => gettext("Private channel"),
            Self::PublicChannel => gettext("Channel"),
            Self::Thread => gettext("Thread"),
            Self::Unknown => gettext("Conversation"),
        }
    }

    fn sort_rank(self) -> u8 {
        match self {
            Self::Thread => 0,
            Self::DirectMessage => 1,
            Self::GroupDirectMessage => 2,
            Self::PrivateChannel => 3,
            Self::PublicChannel => 4,
            Self::Unknown => 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityItem {
    pub channel_id: String,
    pub thread_ts: Option<String>,
    pub title: String,
    pub kind: ActivityKind,
    pub unread: bool,
    pub unread_count: u64,
}

impl ActivityItem {
    pub fn unread_label(&self) -> String {
        match self.unread_count {
            0 if self.unread => gettext("Unread conversation"),
            0 => gettext("No unread conversation"),
            count => ngettext(
                "1 unread",
                "{count} unread",
                count.min(u32::MAX.into()) as u32,
            )
            .replace("{count}", &count.to_string()),
        }
    }
}

pub fn build_activity_items(
    conversations: &[SlackConversation],
    user_names: &HashMap<String, String>,
    current_user_id: Option<&str>,
) -> Vec<ActivityItem> {
    let mut items = conversations
        .iter()
        .filter_map(|conversation| {
            let unread = conversation.has_unread_activity();
            let unread_count = conversation.unread_activity_count();
            unread.then(|| ActivityItem {
                channel_id: conversation.id.clone(),
                thread_ts: None,
                title: conversation.display_name_with_users(user_names, current_user_id),
                kind: activity_kind(conversation),
                unread,
                unread_count,
            })
        })
        .collect::<Vec<_>>();

    sort_activity_items(&mut items);
    items
}

pub fn sort_activity_items(items: &mut [ActivityItem]) {
    items.sort_by_key(|item| {
        (
            item.kind.sort_rank(),
            Reverse(item.unread_count),
            item.title.to_lowercase(),
        )
    });
}

pub fn build_thread_activity_items(
    records: impl IntoIterator<Item = ThreadRecord>,
    conversation_titles: &HashMap<String, String>,
) -> Vec<ActivityItem> {
    let mut items = records
        .into_iter()
        .filter_map(|record| {
            if record.subscribed == Some(false) {
                return None;
            }
            let ThreadUnreadState::Known { count, .. } = record.unread else {
                return None;
            };
            if count == 0 {
                return None;
            }
            let channel_title = conversation_titles
                .get(&record.key.channel_id)
                .cloned()
                .unwrap_or_else(|| record.key.channel_id.clone());
            let root_text = record
                .root
                .as_ref()
                .and_then(|root| root.text.as_deref())
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .unwrap_or("Thread");
            Some(ActivityItem {
                channel_id: record.key.channel_id,
                thread_ts: Some(record.key.root_ts),
                title: format!("{channel_title}: {root_text}"),
                kind: ActivityKind::Thread,
                unread: true,
                unread_count: count,
            })
        })
        .collect::<Vec<_>>();
    sort_activity_items(&mut items);
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
    use crate::models::SlackMessage;
    use crate::thread_catalog::ThreadCatalog;

    #[test]
    fn unread_items_include_only_unread_conversations_and_sort_by_attention() {
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

        let items = build_activity_items(&[public, dm, read], &names, None);

        assert_eq!(
            items,
            vec![
                ActivityItem {
                    channel_id: "D1".to_string(),
                    thread_ts: None,
                    title: "Ada".to_string(),
                    kind: ActivityKind::DirectMessage,
                    unread: true,
                    unread_count: 1,
                },
                ActivityItem {
                    channel_id: "C1".to_string(),
                    thread_ts: None,
                    title: "#general".to_string(),
                    kind: ActivityKind::PublicChannel,
                    unread: true,
                    unread_count: 8,
                },
            ]
        );
    }

    #[test]
    fn unread_items_use_extra_unread_fields() {
        let mut conversation = SlackConversation {
            id: "G1".to_string(),
            name: Some("group".to_string()),
            is_mpim: Some(true),
            ..Default::default()
        };
        conversation
            .extra
            .insert("has_unreads".to_string(), serde_json::json!(true));

        let items = build_activity_items(&[conversation], &HashMap::new(), None);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ActivityKind::GroupDirectMessage);
        assert!(items[0].unread);
        assert_eq!(items[0].unread_count, 0);
        assert_eq!(items[0].unread_label(), "Unread conversation");
    }

    #[test]
    fn group_dm_activity_title_excludes_current_user() {
        let conversation: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G1",
            "is_mpim": true,
            "members": ["U_SELF", "U1", "U2"],
            "unread_count": 1
        }))
        .expect("failed to parse group direct message");
        let names = HashMap::from([
            ("U_SELF".to_string(), "Vincent".to_string()),
            ("U1".to_string(), "Fatima".to_string()),
            ("U2".to_string(), "Robey".to_string()),
        ]);

        let items = build_activity_items(&[conversation], &names, Some("U_SELF"));

        assert_eq!(items[0].title, "Fatima, Robey");
    }

    #[test]
    fn unread_thread_records_become_navigable_activity_items() {
        let mut catalog = ThreadCatalog::default();
        catalog.observe_thread(
            "C1",
            "1.0",
            &[SlackMessage {
                ts: "1.0".to_string(),
                text: Some("Deployment status".to_string()),
                subscribed: Some(true),
                unread_count: Some(2),
                ..Default::default()
            }],
            false,
        );

        let items = build_thread_activity_items(
            catalog.into_records(),
            &HashMap::from([("C1".to_string(), "#general".to_string())]),
        );

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].thread_ts.as_deref(), Some("1.0"));
        assert_eq!(items[0].title, "#general: Deployment status");
        assert_eq!(items[0].unread_count, 2);
        assert_eq!(items[0].kind, ActivityKind::Thread);
    }
}
