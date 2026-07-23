use crate::models::{SlackConversation, SlackConversationUnreadSnapshot, SlackUnreadState};
use std::collections::HashMap;

/// The canonical, revision-aware set of conversations for a workspace.
///
/// Membership snapshots are accumulated separately and committed atomically. Updates that
/// happen after a snapshot starts (for example, opening a DM or receiving a realtime unread
/// event) are protected from that older snapshot when it eventually commits.
#[derive(Debug, Default)]
pub(crate) struct ConversationCatalog {
    entries: HashMap<String, CatalogEntry>,
    revision: u64,
    last_committed_snapshot: u64,
}

#[derive(Debug)]
struct CatalogEntry {
    conversation: SlackConversation,
    membership_revision: u64,
    metadata_revision: u64,
    unread_revision: u64,
}

#[derive(Debug)]
pub(crate) struct MembershipSnapshot {
    revision: u64,
    conversations: HashMap<String, SlackConversation>,
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
        self.entries.get(id).map(|entry| &entry.conversation)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn conversations(&self) -> Vec<SlackConversation> {
        let mut conversations = self
            .entries
            .values()
            .map(|entry| entry.conversation.clone())
            .collect::<Vec<_>>();
        conversations.sort_by(|left, right| left.id.cmp(&right.id));
        conversations
    }

    /// Removes a conversation after membership has ended locally or remotely.
    pub(crate) fn remove(&mut self, id: &str) -> Option<SlackConversation> {
        let revision = self.next_revision();
        self.last_committed_snapshot = self.last_committed_snapshot.max(revision);
        self.entries.remove(id).map(|entry| entry.conversation)
    }

    pub(crate) fn begin_membership_snapshot(&mut self) -> MembershipSnapshot {
        MembershipSnapshot {
            revision: self.next_revision(),
            conversations: HashMap::new(),
        }
    }

    /// Commits a complete, authoritative membership snapshot.
    ///
    /// Returns `false` when a newer snapshot has already committed. This makes overlapping
    /// refreshes safe even if their responses finish out of order.
    pub(crate) fn commit_membership_snapshot(&mut self, snapshot: MembershipSnapshot) -> bool {
        if snapshot.revision < self.last_committed_snapshot {
            return false;
        }

        let snapshot_revision = snapshot.revision;
        for (id, incoming) in snapshot.conversations {
            match self.entries.get_mut(&id) {
                Some(entry) => {
                    if entry.metadata_revision <= snapshot_revision {
                        merge_metadata(&mut entry.conversation, &incoming);
                        entry.metadata_revision = snapshot_revision;
                    }
                    // Membership payloads are limited snapshots and can race newer
                    // realtime/local read state. Existing unread overlays are updated
                    // only through explicit unread patches.
                    entry.membership_revision = entry.membership_revision.max(snapshot_revision);
                }
                None => {
                    self.entries.insert(
                        id,
                        CatalogEntry {
                            conversation: incoming,
                            membership_revision: snapshot_revision,
                            metadata_revision: snapshot_revision,
                            unread_revision: snapshot_revision,
                        },
                    );
                }
            }
        }

        self.entries
            .retain(|_, entry| entry.membership_revision >= snapshot_revision);
        self.last_committed_snapshot = snapshot_revision;
        true
    }

    /// Upserts a conversation opened while a membership refresh may be in flight.
    pub(crate) fn upsert_opened(&mut self, conversation: SlackConversation) {
        let revision = self.next_revision();
        let id = conversation.id.clone();
        match self.entries.get_mut(&id) {
            Some(entry) => {
                merge_metadata(&mut entry.conversation, &conversation);
                if conversation.unread_state().known {
                    replace_unread_fields(&mut entry.conversation, &conversation);
                    entry.unread_revision = revision;
                }
                entry.membership_revision = revision;
                entry.metadata_revision = revision;
            }
            None => {
                self.entries.insert(
                    id,
                    CatalogEntry {
                        conversation,
                        membership_revision: revision,
                        metadata_revision: revision,
                        unread_revision: revision,
                    },
                );
            }
        }
    }

    /// Merges identity and presentation fields without allowing a delayed
    /// details response to replace newer local/realtime unread state.
    pub(crate) fn upsert_metadata(&mut self, mut conversation: SlackConversation) {
        let revision = self.next_revision();
        let id = conversation.id.clone();
        match self.entries.get_mut(&id) {
            Some(entry) => {
                merge_metadata(&mut entry.conversation, &conversation);
                entry.membership_revision = revision;
                entry.metadata_revision = revision;
            }
            None => {
                strip_unread_fields(&mut conversation);
                self.entries.insert(
                    id,
                    CatalogEntry {
                        conversation,
                        membership_revision: revision,
                        metadata_revision: revision,
                        unread_revision: revision,
                    },
                );
            }
        }
    }

    pub(crate) fn apply_realtime_unread(&mut self, id: &str, state: SlackUnreadState) {
        self.apply_unread(id, state);
    }

    /// Applies Conduit's message-level attention classification without
    /// overwriting the raw unread counters received from Slack.
    ///
    /// Returns whether the conversation metadata was already present.
    pub(crate) fn observe_attention_message(
        &mut self,
        id: &str,
        message_ts: &str,
        record_unread: bool,
    ) -> bool {
        if id.trim().is_empty() || message_ts.trim().is_empty() {
            return false;
        }
        let existed = self.entries.contains_key(id);
        let revision = self.next_revision();
        let entry = self
            .entries
            .entry(id.to_string())
            .or_insert_with(|| CatalogEntry {
                conversation: SlackConversation {
                    id: id.to_string(),
                    ..SlackConversation::default()
                },
                membership_revision: revision,
                metadata_revision: revision,
                unread_revision: revision,
            });
        entry
            .conversation
            .observe_attention_message_at(message_ts, record_unread);
        entry.unread_revision = revision;
        entry.membership_revision = entry.membership_revision.max(revision);
        existed
    }

    pub(crate) fn apply_unread_snapshot(&mut self, snapshot: &SlackConversationUnreadSnapshot) {
        if !snapshot.unread_state.known || snapshot.channel_id.trim().is_empty() {
            return;
        }

        let revision = self.next_revision();
        let entry = self
            .entries
            .entry(snapshot.channel_id.clone())
            .or_insert_with(|| CatalogEntry {
                conversation: SlackConversation {
                    id: snapshot.channel_id.clone(),
                    ..SlackConversation::default()
                },
                membership_revision: revision,
                metadata_revision: revision,
                unread_revision: revision,
            });
        entry.conversation.apply_unread_snapshot(snapshot);
        entry.unread_revision = revision;
        entry.membership_revision = entry.membership_revision.max(revision);
    }

    pub(crate) fn advance_read_cursor(&mut self, id: &str, ts: &str, remaining_unread: u64) {
        self.apply_unread(
            id,
            SlackUnreadState::from_parts(true, remaining_unread > 0, remaining_unread),
        );
        if let Some(entry) = self.entries.get_mut(id) {
            entry.conversation.advance_read_cursor(ts, remaining_unread);
        }
    }

    fn insert_cached(&mut self, conversation: SlackConversation) {
        let id = conversation.id.clone();
        match self.entries.get_mut(&id) {
            Some(entry) => {
                merge_metadata(&mut entry.conversation, &conversation);
                if conversation.unread_state().known {
                    replace_unread_fields(&mut entry.conversation, &conversation);
                }
            }
            None => {
                self.entries.insert(
                    id,
                    CatalogEntry {
                        conversation,
                        membership_revision: 0,
                        metadata_revision: 0,
                        unread_revision: 0,
                    },
                );
            }
        }
    }

    fn apply_unread(&mut self, id: &str, state: SlackUnreadState) {
        if !state.known {
            return;
        }

        let revision = self.next_revision();
        let entry = self
            .entries
            .entry(id.to_string())
            .or_insert_with(|| CatalogEntry {
                conversation: SlackConversation {
                    id: id.to_string(),
                    ..SlackConversation::default()
                },
                membership_revision: revision,
                metadata_revision: revision,
                unread_revision: revision,
            });
        entry.conversation.apply_unread_state(state);
        entry.unread_revision = revision;
        entry.membership_revision = entry.membership_revision.max(revision);
    }

    fn next_revision(&mut self) -> u64 {
        self.revision = self.revision.saturating_add(1);
        self.revision
    }
}

impl MembershipSnapshot {
    /// Adds one page/item to the in-progress snapshot. Duplicate sparse objects are merged.
    pub(crate) fn upsert(&mut self, conversation: SlackConversation) {
        let id = conversation.id.clone();
        match self.conversations.get_mut(&id) {
            Some(existing) => {
                merge_metadata(existing, &conversation);
                if conversation.unread_state().known {
                    replace_unread_fields(existing, &conversation);
                }
            }
            None => {
                self.conversations.insert(id, conversation);
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
    key.to_ascii_lowercase().contains("unread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conversation(id: &str) -> SlackConversation {
        SlackConversation {
            id: id.to_string(),
            ..SlackConversation::default()
        }
    }

    #[test]
    fn sparse_fresh_snapshot_preserves_enriched_cached_fields() {
        let mut cached = conversation("C1");
        cached.name = Some("old-name".to_string());
        cached.is_private = Some(true);
        cached.unread_count = Some(4);
        cached
            .extra
            .insert("topic".to_string(), json!("Cached topic"));
        cached
            .extra
            .insert("unread_count_display".to_string(), json!(4));

        let mut catalog = ConversationCatalog::from_cached([cached]);
        let mut snapshot = catalog.begin_membership_snapshot();
        let mut fresh = conversation("C1");
        fresh.name = Some("fresh-name".to_string());
        fresh.is_channel = Some(true);
        fresh
            .extra
            .insert("purpose".to_string(), json!("Fresh purpose"));
        snapshot.upsert(fresh);
        assert!(catalog.commit_membership_snapshot(snapshot));

        let merged = catalog.get("C1").unwrap();
        assert_eq!(merged.name.as_deref(), Some("fresh-name"));
        assert_eq!(merged.is_private, Some(true));
        assert_eq!(merged.is_channel, Some(true));
        assert_eq!(merged.unread_state().display_count, 4);
        assert_eq!(merged.extra["topic"], json!("Cached topic"));
        assert_eq!(merged.extra["purpose"], json!("Fresh purpose"));
    }

    #[test]
    fn complete_snapshot_authoritatively_removes_missing_memberships() {
        let mut catalog =
            ConversationCatalog::from_cached([conversation("C1"), conversation("C2")]);
        let mut snapshot = catalog.begin_membership_snapshot();
        snapshot.upsert(conversation("C1"));

        assert!(catalog.commit_membership_snapshot(snapshot));
        assert_eq!(catalog.len(), 1);
        assert!(catalog.get("C1").is_some());
        assert!(catalog.get("C2").is_none());
    }

    #[test]
    fn explicit_removal_returns_and_forgets_the_conversation() {
        let mut catalog =
            ConversationCatalog::from_cached([conversation("C1"), conversation("C2")]);
        let mut stale_snapshot = catalog.begin_membership_snapshot();
        stale_snapshot.upsert(conversation("C1"));

        assert_eq!(catalog.remove("C1").map(|item| item.id), Some("C1".into()));
        assert!(catalog.get("C1").is_none());
        assert_eq!(catalog.len(), 1);
        assert!(!catalog.commit_membership_snapshot(stale_snapshot));
        assert!(catalog.get("C1").is_none());
        assert!(catalog.remove("missing").is_none());
    }

    #[test]
    fn conversation_opened_during_refresh_survives_that_snapshot() {
        let mut catalog = ConversationCatalog::from_cached([conversation("C1")]);
        let mut snapshot = catalog.begin_membership_snapshot();
        snapshot.upsert(conversation("C1"));

        let mut opened = conversation("D1");
        opened.is_im = Some(true);
        catalog.upsert_opened(opened);
        assert!(catalog.commit_membership_snapshot(snapshot));

        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog.get("D1").and_then(|item| item.is_im), Some(true));
    }

    #[test]
    fn realtime_unread_beats_an_older_snapshot() {
        let mut cached = conversation("C1");
        cached.unread_count = Some(0);
        let mut catalog = ConversationCatalog::from_cached([cached]);
        let mut snapshot = catalog.begin_membership_snapshot();
        let mut stale = conversation("C1");
        stale.unread_count = Some(1);
        snapshot.upsert(stale);

        catalog.apply_realtime_unread("C1", SlackUnreadState::from_parts(true, true, 7));
        assert!(catalog.commit_membership_snapshot(snapshot));

        assert_eq!(catalog.get("C1").unwrap().unread_state().display_count, 7);
    }

    #[test]
    fn attention_projection_filters_noise_and_read_marker_clears_local_unread() {
        let mut cached = conversation("C1");
        cached.unread_count = Some(0);
        let mut catalog = ConversationCatalog::from_cached([cached]);

        assert!(catalog.observe_attention_message("C1", "1.0", false));
        catalog.apply_realtime_unread("C1", SlackUnreadState::from_parts(true, true, 1));
        let filtered = catalog.get("C1").unwrap();
        assert_eq!(filtered.raw_unread_activity_count(), 1);
        assert!(!filtered.has_unread_activity());

        assert!(catalog.observe_attention_message("C1", "2.0", true));
        assert_eq!(catalog.get("C1").unwrap().unread_activity_count(), 1);
        catalog.advance_read_cursor("C1", "20.0", 0);
        assert!(!catalog.get("C1").unwrap().has_unread_activity());
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
        cached.extra.insert("has_unreads".to_string(), json!(false));
        let mut catalog = ConversationCatalog::from_cached([cached]);

        let mut stale_details = conversation("C1");
        stale_details.name = Some("renamed".into());
        stale_details.unread_count = Some(9);
        stale_details
            .extra
            .insert("has_unreads".to_string(), json!(true));
        catalog.upsert_metadata(stale_details);

        let merged = catalog.get("C1").unwrap();
        assert_eq!(merged.name.as_deref(), Some("renamed"));
        assert_eq!(merged.unread_activity_count(), 0);

        let mut new_details = conversation("D1");
        new_details.unread_count = Some(4);
        catalog.upsert_metadata(new_details);
        assert!(!catalog.get("D1").unwrap().unread_state().known);
    }

    #[test]
    fn local_mark_read_beats_an_older_unread_snapshot() {
        let mut cached = conversation("C1");
        cached.unread_count = Some(5);
        let mut catalog = ConversationCatalog::from_cached([cached]);
        let mut snapshot = catalog.begin_membership_snapshot();
        let mut stale = conversation("C1");
        stale.unread_count = Some(5);
        snapshot.upsert(stale);

        catalog.advance_read_cursor("C1", "20.0", 0);
        assert!(catalog.commit_membership_snapshot(snapshot));

        let unread = catalog.get("C1").unwrap().unread_state();
        assert!(unread.known);
        assert!(!unread.has_unread);
        assert_eq!(unread.display_count, 0);
    }

    #[test]
    fn sparse_objects_merge_field_by_field_within_a_snapshot() {
        let mut catalog = ConversationCatalog::default();
        let mut snapshot = catalog.begin_membership_snapshot();
        let mut first = conversation("C1");
        first.name = Some("general".to_string());
        first.is_channel = Some(true);
        first.extra.insert("topic".to_string(), json!("One"));
        snapshot.upsert(first);

        let mut second = conversation("C1");
        second.is_private = Some(false);
        second.extra.insert("purpose".to_string(), json!("Two"));
        snapshot.upsert(second);
        assert!(catalog.commit_membership_snapshot(snapshot));

        let merged = catalog.get("C1").unwrap();
        assert_eq!(merged.name.as_deref(), Some("general"));
        assert_eq!(merged.is_channel, Some(true));
        assert_eq!(merged.is_private, Some(false));
        assert_eq!(merged.extra["topic"], json!("One"));
        assert_eq!(merged.extra["purpose"], json!("Two"));
    }

    #[test]
    fn older_overlapping_snapshot_cannot_replace_a_newer_commit() {
        let mut catalog = ConversationCatalog::default();
        let mut older = catalog.begin_membership_snapshot();
        older.upsert(conversation("OLD"));
        let mut newer = catalog.begin_membership_snapshot();
        newer.upsert(conversation("NEW"));

        assert!(catalog.commit_membership_snapshot(newer));
        assert!(!catalog.commit_membership_snapshot(older));
        assert!(catalog.get("NEW").is_some());
        assert!(catalog.get("OLD").is_none());
    }
}
