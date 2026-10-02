use crate::models::SlackConversation;
use std::collections::HashMap;

/// The canonical, revision-aware set of conversations for a workspace.
///
/// Membership snapshots are accumulated separately and committed atomically. Updates that
/// happen after a snapshot starts (for example, opening a DM) are protected from that older
/// snapshot when it eventually commits.
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

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Iterates borrowed conversations without constructing an owned presentation snapshot.
    pub(crate) fn iter(&self) -> impl Clone + Iterator<Item = &SlackConversation> {
        self.entries.values().map(|entry| &entry.conversation)
    }

    /// Creates a stable owned snapshot for persistence and explicit handoffs.
    pub(crate) fn conversations(&self) -> Vec<SlackConversation> {
        let mut conversations = self.iter().cloned().collect::<Vec<_>>();
        conversations.sort_by(|left, right| left.id.cmp(&right.id));
        conversations
    }

    /// Replaces one presentation row with the coordinator's complete
    /// revisioned value.
    pub(crate) fn upsert_authoritative(&mut self, conversation: SlackConversation) {
        let revision = self.next_revision();
        self.entries.insert(
            conversation.id.clone(),
            CatalogEntry {
                conversation,
                membership_revision: revision,
                metadata_revision: revision,
            },
        );
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
                    entry.membership_revision = entry.membership_revision.max(snapshot_revision);
                }
                None => {
                    self.entries.insert(
                        id,
                        CatalogEntry {
                            conversation: incoming,
                            membership_revision: snapshot_revision,
                            metadata_revision: snapshot_revision,
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

    pub(crate) fn advance_last_read(&mut self, id: &str, ts: &str) -> bool {
        let revision = self.next_revision();
        if let Some(entry) = self.entries.get_mut(id) {
            if entry.conversation.advance_last_read(ts) {
                entry.metadata_revision = revision;
                return true;
            }
        }
        false
    }

    pub(crate) fn set_last_read(&mut self, id: &str, ts: String) -> bool {
        let revision = self.next_revision();
        if let Some(entry) = self.entries.get_mut(id) {
            entry.conversation.last_read = Some(ts);
            entry.metadata_revision = revision;
            return true;
        }
        false
    }

    /// Upserts a conversation opened while a membership refresh may be in flight.
    pub(crate) fn upsert_opened(&mut self, conversation: SlackConversation) {
        let revision = self.next_revision();
        let id = conversation.id.clone();
        match self.entries.get_mut(&id) {
            Some(entry) => {
                merge_metadata(&mut entry.conversation, &conversation);
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
                    },
                );
            }
        }
    }

    /// Merges identity and presentation fields from a details response.
    pub(crate) fn upsert_metadata(&mut self, conversation: SlackConversation) {
        let revision = self.next_revision();
        let id = conversation.id.clone();
        match self.entries.get_mut(&id) {
            Some(entry) => {
                merge_metadata(&mut entry.conversation, &conversation);
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
                    },
                );
            }
        }
    }

    fn insert_cached(&mut self, conversation: SlackConversation) {
        let id = conversation.id.clone();
        match self.entries.get_mut(&id) {
            Some(entry) => {
                merge_metadata(&mut entry.conversation, &conversation);
            }
            None => {
                self.entries.insert(
                    id,
                    CatalogEntry {
                        conversation,
                        membership_revision: 0,
                        metadata_revision: 0,
                    },
                );
            }
        }
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
    merge_option(&mut current.is_starred, &incoming.is_starred);
    merge_option(&mut current.last_read, &incoming.last_read);
    merge_option(&mut current.unread_count, &incoming.unread_count);
    merge_option(&mut current.unread_count_display, &incoming.unread_count_display);

    for (key, value) in &incoming.extra {
        current.extra.insert(key.clone(), value.clone());
    }
}

fn merge_option<T: Clone>(current: &mut Option<T>, incoming: &Option<T>) {
    if let Some(value) = incoming {
        *current = Some(value.clone());
    }
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
        cached
            .extra
            .insert("topic".to_string(), json!("Cached topic"));

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
        assert_eq!(merged.extra["topic"], json!("Cached topic"));
        assert_eq!(merged.extra["purpose"], json!("Fresh purpose"));
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
    fn borrowed_iteration_reflects_catalog_updates_without_an_owned_snapshot() {
        let mut catalog =
            ConversationCatalog::from_cached([conversation("C1"), conversation("C2")]);
        catalog.remove("C1");
        catalog.upsert_authoritative(conversation("C3"));

        let mut ids = catalog
            .iter()
            .map(|conversation| conversation.id.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();

        assert_eq!(ids, vec!["C2", "C3"]);
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
    fn metadata_merge_applies_newer_details_over_cached_fields() {
        let mut cached = conversation("C1");
        cached.name = Some("old".into());
        cached
            .extra
            .insert("topic".to_string(), json!("Cached topic"));
        let mut catalog = ConversationCatalog::from_cached([cached]);

        let mut details = conversation("C1");
        details.name = Some("renamed".into());
        details.extra.insert("purpose".to_string(), json!("Fresh"));
        catalog.upsert_metadata(details);

        let merged = catalog.get("C1").unwrap();
        assert_eq!(merged.name.as_deref(), Some("renamed"));
        assert_eq!(merged.extra["topic"], json!("Cached topic"));
        assert_eq!(merged.extra["purpose"], json!("Fresh"));

        let new_details = conversation("D1");
        catalog.upsert_metadata(new_details);
        assert!(catalog.get("D1").is_some());
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
