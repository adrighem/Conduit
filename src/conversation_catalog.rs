use crate::models::SlackConversation;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Source of [`ConversationCatalog::content_stamp`] values. Process-wide, so
/// a stamp never repeats even when a catalog is replaced wholesale.
static NEXT_CONTENT_STAMP: AtomicU64 = AtomicU64::new(1);

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
    /// Changes on every mutation and is unique across catalogs; `0` only for
    /// a never-mutated (empty) catalog. See [`ConversationTitlesCache`].
    content_stamp: u64,
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

    /// Cheap change marker: equal stamps imply equal contents.
    pub(crate) fn content_stamp(&self) -> u64 {
        self.content_stamp
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
        self.touch();
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
        self.touch();
        self.revision = self.revision.saturating_add(1);
        self.revision
    }

    fn touch(&mut self) {
        self.content_stamp = NEXT_CONTENT_STAMP.fetch_add(1, Ordering::Relaxed);
    }
}

/// Conversation titles and privacy as message renderers read them, derived
/// from the catalog plus the channels discovered for the picker.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConversationTitles {
    pub(crate) titles: Arc<HashMap<String, String>>,
    pub(crate) private_ids: Arc<HashSet<String>>,
}

/// Rebuilds [`ConversationTitles`] only when an input changed, so renders
/// share one snapshot and render contexts compare it by pointer.
#[derive(Debug, Default)]
pub(crate) struct ConversationTitlesCache {
    entry: Option<(ConversationTitlesKey, ConversationTitles)>,
}

/// Holds clones of the shared inputs: while it does, copy-on-write mutation
/// (`Arc::make_mut`) must reallocate, so pointer equality means "unchanged".
#[derive(Debug)]
struct ConversationTitlesKey {
    user_names: Arc<HashMap<String, String>>,
    current_user_id: Option<String>,
    discovered: Arc<Vec<SlackConversation>>,
    catalog_stamp: u64,
}

impl ConversationTitlesKey {
    fn matches(
        &self,
        user_names: &Arc<HashMap<String, String>>,
        current_user_id: Option<&str>,
        discovered: &Arc<Vec<SlackConversation>>,
        catalog: &ConversationCatalog,
    ) -> bool {
        Arc::ptr_eq(&self.user_names, user_names)
            && self.current_user_id.as_deref() == current_user_id
            && Arc::ptr_eq(&self.discovered, discovered)
            && self.catalog_stamp == catalog.content_stamp()
    }
}

impl ConversationTitlesCache {
    pub(crate) fn get(
        &mut self,
        user_names: &Arc<HashMap<String, String>>,
        current_user_id: Option<&str>,
        discovered: &Arc<Vec<SlackConversation>>,
        catalog: &ConversationCatalog,
    ) -> ConversationTitles {
        if let Some((key, titles)) = &self.entry {
            if key.matches(user_names, current_user_id, discovered, catalog) {
                return titles.clone();
            }
        }
        // Catalog entries win over discovered channels with the same id.
        let conversations = discovered.iter().chain(catalog.iter());
        let titles = ConversationTitles {
            titles: Arc::new(
                conversations
                    .clone()
                    .map(|conversation| {
                        let title =
                            conversation.display_name_with_users(user_names, current_user_id);
                        (conversation.id.clone(), title)
                    })
                    .collect(),
            ),
            private_ids: Arc::new(
                conversations
                    .filter(|conversation| conversation.is_private.unwrap_or(false))
                    .map(|conversation| conversation.id.clone())
                    .collect(),
            ),
        };
        let key = ConversationTitlesKey {
            user_names: user_names.clone(),
            current_user_id: current_user_id.map(ToString::to_string),
            discovered: discovered.clone(),
            catalog_stamp: catalog.content_stamp(),
        };
        self.entry = Some((key, titles.clone()));
        titles
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
    fn content_stamp_changes_on_every_mutation_and_never_repeats() {
        let mut catalog = ConversationCatalog::from_cached([conversation("C1")]);
        let cached = catalog.content_stamp();
        assert_ne!(cached, ConversationCatalog::default().content_stamp());

        catalog.upsert_metadata(conversation("C2"));
        let upserted = catalog.content_stamp();
        assert_ne!(upserted, cached);

        catalog.remove("C2");
        assert_ne!(catalog.content_stamp(), upserted);

        // A wholesale replacement with the same contents still gets a fresh stamp.
        let replaced = ConversationCatalog::from_cached([conversation("C1")]);
        assert_ne!(replaced.content_stamp(), cached);
    }

    #[test]
    fn title_cache_reuses_the_snapshot_until_an_input_changes() {
        let mut general = conversation("C1");
        general.name = Some("general".to_string());
        general.is_private = Some(true);
        let mut catalog = ConversationCatalog::from_cached([general]);
        let mut discovered_channel = conversation("C2");
        discovered_channel.name = Some("random".to_string());
        let discovered = Arc::new(vec![discovered_channel]);
        let user_names = Arc::new(HashMap::new());
        let mut cache = ConversationTitlesCache::default();

        let first = cache.get(&user_names, None, &discovered, &catalog);
        assert_eq!(first.titles.len(), 2);
        assert!(first.private_ids.contains("C1"));
        assert!(!first.private_ids.contains("C2"));

        let again = cache.get(&user_names, None, &discovered, &catalog);
        assert!(Arc::ptr_eq(&first.titles, &again.titles));
        assert!(Arc::ptr_eq(&first.private_ids, &again.private_ids));

        let mut renamed = conversation("C1");
        renamed.name = Some("announcements".to_string());
        catalog.upsert_metadata(renamed);
        let after_catalog = cache.get(&user_names, None, &discovered, &catalog);
        assert!(!Arc::ptr_eq(&first.titles, &after_catalog.titles));
        assert_ne!(first.titles, after_catalog.titles);

        let mut shared_names = user_names.clone();
        Arc::make_mut(&mut shared_names).insert("U1".to_string(), "Ada".to_string());
        let after_users = cache.get(&shared_names, None, &discovered, &catalog);
        assert!(!Arc::ptr_eq(&after_catalog.titles, &after_users.titles));

        let after_user_id = cache.get(&shared_names, Some("U1"), &discovered, &catalog);
        assert!(!Arc::ptr_eq(&after_users.titles, &after_user_id.titles));

        let rediscovered = Arc::new(discovered.as_ref().clone());
        let after_discovery = cache.get(&shared_names, Some("U1"), &rediscovered, &catalog);
        assert!(!Arc::ptr_eq(&after_user_id.titles, &after_discovery.titles));
        assert_eq!(after_user_id.titles, after_discovery.titles);
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
