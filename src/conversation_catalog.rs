use crate::models::{
    conversation_metadata_key_is_unread_owned, SlackConversation, SlackConversationUnreadSnapshot,
};
use std::collections::HashMap;

/// The presentation projection of the coordinator's canonical conversations.
#[derive(Debug, Default)]
pub(crate) struct ConversationCatalog {
    entries: HashMap<String, SlackConversation>,
}

impl ConversationCatalog {
    pub(crate) fn from_cached(conversations: impl IntoIterator<Item = SlackConversation>) -> Self {
        let mut catalog = Self::default();
        for conversation in conversations {
            catalog.insert_cached(conversation);
        }
        catalog
    }

    pub(crate) fn get(&self, id: &str) -> Option<&SlackConversation> {
        self.entries.get(id)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn conversations(&self) -> Vec<SlackConversation> {
        let mut conversations = self.entries.values().cloned().collect::<Vec<_>>();
        conversations.sort_by(|left, right| left.id.cmp(&right.id));
        conversations
    }

    /// Replaces one presentation row with the coordinator's complete value.
    pub(crate) fn upsert_authoritative(&mut self, conversation: SlackConversation) {
        self.entries.insert(conversation.id.clone(), conversation);
    }

    /// Removes a conversation after membership has ended locally or remotely.
    pub(crate) fn remove(&mut self, id: &str) -> Option<SlackConversation> {
        self.entries.remove(id)
    }

    /// Merges identity and presentation fields without allowing a delayed
    /// details response to replace newer local/realtime unread state.
    pub(crate) fn upsert_metadata(&mut self, mut conversation: SlackConversation) {
        let id = conversation.id.clone();
        match self.entries.get_mut(&id) {
            Some(current) => {
                merge_metadata(current, &conversation);
            }
            None => {
                strip_unread_fields(&mut conversation);
                self.entries.insert(id, conversation);
            }
        }
    }

    /// Applies one semantic attention observation without replacing any
    /// conversation fields. Returns `(conversation_existed, changed)`.
    pub(crate) fn apply_attention_observation(
        &mut self,
        id: &str,
        message_ts: &str,
        record_unread: bool,
    ) -> (bool, bool) {
        if id.trim().is_empty() || message_ts.trim().is_empty() {
            return (false, false);
        }
        let existed = self.entries.contains_key(id);
        if self
            .entries
            .get(id)
            .is_some_and(|conversation| conversation.has_observed_attention_message(message_ts))
        {
            return (existed, false);
        }
        let conversation =
            self.entries
                .entry(id.to_string())
                .or_insert_with(|| SlackConversation {
                    id: id.to_string(),
                    ..SlackConversation::default()
                });
        let changed = conversation.observe_attention_message_at(message_ts, record_unread);
        (existed, changed)
    }

    pub(crate) fn apply_unread_snapshot(
        &mut self,
        snapshot: &SlackConversationUnreadSnapshot,
    ) -> bool {
        if !snapshot.unread_state.known || snapshot.channel_id.trim().is_empty() {
            return false;
        }

        let before = self.get(&snapshot.channel_id).cloned();
        let conversation = self
            .entries
            .entry(snapshot.channel_id.clone())
            .or_insert_with(|| SlackConversation {
                id: snapshot.channel_id.clone(),
                ..SlackConversation::default()
            });
        conversation.apply_unread_snapshot(snapshot);
        before.as_ref() != Some(conversation)
    }

    fn insert_cached(&mut self, conversation: SlackConversation) {
        let id = conversation.id.clone();
        match self.entries.get_mut(&id) {
            Some(current) => {
                merge_metadata(current, &conversation);
                if conversation.unread_state().known {
                    replace_unread_fields(current, &conversation);
                }
            }
            None => {
                self.entries.insert(id, conversation);
            }
        }
    }
}

fn merge_metadata(current: &mut SlackConversation, incoming: &SlackConversation) {
    merge_option(&mut current.name, &incoming.name);
    merge_option(&mut current.user, &incoming.user);
    merge_option(&mut current.is_channel, &incoming.is_channel);
    merge_option(&mut current.is_group, &incoming.is_group);
    merge_option(&mut current.is_im, &incoming.is_im);
    merge_option(&mut current.is_mpim, &incoming.is_mpim);
    merge_option(&mut current.is_private, &incoming.is_private);
    merge_option(&mut current.is_archived, &incoming.is_archived);
    merge_option(&mut current.is_starred, &incoming.is_starred);

    for (key, value) in &incoming.extra {
        if !is_unread_key(key) {
            current.extra.insert(key.clone(), value.clone());
        }
    }
}

fn replace_unread_fields(current: &mut SlackConversation, incoming: &SlackConversation) {
    current.unread_count = incoming.unread_count;
    current.extra.retain(|key, _| !is_unread_key(key));
    current.extra.extend(
        incoming
            .extra
            .iter()
            .filter(|(key, _)| is_unread_key(key))
            .map(|(key, value)| (key.clone(), value.clone())),
    );
}

fn strip_unread_fields(conversation: &mut SlackConversation) {
    conversation.unread_count = None;
    conversation.extra.retain(|key, _| !is_unread_key(key));
}

fn merge_option<T: Clone>(current: &mut Option<T>, incoming: &Option<T>) {
    if let Some(value) = incoming {
        *current = Some(value.clone());
    }
}

fn is_unread_key(key: &str) -> bool {
    conversation_metadata_key_is_unread_owned(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::SlackUnreadState;
    use serde_json::json;

    fn conversation(id: &str) -> SlackConversation {
        SlackConversation {
            id: id.to_string(),
            ..SlackConversation::default()
        }
    }

    #[test]
    fn metadata_merge_applies_explicit_conversation_star_changes() {
        let mut cached = conversation("C1");
        cached.is_starred = Some(true);
        let mut catalog = ConversationCatalog::from_cached([cached]);

        let mut update = conversation("C1");
        update.is_starred = Some(false);
        catalog.upsert_metadata(update);

        assert_eq!(catalog.get("C1").unwrap().is_starred, Some(false));
    }

    #[test]
    fn explicit_removal_returns_and_forgets_the_conversation() {
        let mut catalog =
            ConversationCatalog::from_cached([conversation("C1"), conversation("C2")]);

        assert_eq!(catalog.remove("C1").map(|item| item.id), Some("C1".into()));
        assert!(catalog.get("C1").is_none());
        assert_eq!(catalog.len(), 1);
        assert!(catalog.remove("missing").is_none());
    }

    #[test]
    fn authoritative_upsert_replaces_the_complete_projection() {
        let mut catalog = ConversationCatalog::from_cached([conversation("C1")]);
        let mut authoritative = conversation("C1");
        authoritative.is_im = Some(true);
        authoritative.unread_count = Some(3);
        catalog.upsert_authoritative(authoritative);

        let current = catalog.get("C1").unwrap();
        assert_eq!(current.is_im, Some(true));
        assert_eq!(current.unread_count, Some(3));
    }

    #[test]
    fn attention_observation_reports_presence_and_deduplicates() {
        let mut cached = conversation("C1");
        cached.unread_count = Some(0);
        let mut catalog = ConversationCatalog::from_cached([cached]);

        assert_eq!(
            catalog.apply_attention_observation("C1", "2.0", true),
            (true, true)
        );
        assert_eq!(
            catalog.apply_attention_observation("C1", "2.0", true),
            (true, false)
        );
        assert_eq!(catalog.get("C1").unwrap().unread_activity_count(), 1);
        assert_eq!(
            catalog.apply_attention_observation("D1", "3.0", false),
            (false, true)
        );
    }

    #[test]
    fn unread_snapshot_updates_sidebar_state_and_activity_metadata_together() {
        let mut direct_message = conversation("D1");
        direct_message.is_im = Some(true);
        let mut catalog = ConversationCatalog::from_cached([direct_message]);

        catalog.apply_unread_snapshot(&SlackConversationUnreadSnapshot {
            channel_id: "D1".to_string(),
            unread_state: SlackUnreadState::from_parts(true, true, 0),
            last_read: Some("10.0".to_string()),
            latest: Some("11.0".to_string()),
            mention_count: Some(2),
            is_open: Some(true),
        });

        let current = catalog.get("D1").unwrap();
        assert!(current.has_unread_activity());
        assert_eq!(current.unread_activity_count(), 0);
        assert_eq!(current.last_read_ts(), Some("10.0"));
        assert_eq!(current.latest_message_ts(), Some("11.0"));
        assert!(current.has_active_direct_message_hint());
    }

    #[test]
    fn metadata_merge_never_overwrites_unread_state() {
        let mut cached = conversation("C1");
        cached.name = Some("old".into());
        cached.unread_count = Some(0);
        cached.extra.extend(HashMap::from([
            ("has_unreads".to_string(), json!(false)),
            ("last_read".to_string(), json!("20.0")),
            ("latest".to_string(), json!("21.0")),
            ("mention_count".to_string(), json!(2)),
            ("is_open".to_string(), json!(true)),
            (crate::models::LOCAL_READ_TS_KEY.to_string(), json!("20.0")),
        ]));
        let mut catalog = ConversationCatalog::from_cached([cached]);

        let mut stale_details = conversation("C1");
        stale_details.name = Some("renamed".into());
        stale_details.unread_count = Some(9);
        stale_details.extra.extend(HashMap::from([
            ("has_unreads".to_string(), json!(true)),
            ("last_read".to_string(), json!("10.0")),
            ("latest".to_string(), json!("11.0")),
            ("mention_count".to_string(), json!(9)),
            ("is_open".to_string(), json!(false)),
            (crate::models::LOCAL_READ_TS_KEY.to_string(), json!("10.0")),
        ]));
        catalog.upsert_metadata(stale_details);

        let merged = catalog.get("C1").unwrap();
        assert_eq!(merged.name.as_deref(), Some("renamed"));
        assert_eq!(merged.unread_activity_count(), 0);
        assert_eq!(merged.last_read_ts(), Some("20.0"));
        assert_eq!(merged.latest_message_ts(), Some("21.0"));
        assert_eq!(merged.extra["mention_count"], json!(2));
        assert_eq!(merged.extra["is_open"], json!(true));
        assert_eq!(merged.local_read_ts(), Some("20.0"));

        let mut new_details = conversation("D1");
        new_details.unread_count = Some(4);
        catalog.upsert_metadata(new_details);
        assert!(!catalog.get("D1").unwrap().unread_state().known);
    }
}
