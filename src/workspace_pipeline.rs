/* workspace_pipeline.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Revisioned contracts shared by workspace producers, the pure reducer, presentation, and
//! persistence. This module intentionally has no dependency on GTK, WebKit, Slack clients, or
//! SQLite so every input can follow the same deterministic path.

// These contracts are migrated surface-by-surface; the coordinator task wires their consumers.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use crate::attention::{
    AttentionCandidate, AttentionDecision, AttentionPolicy, AttentionPreferences, ConversationKind,
    DeliveryState, MessageMutation, ThreadRelationship,
};
#[cfg(test)]
use crate::models::SlackUnreadState;
#[cfg(test)]
use crate::models::LOCAL_READ_TS_KEY;
use crate::models::{
    conversation_metadata_key_is_unread_owned, slack_timestamp_is_after, SlackConversation,
    SlackConversationUnreadSnapshot, SlackMessage, SlackUser,
};
use crate::thread_catalog::{ThreadRecord, ThreadUnreadState};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct WorkspaceRevision(u64);

impl WorkspaceRevision {
    pub(crate) const INITIAL: Self = Self(0);

    pub(crate) fn value(self) -> u64 {
        self.0
    }

    pub(crate) fn successor(self) -> Self {
        Self(
            self.0
                .checked_add(1)
                .expect("workspace revision space exhausted"),
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SnapshotEnvelope<T> {
    base_revision: WorkspaceRevision,
    data: T,
}

impl<T> SnapshotEnvelope<T> {
    pub(crate) fn new(base_revision: WorkspaceRevision, data: T) -> Self {
        Self {
            base_revision,
            data,
        }
    }

    pub(crate) fn base_revision(&self) -> WorkspaceRevision {
        self.base_revision
    }

    pub(crate) fn data(&self) -> &T {
        &self.data
    }

    pub(crate) fn into_data(self) -> T {
        self.data
    }

    pub(crate) fn is_stale_at(&self, current_revision: WorkspaceRevision) -> bool {
        self.base_revision < current_revision
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceBootstrapData {
    pub(crate) conversations: Vec<SlackConversation>,
    pub(crate) users: Vec<SlackUser>,
    pub(crate) histories: HashMap<String, Vec<SlackMessage>>,
    pub(crate) threads: Vec<ThreadRecord>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ConversationMembershipSnapshot {
    pub(crate) conversations: Vec<SlackConversation>,
    pub(crate) starred_ids: Option<HashSet<String>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ConversationRefresh {
    pub(crate) metadata: Option<SlackConversation>,
    pub(crate) unread: Option<SlackConversationUnreadSnapshot>,
}

impl ConversationRefresh {
    pub(crate) fn potential_change_count(&self) -> usize {
        usize::from(self.metadata.is_some()) + usize::from(self.unread.is_some())
    }

    fn channel_id(&self) -> Option<&str> {
        let metadata_id = self
            .metadata
            .as_ref()
            .map(|conversation| conversation.id.as_str());
        let unread_id = self
            .unread
            .as_ref()
            .map(|snapshot| snapshot.channel_id.as_str());
        match (metadata_id, unread_id) {
            (Some(metadata_id), Some(unread_id))
                if !metadata_id.trim().is_empty()
                    && !unread_id.trim().is_empty()
                    && metadata_id == unread_id =>
            {
                Some(metadata_id)
            }
            (Some(metadata_id), None) if !metadata_id.trim().is_empty() => Some(metadata_id),
            (None, Some(unread_id)) if !unread_id.trim().is_empty() => Some(unread_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct MessagePage {
    pub(crate) messages: Vec<SlackMessage>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) complete: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct ConversationAttentionObservation {
    pub(crate) message_ts: String,
    pub(crate) record_unread: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationOrigin {
    Cache,
    WebApi,
    Local,
    Realtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageMutationKind {
    Posted,
    Changed,
    Deleted,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub(crate) enum WorkspaceMutation {
    AttentionContextChanged(WorkspaceAttentionContext),
    AttentionPreferencesChanged(AttentionPreferences),
    Hydrate(WorkspaceBootstrapData),
    MembershipSnapshot(SnapshotEnvelope<ConversationMembershipSnapshot>),
    ConversationRefreshBatch(Vec<SnapshotEnvelope<ConversationRefresh>>),
    ConversationUpsert(SlackConversation),
    ConversationStarChanged {
        channel_id: String,
        starred: bool,
    },
    ConversationRemove {
        channel_id: String,
    },
    UnreadChanged {
        snapshot: SlackConversationUnreadSnapshot,
        base_revision: WorkspaceRevision,
    },
    ReadAdvanced {
        channel_id: String,
        ts: String,
        remaining_unread: u64,
    },
    AttentionAcknowledged {
        channel_id: String,
        message_ts: Vec<String>,
    },
    UsersSnapshot(SnapshotEnvelope<Vec<SlackUser>>),
    UserUpsert(SlackUser),
    HistorySnapshot {
        channel_id: String,
        snapshot: SnapshotEnvelope<MessagePage>,
    },
    HistoryPage {
        channel_id: String,
        page: MessagePage,
    },
    ThreadSnapshot {
        channel_id: String,
        thread_ts: String,
        snapshot: SnapshotEnvelope<MessagePage>,
    },
    ThreadPage {
        channel_id: String,
        thread_ts: String,
        page: MessagePage,
    },
    MessageChanged {
        channel_id: String,
        message: SlackMessage,
        kind: MessageMutationKind,
        origin: MutationOrigin,
    },
    MessageChangedWithDelivery {
        channel_id: String,
        message: SlackMessage,
        kind: MessageMutationKind,
        origin: MutationOrigin,
        delivery: DeliveryState,
    },
    ThreadCatalogChanged(Vec<ThreadRecord>),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TimelineTarget {
    Channel(String),
    Thread {
        channel_id: String,
        thread_ts: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MessageChange {
    Upsert(Box<SlackMessage>),
    Remove { message_ts: String },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub(crate) enum WorkspaceChange {
    BootstrapReset(WorkspaceBootstrapData),
    ConversationsReset(Vec<SlackConversation>),
    ConversationUpsert(SlackConversation),
    ConversationMetadataUpsert(SlackConversation),
    ConversationAttentionObserved {
        channel_id: String,
        observations: Vec<ConversationAttentionObservation>,
    },
    ConversationRemoved {
        channel_id: String,
    },
    UnreadChanged {
        snapshot: SlackConversationUnreadSnapshot,
    },
    UsersReset(Vec<SlackUser>),
    UserUpsert(SlackUser),
    TimelineChanged {
        target: TimelineTarget,
        changes: Vec<MessageChange>,
    },
    ThreadCatalogChanged(Vec<ThreadRecord>),
}

#[derive(Debug, Clone)]
pub struct WorkspacePatch {
    revision: WorkspaceRevision,
    changes: Vec<WorkspaceChange>,
}

impl WorkspacePatch {
    pub(crate) fn new(revision: WorkspaceRevision, changes: Vec<WorkspaceChange>) -> Option<Self> {
        (revision > WorkspaceRevision::INITIAL && !changes.is_empty())
            .then_some(Self { revision, changes })
    }

    pub(crate) fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    pub(crate) fn changes(&self) -> &[WorkspaceChange] {
        &self.changes
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub(crate) enum StoreChange {
    BootstrapReplaced(WorkspaceBootstrapData),
    ConversationsReplaced(Vec<SlackConversation>),
    ConversationsRepaired(Vec<SlackConversation>),
    ConversationUpsert(SlackConversation),
    ConversationMetadataUpsert(SlackConversation),
    ConversationMembershipUpsert(SlackConversation),
    ConversationStarChanged {
        channel_id: String,
        starred: bool,
    },
    ConversationAttentionObserved {
        channel_id: String,
        observations: Vec<ConversationAttentionObservation>,
    },
    ConversationRemoved {
        channel_id: String,
    },
    UnreadChanged {
        snapshot: SlackConversationUnreadSnapshot,
    },
    UsersReplaced(Vec<SlackUser>),
    UserUpsert(SlackUser),
    MessageDelta {
        channel_id: String,
        message: SlackMessage,
        kind: MessageMutationKind,
    },
    HistoryReplaced {
        channel_id: String,
        messages: Vec<SlackMessage>,
    },
    HistoryRemoved {
        channel_id: String,
    },
    ThreadReplaced {
        channel_id: String,
        thread_ts: String,
        messages: Vec<SlackMessage>,
    },
    ThreadCatalogReplaced(Vec<ThreadRecord>),
}

#[derive(Debug, Clone)]
pub(crate) struct StoreBatch {
    revision: WorkspaceRevision,
    changes: Vec<StoreChange>,
}

impl StoreBatch {
    pub(crate) fn new(revision: WorkspaceRevision, changes: Vec<StoreChange>) -> Option<Self> {
        (revision > WorkspaceRevision::INITIAL && !changes.is_empty())
            .then_some(Self { revision, changes })
    }

    pub(crate) fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    pub(crate) fn changes(&self) -> &[StoreChange] {
        &self.changes
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceAttentionContext {
    pub(crate) current_user_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MessageAttentionEffect {
    pub(crate) channel_id: String,
    pub(crate) message: SlackMessage,
    pub(crate) decision: AttentionDecision,
    pub(crate) delivery: DeliveryState,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WorkspaceEffect {
    MessageAttention(MessageAttentionEffect),
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceReduction {
    patch: WorkspacePatch,
    store_batch: Option<StoreBatch>,
    effects: Vec<WorkspaceEffect>,
}

#[derive(Debug, Clone)]
struct RevisionedConversation {
    value: SlackConversation,
    membership_revision: WorkspaceRevision,
    metadata_revision: WorkspaceRevision,
    unread_revision: WorkspaceRevision,
    star_revision: WorkspaceRevision,
}

#[derive(Debug, Clone)]
struct RevisionedValue<T> {
    value: T,
    revision: WorkspaceRevision,
}

#[derive(Debug, Clone)]
struct MessageProjectionAuthority {
    revision: WorkspaceRevision,
    current_ts: String,
    retained_targets: Vec<TimelineTarget>,
}

#[derive(Debug, Clone, Default)]
struct TimelineState {
    messages: HashMap<String, RevisionedValue<SlackMessage>>,
    tombstones: HashMap<String, WorkspaceRevision>,
}

impl TimelineState {
    fn messages(&self) -> Vec<SlackMessage> {
        let mut messages = self
            .messages
            .values()
            .map(|entry| entry.value.clone())
            .collect::<Vec<_>>();
        messages.sort_by(|left, right| left.ts.cmp(&right.ts));
        messages
    }

    fn contains_identity(&self, message: &SlackMessage) -> bool {
        self.messages
            .values()
            .any(|entry| same_message_identity(&entry.value, message))
    }

    fn identity_timestamps(&self, message: &SlackMessage) -> Vec<String> {
        let mut timestamps = self
            .messages
            .iter()
            .filter(|(_, entry)| same_message_identity(&entry.value, message))
            .map(|(message_ts, _)| message_ts.clone())
            .collect::<Vec<_>>();
        timestamps.sort();
        timestamps
    }

    fn identity_message(&self, message: &SlackMessage) -> Option<SlackMessage> {
        self.messages
            .values()
            .filter(|entry| same_message_identity(&entry.value, message))
            .map(|entry| entry.value.clone())
            .max_by(|left, right| left.ts.cmp(&right.ts))
    }
}

/// Pure owner of one workspace's canonical domain model and global revision.
///
/// Runtime and GTK adapters are deliberately absent here. A mutation either changes the model
/// once and produces one revision-stamped reduction, or is a no-op that leaves the revision
/// untouched.
#[derive(Debug)]
pub(crate) struct WorkspaceCoordinator {
    revision: WorkspaceRevision,
    conversations: HashMap<String, RevisionedConversation>,
    users: HashMap<String, RevisionedValue<SlackUser>>,
    histories: HashMap<String, TimelineState>,
    threads: HashMap<(String, String), TimelineState>,
    message_authority_by_ts: HashMap<(String, String), MessageProjectionAuthority>,
    message_authority_by_client_id: HashMap<(String, String), MessageProjectionAuthority>,
    thread_catalog: Vec<ThreadRecord>,
    attention_context: WorkspaceAttentionContext,
    attention_preferences: AttentionPreferences,
    attention_policy: AttentionPolicy,
}

impl WorkspaceCoordinator {
    pub(crate) fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    pub(crate) fn conversation(&self, channel_id: &str) -> Option<&SlackConversation> {
        self.conversations.get(channel_id).map(|entry| &entry.value)
    }

    pub(crate) fn conversations(&self) -> Vec<SlackConversation> {
        let mut conversations = self
            .conversations
            .values()
            .map(|entry| entry.value.clone())
            .collect::<Vec<_>>();
        conversations.sort_by(|left, right| left.id.cmp(&right.id));
        conversations
    }

    pub(crate) fn history(&self, channel_id: &str) -> Vec<SlackMessage> {
        self.histories
            .get(channel_id)
            .map(TimelineState::messages)
            .unwrap_or_default()
    }

    pub(crate) fn apply(&mut self, mutation: WorkspaceMutation) -> Option<WorkspaceReduction> {
        self.apply_from(MutationOrigin::Cache, mutation)
    }

    pub(crate) fn apply_from(
        &mut self,
        origin: MutationOrigin,
        mutation: WorkspaceMutation,
    ) -> Option<WorkspaceReduction> {
        match mutation {
            WorkspaceMutation::AttentionContextChanged(context) => {
                self.attention_context = context;
                None
            }
            WorkspaceMutation::AttentionPreferencesChanged(preferences) => {
                if self.attention_preferences != preferences {
                    self.attention_policy = AttentionPolicy::new(preferences.clone());
                    self.attention_preferences = preferences;
                }
                None
            }
            WorkspaceMutation::Hydrate(data) => self.apply_hydration(data, origin),
            WorkspaceMutation::MembershipSnapshot(snapshot) => {
                self.apply_membership_snapshot(snapshot)
            }
            WorkspaceMutation::ConversationRefreshBatch(refreshes) => {
                self.apply_conversation_refresh_batch(refreshes)
            }
            WorkspaceMutation::ConversationUpsert(conversation) => {
                self.apply_conversation_upsert(conversation)
            }
            WorkspaceMutation::ConversationStarChanged {
                channel_id,
                starred,
            } => self.apply_conversation_star_changed(&channel_id, starred),
            WorkspaceMutation::ConversationRemove { channel_id } => {
                self.apply_conversation_remove(&channel_id)
            }
            WorkspaceMutation::UnreadChanged {
                snapshot,
                base_revision,
            } => self.apply_unread(snapshot, base_revision),
            WorkspaceMutation::ReadAdvanced {
                channel_id,
                ts,
                remaining_unread,
            } => self.apply_read_advanced(&channel_id, &ts, remaining_unread),
            WorkspaceMutation::AttentionAcknowledged {
                channel_id,
                message_ts,
            } => self.apply_attention_acknowledged(&channel_id, &message_ts),
            WorkspaceMutation::UsersSnapshot(snapshot) => self.apply_users_snapshot(snapshot),
            WorkspaceMutation::UserUpsert(user) => self.apply_user_upsert(user),
            WorkspaceMutation::HistorySnapshot {
                channel_id,
                snapshot,
            } => {
                self.apply_timeline_snapshot(TimelineTarget::Channel(channel_id), snapshot, origin)
            }
            WorkspaceMutation::HistoryPage { channel_id, page } => self.apply_timeline_snapshot(
                TimelineTarget::Channel(channel_id),
                SnapshotEnvelope::new(self.revision, page),
                origin,
            ),
            WorkspaceMutation::ThreadSnapshot {
                channel_id,
                thread_ts,
                snapshot,
            } => self.apply_timeline_snapshot(
                TimelineTarget::Thread {
                    channel_id,
                    thread_ts,
                },
                snapshot,
                origin,
            ),
            WorkspaceMutation::ThreadPage {
                channel_id,
                thread_ts,
                page,
            } => self.apply_timeline_snapshot(
                TimelineTarget::Thread {
                    channel_id,
                    thread_ts,
                },
                SnapshotEnvelope::new(self.revision, page),
                origin,
            ),
            WorkspaceMutation::MessageChanged {
                channel_id,
                message,
                kind,
                origin,
            } => self.apply_message(&channel_id, message, kind, origin, None),
            WorkspaceMutation::MessageChangedWithDelivery {
                channel_id,
                message,
                kind,
                origin,
                delivery,
            } => self.apply_message(&channel_id, message, kind, origin, Some(delivery)),
            WorkspaceMutation::ThreadCatalogChanged(records) => self.apply_thread_catalog(records),
        }
    }

    fn next_revision(&self) -> WorkspaceRevision {
        self.revision.successor()
    }

    fn commit(
        &mut self,
        revision: WorkspaceRevision,
        patch_changes: Vec<WorkspaceChange>,
        store_changes: Vec<StoreChange>,
    ) -> Option<WorkspaceReduction> {
        self.commit_with_effects(revision, patch_changes, store_changes, Vec::new())
    }

    fn commit_with_effects(
        &mut self,
        revision: WorkspaceRevision,
        patch_changes: Vec<WorkspaceChange>,
        store_changes: Vec<StoreChange>,
        effects: Vec<WorkspaceEffect>,
    ) -> Option<WorkspaceReduction> {
        let reduction =
            WorkspaceReduction::new_with_effects(revision, patch_changes, store_changes, effects)?;
        self.revision = revision;
        Some(reduction)
    }

    fn apply_hydration(
        &mut self,
        data: WorkspaceBootstrapData,
        origin: MutationOrigin,
    ) -> Option<WorkspaceReduction> {
        let unchanged = self.conversations.len() == data.conversations.len()
            && data
                .conversations
                .iter()
                .all(|conversation| self.conversation(&conversation.id) == Some(conversation))
            && self.users.len() == data.users.len()
            && data.users.iter().all(|user| {
                user.id.as_deref().is_some_and(|user_id| {
                    self.users
                        .get(user_id)
                        .is_some_and(|entry| entry.value == *user)
                })
            })
            && data
                .histories
                .iter()
                .all(|(channel_id, messages)| self.history(channel_id) == *messages)
            && self.thread_catalog == data.threads;
        if unchanged {
            return None;
        }

        let revision = self.next_revision();
        self.conversations = data
            .conversations
            .iter()
            .cloned()
            .map(|conversation| {
                (
                    conversation.id.clone(),
                    RevisionedConversation {
                        value: conversation,
                        membership_revision: revision,
                        metadata_revision: revision,
                        unread_revision: revision,
                        star_revision: revision,
                    },
                )
            })
            .collect();
        self.users = data
            .users
            .iter()
            .cloned()
            .filter_map(|user| {
                let user_id = user.id.clone()?;
                Some((
                    user_id,
                    RevisionedValue {
                        value: user,
                        revision,
                    },
                ))
            })
            .collect();
        self.histories = data
            .histories
            .iter()
            .map(|(channel_id, messages)| {
                (
                    channel_id.clone(),
                    timeline_from_messages(messages, revision),
                )
            })
            .collect();
        self.message_authority_by_ts.clear();
        self.message_authority_by_client_id.clear();
        self.thread_catalog = data.threads.clone();
        let store_changes = if origin == MutationOrigin::Cache {
            Vec::new()
        } else {
            vec![StoreChange::BootstrapReplaced(data.clone())]
        };
        self.commit(
            revision,
            vec![WorkspaceChange::BootstrapReset(data)],
            store_changes,
        )
    }

    fn apply_conversation_upsert(
        &mut self,
        mut conversation: SlackConversation,
    ) -> Option<WorkspaceReduction> {
        if conversation.id.trim().is_empty() {
            return None;
        }
        // Generic join/open/invite/details responses do not carry an
        // authoritative star projection and may finish after a newer toggle.
        conversation.is_starred = None;
        let revision = self.next_revision();
        let changed = match self.conversations.get_mut(&conversation.id) {
            Some(entry) => {
                let mut merged = entry.value.clone();
                merge_conversation_metadata(&mut merged, &conversation);
                if merged == entry.value {
                    false
                } else {
                    entry.value = merged;
                    entry.metadata_revision = revision;
                    entry.membership_revision = revision;
                    true
                }
            }
            None => {
                self.conversations.insert(
                    conversation.id.clone(),
                    RevisionedConversation {
                        value: conversation.clone(),
                        membership_revision: revision,
                        metadata_revision: revision,
                        unread_revision: revision,
                        star_revision: WorkspaceRevision::INITIAL,
                    },
                );
                true
            }
        };
        if !changed {
            return None;
        }
        let current = self.conversation(&conversation.id).unwrap().clone();
        self.commit(
            revision,
            vec![WorkspaceChange::ConversationUpsert(current.clone())],
            vec![StoreChange::ConversationMetadataUpsert(current)],
        )
    }

    fn apply_conversation_remove(&mut self, channel_id: &str) -> Option<WorkspaceReduction> {
        self.conversations.remove(channel_id)?;
        let revision = self.next_revision();
        self.commit(
            revision,
            vec![WorkspaceChange::ConversationRemoved {
                channel_id: channel_id.to_string(),
            }],
            vec![StoreChange::ConversationRemoved {
                channel_id: channel_id.to_string(),
            }],
        )
    }

    fn apply_conversation_star_changed(
        &mut self,
        channel_id: &str,
        starred: bool,
    ) -> Option<WorkspaceReduction> {
        if channel_id.trim().is_empty()
            || self
                .conversations
                .get(channel_id)
                .is_some_and(|entry| entry.value.is_starred == Some(starred))
        {
            return None;
        }
        let revision = self.next_revision();
        let entry = self
            .conversations
            .entry(channel_id.to_string())
            .or_insert_with(|| RevisionedConversation {
                value: SlackConversation {
                    id: channel_id.to_string(),
                    ..Default::default()
                },
                membership_revision: revision,
                metadata_revision: revision,
                unread_revision: revision,
                star_revision: WorkspaceRevision::INITIAL,
            });
        entry.value.is_starred = Some(starred);
        entry.star_revision = revision;
        let conversation = entry.value.clone();
        self.commit(
            revision,
            vec![WorkspaceChange::ConversationUpsert(conversation)],
            vec![StoreChange::ConversationStarChanged {
                channel_id: channel_id.to_string(),
                starred,
            }],
        )
    }

    fn apply_membership_snapshot(
        &mut self,
        snapshot: SnapshotEnvelope<ConversationMembershipSnapshot>,
    ) -> Option<WorkspaceReduction> {
        let base_revision = snapshot.base_revision();
        let snapshot = snapshot.into_data();
        let starred_ids = snapshot.starred_ids;
        let mut incoming = HashMap::<String, SlackConversation>::new();
        for mut conversation in snapshot
            .conversations
            .into_iter()
            .filter(|conversation| !conversation.id.trim().is_empty())
        {
            conversation.is_starred = None;
            match incoming.entry(conversation.id.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    merge_conversation_metadata(entry.get_mut(), &conversation);
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(conversation);
                }
            }
        }
        let revision = self.next_revision();
        let mut patch_changes = Vec::new();
        let mut store_changes = Vec::new();

        for (channel_id, conversation) in &incoming {
            match self.conversations.get_mut(channel_id) {
                Some(entry) if entry.metadata_revision <= base_revision => {
                    let mut merged = entry.value.clone();
                    merge_conversation_metadata(&mut merged, conversation);
                    if merged != entry.value {
                        entry.value = merged.clone();
                        entry.metadata_revision = revision;
                        patch_changes.push(WorkspaceChange::ConversationUpsert(merged.clone()));
                        store_changes.push(StoreChange::ConversationMembershipUpsert(merged));
                    }
                }
                Some(_) => {}
                None => {
                    self.conversations.insert(
                        channel_id.clone(),
                        RevisionedConversation {
                            value: conversation.clone(),
                            membership_revision: revision,
                            metadata_revision: revision,
                            unread_revision: revision,
                            star_revision: WorkspaceRevision::INITIAL,
                        },
                    );
                    patch_changes.push(WorkspaceChange::ConversationUpsert(conversation.clone()));
                    store_changes.push(StoreChange::ConversationMembershipUpsert(
                        conversation.clone(),
                    ));
                }
            }
        }

        let removed = self
            .conversations
            .iter()
            .filter(|(channel_id, entry)| {
                !incoming.contains_key(*channel_id) && entry.membership_revision <= base_revision
            })
            .map(|(channel_id, _)| channel_id.clone())
            .collect::<Vec<_>>();
        for channel_id in removed {
            self.conversations.remove(&channel_id);
            patch_changes.push(WorkspaceChange::ConversationRemoved {
                channel_id: channel_id.clone(),
            });
            store_changes.push(StoreChange::ConversationRemoved { channel_id });
        }

        if let Some(starred_ids) = starred_ids {
            for entry in self.conversations.values_mut() {
                if entry.star_revision > base_revision || !conversation_supports_stars(&entry.value)
                {
                    continue;
                }
                let starred = starred_ids.contains(&entry.value.id);
                if entry.value.is_starred == Some(starred) {
                    continue;
                }
                entry.value.is_starred = Some(starred);
                entry.star_revision = revision;
                patch_changes.push(WorkspaceChange::ConversationUpsert(entry.value.clone()));
                store_changes.push(StoreChange::ConversationStarChanged {
                    channel_id: entry.value.id.clone(),
                    starred,
                });
            }
        }

        self.commit(revision, patch_changes, store_changes)
    }

    fn apply_conversation_refresh_batch(
        &mut self,
        refreshes: Vec<SnapshotEnvelope<ConversationRefresh>>,
    ) -> Option<WorkspaceReduction> {
        let revision = self.next_revision();
        let mut patch_changes = Vec::new();
        let mut store_changes = Vec::new();

        for refresh in refreshes {
            let base_revision = refresh.base_revision();
            let mut refresh = refresh.into_data();
            let Some(channel_id) = refresh.channel_id().map(str::to_string) else {
                continue;
            };
            let Some(entry) = self.conversations.get_mut(&channel_id) else {
                continue;
            };

            match refresh.metadata.take() {
                Some(mut metadata) if entry.metadata_revision <= base_revision => {
                    sanitize_conversation_refresh_metadata(&mut metadata);
                    let mut merged = entry.value.clone();
                    merge_conversation_metadata(&mut merged, &metadata);
                    if merged != entry.value {
                        entry.value = merged;
                        entry.metadata_revision = revision;
                        patch_changes.push(WorkspaceChange::ConversationMetadataUpsert(
                            metadata.clone(),
                        ));
                        store_changes.push(StoreChange::ConversationMetadataUpsert(metadata));
                    }
                }
                _ => {}
            }

            match refresh.unread.take() {
                Some(unread)
                    if entry.unread_revision <= base_revision
                        && unread.unread_state.known
                        && !entry.value.unread_snapshot_rewinds_read(&unread) =>
                {
                    let before = entry.value.clone();
                    entry.value.clear_local_read_ts();
                    entry.value.apply_unread_snapshot(&unread);
                    if entry.value != before {
                        entry.unread_revision = revision;
                        entry.membership_revision = entry.membership_revision.max(revision);
                        patch_changes.push(WorkspaceChange::UnreadChanged {
                            snapshot: unread.clone(),
                        });
                        store_changes.push(StoreChange::UnreadChanged { snapshot: unread });
                    }
                }
                _ => {}
            }
        }

        self.commit(revision, patch_changes, store_changes)
    }

    fn apply_unread(
        &mut self,
        snapshot: SlackConversationUnreadSnapshot,
        base_revision: WorkspaceRevision,
    ) -> Option<WorkspaceReduction> {
        if !snapshot.unread_state.known || snapshot.channel_id.trim().is_empty() {
            return None;
        }
        if self
            .conversations
            .get(&snapshot.channel_id)
            .is_some_and(|entry| entry.unread_revision > base_revision)
        {
            return None;
        }
        if self
            .conversations
            .get(&snapshot.channel_id)
            .is_some_and(|entry| entry.value.unread_snapshot_rewinds_read(&snapshot))
        {
            return None;
        }
        let revision = self.next_revision();
        let entry = self
            .conversations
            .entry(snapshot.channel_id.clone())
            .or_insert_with(|| RevisionedConversation {
                value: SlackConversation {
                    id: snapshot.channel_id.clone(),
                    ..Default::default()
                },
                membership_revision: revision,
                metadata_revision: revision,
                unread_revision: revision,
                star_revision: WorkspaceRevision::INITIAL,
            });
        let before = entry.value.clone();
        entry.value.clear_local_read_ts();
        entry.value.apply_unread_snapshot(&snapshot);
        if entry.value == before {
            return None;
        }
        entry.unread_revision = revision;
        entry.membership_revision = entry.membership_revision.max(revision);
        self.commit(
            revision,
            vec![WorkspaceChange::UnreadChanged {
                snapshot: snapshot.clone(),
            }],
            vec![StoreChange::UnreadChanged { snapshot }],
        )
    }

    fn apply_read_advanced(
        &mut self,
        channel_id: &str,
        ts: &str,
        remaining_unread: u64,
    ) -> Option<WorkspaceReduction> {
        self.conversations.get(channel_id)?;
        let revision = self.next_revision();
        let entry = self.conversations.get_mut(channel_id).unwrap();
        let before = entry.value.clone();
        entry.value.advance_read_cursor(ts, remaining_unread);
        entry.value.set_local_read_ts(ts);
        if entry.value == before {
            return None;
        }
        entry.unread_revision = revision;
        let state = entry.value.unread_state();
        let conversation = entry.value.clone();
        let snapshot = SlackConversationUnreadSnapshot {
            channel_id: channel_id.to_string(),
            unread_state: state,
            last_read: Some(ts.to_string()),
            ..Default::default()
        };
        self.commit(
            revision,
            vec![WorkspaceChange::UnreadChanged { snapshot }],
            vec![StoreChange::ConversationUpsert(conversation)],
        )
    }

    fn apply_attention_acknowledged(
        &mut self,
        channel_id: &str,
        message_ts: &[String],
    ) -> Option<WorkspaceReduction> {
        if message_ts.is_empty() {
            return None;
        }
        self.conversations.get(channel_id)?;
        let revision = self.next_revision();
        let entry = self.conversations.get_mut(channel_id).unwrap();
        if entry.value.acknowledge_attention_messages(message_ts) == 0 {
            return None;
        }
        entry.unread_revision = revision;
        let conversation = entry.value.clone();
        self.commit(
            revision,
            vec![WorkspaceChange::ConversationUpsert(conversation.clone())],
            vec![StoreChange::ConversationUpsert(conversation)],
        )
    }

    fn apply_users_snapshot(
        &mut self,
        snapshot: SnapshotEnvelope<Vec<SlackUser>>,
    ) -> Option<WorkspaceReduction> {
        let base_revision = snapshot.base_revision();
        let revision = self.next_revision();
        let mut changed = Vec::new();
        for user in snapshot.into_data() {
            let Some(user_id) = user
                .id
                .as_deref()
                .map(str::trim)
                .filter(|user_id| !user_id.is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            let should_apply = self
                .users
                .get(&user_id)
                .is_none_or(|entry| entry.revision <= base_revision && entry.value != user);
            if should_apply {
                self.users.insert(
                    user_id,
                    RevisionedValue {
                        value: user.clone(),
                        revision,
                    },
                );
                changed.push(user);
            }
        }
        if changed.is_empty() {
            return None;
        }
        self.commit(
            revision,
            changed
                .iter()
                .cloned()
                .map(WorkspaceChange::UserUpsert)
                .collect(),
            changed.into_iter().map(StoreChange::UserUpsert).collect(),
        )
    }

    fn apply_user_upsert(&mut self, user: SlackUser) -> Option<WorkspaceReduction> {
        let user_id = user
            .id
            .as_deref()
            .map(str::trim)
            .filter(|user_id| !user_id.is_empty())?
            .to_string();
        if self
            .users
            .get(&user_id)
            .is_some_and(|entry| entry.value == user)
        {
            return None;
        }
        let revision = self.next_revision();
        self.users.insert(
            user_id,
            RevisionedValue {
                value: user.clone(),
                revision,
            },
        );
        self.commit(
            revision,
            vec![WorkspaceChange::UserUpsert(user.clone())],
            vec![StoreChange::UserUpsert(user)],
        )
    }

    fn message_projection_is_superseded(
        &self,
        target: &TimelineTarget,
        message: &SlackMessage,
        base_revision: WorkspaceRevision,
    ) -> bool {
        let channel_id = match target {
            TimelineTarget::Channel(channel_id) => channel_id,
            TimelineTarget::Thread { channel_id, .. } => channel_id,
        };
        let timestamp_key = (channel_id.clone(), message.ts.clone());
        let client_key = message
            .client_msg_id
            .as_deref()
            .filter(|client_id| !client_id.trim().is_empty())
            .map(|client_id| (channel_id.clone(), client_id.to_string()));
        let authority = [
            self.message_authority_by_ts.get(&timestamp_key),
            client_key
                .as_ref()
                .and_then(|key| self.message_authority_by_client_id.get(key)),
        ]
        .into_iter()
        .flatten()
        .filter(|authority| authority.revision > base_revision)
        .max_by_key(|authority| authority.revision);
        authority.is_some_and(|authority| {
            authority.current_ts != message.ts || !authority.retained_targets.contains(target)
        })
    }

    fn record_message_projection_authority(
        &mut self,
        channel_id: &str,
        current: &SlackMessage,
        identity_messages: &[SlackMessage],
        retained_targets: Vec<TimelineTarget>,
        revision: WorkspaceRevision,
    ) {
        let authority = MessageProjectionAuthority {
            revision,
            current_ts: current.ts.clone(),
            retained_targets,
        };
        for message in identity_messages {
            if !message.ts.trim().is_empty() {
                self.message_authority_by_ts.insert(
                    (channel_id.to_string(), message.ts.clone()),
                    authority.clone(),
                );
            }
            if let Some(client_id) = message
                .client_msg_id
                .as_deref()
                .filter(|client_id| !client_id.trim().is_empty())
            {
                self.message_authority_by_client_id.insert(
                    (channel_id.to_string(), client_id.to_string()),
                    authority.clone(),
                );
            }
        }
    }

    fn apply_timeline_snapshot(
        &mut self,
        target: TimelineTarget,
        snapshot: SnapshotEnvelope<MessagePage>,
        origin: MutationOrigin,
    ) -> Option<WorkspaceReduction> {
        let base_revision = snapshot.base_revision();
        let page = snapshot.into_data();
        let revision = self.next_revision();
        let incoming = page
            .messages
            .into_iter()
            .filter(|message| match &target {
                TimelineTarget::Channel(_) => message.belongs_in_channel_timeline(),
                TimelineTarget::Thread { thread_ts, .. } => message.belongs_to_thread(thread_ts),
            })
            .filter(|message| {
                !self.message_projection_is_superseded(&target, message, base_revision)
            })
            .map(|message| (message.ts.clone(), message))
            .collect::<HashMap<_, _>>();
        let timeline = self.timeline_mut(&target);
        let mut changes = Vec::new();
        let mut accepted_messages = Vec::new();
        for (message_ts, message) in &incoming {
            if timeline
                .tombstones
                .get(message_ts)
                .is_some_and(|deleted_at| *deleted_at > base_revision)
                || timeline
                    .messages
                    .get(message_ts)
                    .is_some_and(|entry| entry.revision > base_revision)
            {
                continue;
            }
            if timeline
                .messages
                .get(message_ts)
                .is_none_or(|entry| entry.value != *message)
            {
                timeline.messages.insert(
                    message_ts.clone(),
                    RevisionedValue {
                        value: message.clone(),
                        revision,
                    },
                );
                timeline.tombstones.remove(message_ts);
                changes.push(MessageChange::Upsert(Box::new(message.clone())));
                accepted_messages.push(message.clone());
            }
        }
        if page.complete {
            let removed = timeline
                .messages
                .iter()
                .filter(|(message_ts, entry)| {
                    !incoming.contains_key(*message_ts) && entry.revision <= base_revision
                })
                .map(|(message_ts, _)| message_ts.clone())
                .collect::<Vec<_>>();
            for message_ts in removed {
                timeline.messages.remove(&message_ts);
                timeline.tombstones.insert(message_ts.clone(), revision);
                changes.push(MessageChange::Remove { message_ts });
            }
        }
        if changes.is_empty() {
            return None;
        }
        accepted_messages.sort_by(|left, right| left.ts.cmp(&right.ts));
        let messages = timeline.messages();
        let store_change = store_timeline_replacement(&target, messages);
        let reconciled_message_ts =
            self.reconciled_unread_message_ts(&target, &accepted_messages, origin);
        let attention_effects = accepted_messages
            .into_iter()
            .filter_map(|message| {
                let channel_id = match &target {
                    TimelineTarget::Channel(channel_id) => channel_id.as_str(),
                    TimelineTarget::Thread { channel_id, .. } => channel_id.as_str(),
                };
                let delivery = if reconciled_message_ts.contains(&message.ts) {
                    DeliveryState::Reconciled
                } else {
                    DeliveryState::Historical
                };
                self.message_attention_effect(
                    channel_id,
                    &message,
                    MessageMutationKind::Posted,
                    origin,
                    delivery,
                )
            })
            .collect::<Vec<_>>();
        let attention_channel_id = match &target {
            TimelineTarget::Channel(channel_id) => channel_id.clone(),
            TimelineTarget::Thread { channel_id, .. } => channel_id.clone(),
        };
        let mut attention_observations = Vec::new();
        if let Some(entry) = self.conversations.get_mut(&attention_channel_id) {
            for effect in attention_effects.iter().filter(|effect| {
                !effect
                    .decision
                    .reasons
                    .contains(&crate::attention::AttentionReason::SelfAuthored)
            }) {
                if entry.value.local_read_ts().is_some_and(|last_read| {
                    !slack_timestamp_is_after(&effect.message.ts, last_read)
                }) {
                    continue;
                }
                if entry
                    .value
                    .observe_attention_message_at(&effect.message.ts, effect.decision.record_unread)
                {
                    attention_observations.push(ConversationAttentionObservation {
                        message_ts: effect.message.ts.clone(),
                        record_unread: effect.decision.record_unread,
                    });
                }
            }
            if !attention_observations.is_empty() {
                entry.unread_revision = revision;
            }
        }
        let mut patch_changes = vec![WorkspaceChange::TimelineChanged { target, changes }];
        let mut store_changes = vec![store_change];
        if !attention_observations.is_empty() {
            patch_changes.push(WorkspaceChange::ConversationAttentionObserved {
                channel_id: attention_channel_id.clone(),
                observations: attention_observations.clone(),
            });
            store_changes.push(StoreChange::ConversationAttentionObserved {
                channel_id: attention_channel_id,
                observations: attention_observations,
            });
        }
        let effects = attention_effects
            .into_iter()
            .map(WorkspaceEffect::MessageAttention)
            .collect();
        self.commit_with_effects(revision, patch_changes, store_changes, effects)
    }

    pub(crate) fn preview_message_attention(
        &self,
        channel_id: &str,
        message: &SlackMessage,
        kind: MessageMutationKind,
        origin: MutationOrigin,
    ) -> Option<MessageAttentionEffect> {
        if channel_id.trim().is_empty() || message.ts.trim().is_empty() {
            return None;
        }
        self.message_attention_effect(
            channel_id,
            message,
            kind,
            origin,
            self.message_delivery_state(channel_id, message, origin),
        )
    }

    fn apply_message(
        &mut self,
        channel_id: &str,
        mut message: SlackMessage,
        kind: MessageMutationKind,
        origin: MutationOrigin,
        delivery_override: Option<DeliveryState>,
    ) -> Option<WorkspaceReduction> {
        if channel_id.trim().is_empty() || message.ts.trim().is_empty() {
            return None;
        }
        if kind == MessageMutationKind::Posted
            && origin == MutationOrigin::Realtime
            && self.conversation(channel_id).is_some_and(|conversation| {
                conversation.has_observed_attention_message(&message.ts)
            })
        {
            return None;
        }
        let previous_channel_message = self
            .histories
            .get(channel_id)
            .and_then(|timeline| timeline.identity_message(&message));
        let previous_thread_root_message = self
            .threads
            .get(&(channel_id.to_string(), message.ts.clone()))
            .and_then(|timeline| timeline.identity_message(&message));
        let previous_catalog_root_message = self
            .thread_catalog
            .iter()
            .find(|record| record.key.channel_id == channel_id && record.key.root_ts == message.ts)
            .and_then(|record| record.root.clone());
        if kind == MessageMutationKind::Changed && message.thread_root_ts().is_none() {
            preserve_missing_root_aggregates(
                &mut message,
                [
                    previous_channel_message.as_ref(),
                    previous_thread_root_message.as_ref(),
                    previous_catalog_root_message.as_ref(),
                ]
                .into_iter()
                .flatten(),
            );
        }
        let previous_channel_known = previous_channel_message.is_some();
        let mut previous_replies = self
            .threads
            .iter()
            .filter_map(|((known_channel_id, thread_ts), timeline)| {
                if known_channel_id != channel_id {
                    return None;
                }
                timeline
                    .identity_message(&message)
                    .filter(|message| message.thread_root_ts() == Some(thread_ts.as_str()))
                    .map(|message| (thread_ts.clone(), message))
            })
            .collect::<Vec<_>>();
        previous_replies.sort_by(|left, right| left.0.cmp(&right.0));
        let mut targets = Vec::new();
        if message.belongs_in_channel_timeline() {
            targets.push(TimelineTarget::Channel(channel_id.to_string()));
        }
        let existing_own_thread_root = self
            .threads
            .get(&(channel_id.to_string(), message.ts.clone()))
            .is_some_and(|timeline| timeline.messages.contains_key(&message.ts));
        let has_thread_root_aggregate = message.reply_count.is_some()
            || message.latest_reply.is_some()
            || message.reply_users.is_some();
        let catalog_own_thread_root = self
            .thread_catalog
            .iter()
            .any(|record| record.key.channel_id == channel_id && record.key.root_ts == message.ts);
        if let Some(thread_ts) = message.thread_root_ts() {
            targets.push(TimelineTarget::Thread {
                channel_id: channel_id.to_string(),
                thread_ts: thread_ts.to_string(),
            });
        } else if message.thread_ts.as_deref() == Some(message.ts.as_str())
            || existing_own_thread_root
            || has_thread_root_aggregate
            || catalog_own_thread_root
        {
            targets.push(TimelineTarget::Thread {
                channel_id: channel_id.to_string(),
                thread_ts: message.ts.clone(),
            });
        }
        let retained_targets = targets.clone();
        if matches!(
            kind,
            MessageMutationKind::Changed | MessageMutationKind::Deleted
        ) {
            let mut previous_targets = Vec::new();
            let channel_target = TimelineTarget::Channel(channel_id.to_string());
            if previous_channel_known {
                previous_targets.push(channel_target);
            }
            previous_targets.extend(previous_replies.iter().map(|(thread_ts, _)| {
                TimelineTarget::Thread {
                    channel_id: channel_id.to_string(),
                    thread_ts: thread_ts.clone(),
                }
            }));
            previous_targets.sort();
            for target in previous_targets {
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        if kind == MessageMutationKind::Posted
            && targets.iter().any(|target| {
                self.timeline(target)
                    .is_some_and(|timeline| timeline.contains_identity(&message))
            })
        {
            return None;
        }
        let delivery = delivery_override
            .unwrap_or_else(|| self.message_delivery_state(channel_id, &message, origin));
        let attention_effect =
            self.message_attention_effect(channel_id, &message, kind, origin, delivery);

        let revision = self.next_revision();
        let mut patch_changes = Vec::new();
        for target in targets {
            let timeline = self.timeline_mut(&target);
            let identity_timestamps = timeline.identity_timestamps(&message);
            let belongs_in_target = message_belongs_in_target(&message, &target);
            let message_changes = match kind {
                MessageMutationKind::Deleted => {
                    let mut timestamps = identity_timestamps;
                    if !timestamps.contains(&message.ts) {
                        timestamps.push(message.ts.clone());
                        timestamps.sort();
                    }
                    let mut changes = Vec::new();
                    for message_ts in timestamps {
                        let already_deleted = timeline.tombstones.contains_key(&message_ts);
                        let removed = timeline.messages.remove(&message_ts).is_some();
                        if removed || !already_deleted {
                            timeline.tombstones.insert(message_ts.clone(), revision);
                            changes.push(MessageChange::Remove { message_ts });
                        }
                    }
                    changes
                }
                MessageMutationKind::Posted => {
                    if timeline
                        .messages
                        .get(&message.ts)
                        .is_some_and(|entry| entry.value == message)
                    {
                        Vec::new()
                    } else {
                        timeline.messages.insert(
                            message.ts.clone(),
                            RevisionedValue {
                                value: message.clone(),
                                revision,
                            },
                        );
                        timeline.tombstones.remove(&message.ts);
                        vec![MessageChange::Upsert(Box::new(message.clone()))]
                    }
                }
                MessageMutationKind::Changed if belongs_in_target => {
                    if identity_timestamps.len() == 1
                        && identity_timestamps[0] == message.ts
                        && timeline
                            .messages
                            .get(&message.ts)
                            .is_some_and(|entry| entry.value == message)
                    {
                        Vec::new()
                    } else {
                        let mut changes = Vec::new();
                        for message_ts in identity_timestamps {
                            if message_ts == message.ts {
                                continue;
                            }
                            timeline.messages.remove(&message_ts);
                            timeline.tombstones.insert(message_ts.clone(), revision);
                            changes.push(MessageChange::Remove { message_ts });
                        }
                        timeline.messages.insert(
                            message.ts.clone(),
                            RevisionedValue {
                                value: message.clone(),
                                revision,
                            },
                        );
                        timeline.tombstones.remove(&message.ts);
                        changes.push(MessageChange::Upsert(Box::new(message.clone())));
                        changes
                    }
                }
                MessageMutationKind::Changed => {
                    let mut changes = Vec::new();
                    for message_ts in identity_timestamps {
                        timeline.messages.remove(&message_ts);
                        timeline.tombstones.insert(message_ts.clone(), revision);
                        changes.push(MessageChange::Remove { message_ts });
                    }
                    changes
                }
            };
            if message_changes.is_empty() {
                continue;
            }
            patch_changes.push(WorkspaceChange::TimelineChanged {
                target,
                changes: message_changes,
            });
        }

        let changed_roots = self.reconcile_channel_roots_for_message(
            channel_id,
            &message,
            kind,
            previous_channel_known,
            &previous_replies,
            revision,
        );
        for (root, thread_root_change) in changed_roots {
            let channel_target = TimelineTarget::Channel(channel_id.to_string());
            patch_changes.push(WorkspaceChange::TimelineChanged {
                target: channel_target,
                changes: vec![MessageChange::Upsert(Box::new(root.clone()))],
            });
            if let Some(thread_root) = thread_root_change {
                patch_changes.push(WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread {
                        channel_id: channel_id.to_string(),
                        thread_ts: root.ts.clone(),
                    },
                    changes: vec![MessageChange::Upsert(Box::new(thread_root))],
                });
            }
        }

        if patch_changes.is_empty() {
            return None;
        }
        if matches!(
            kind,
            MessageMutationKind::Changed | MessageMutationKind::Deleted
        ) {
            let mut authority_messages = vec![message.clone()];
            authority_messages.extend(previous_channel_message);
            authority_messages.extend(previous_thread_root_message);
            authority_messages.extend(previous_catalog_root_message);
            authority_messages.extend(
                previous_replies
                    .iter()
                    .map(|(_, previous)| previous.clone()),
            );
            self.record_message_projection_authority(
                channel_id,
                &message,
                &authority_messages,
                retained_targets,
                revision,
            );
        }
        let mut store_changes = vec![StoreChange::MessageDelta {
            channel_id: channel_id.to_string(),
            message: message.clone(),
            kind,
        }];
        if kind == MessageMutationKind::Posted {
            if let Some(effect) = attention_effect.as_ref() {
                let self_authored = effect
                    .decision
                    .reasons
                    .contains(&crate::attention::AttentionReason::SelfAuthored);
                if !self_authored {
                    if let Some(entry) = self.conversations.get_mut(channel_id) {
                        let at_or_before_local_read =
                            entry.value.local_read_ts().is_some_and(|last_read| {
                                !slack_timestamp_is_after(&effect.message.ts, last_read)
                            });
                        if !at_or_before_local_read
                            && entry.value.observe_attention_message_at(
                                &effect.message.ts,
                                effect.decision.record_unread,
                            )
                        {
                            entry.unread_revision = revision;
                            let observations = vec![ConversationAttentionObservation {
                                message_ts: effect.message.ts.clone(),
                                record_unread: effect.decision.record_unread,
                            }];
                            patch_changes.push(WorkspaceChange::ConversationAttentionObserved {
                                channel_id: channel_id.to_string(),
                                observations: observations.clone(),
                            });
                            store_changes.push(StoreChange::ConversationAttentionObserved {
                                channel_id: channel_id.to_string(),
                                observations,
                            });
                        }
                    }
                }
            }
        }
        let effects = attention_effect
            .map(WorkspaceEffect::MessageAttention)
            .into_iter()
            .collect();
        self.commit_with_effects(revision, patch_changes, store_changes, effects)
    }

    fn message_delivery_state(
        &self,
        channel_id: &str,
        message: &SlackMessage,
        origin: MutationOrigin,
    ) -> DeliveryState {
        if origin == MutationOrigin::Realtime
            && self
                .conversation(channel_id)
                .and_then(SlackConversation::last_read_ts)
                .is_some_and(|last_read| !slack_timestamp_is_after(&message.ts, last_read))
        {
            DeliveryState::Stale
        } else {
            DeliveryState::Fresh
        }
    }

    fn message_attention_effect(
        &self,
        channel_id: &str,
        message: &SlackMessage,
        kind: MessageMutationKind,
        origin: MutationOrigin,
        delivery: DeliveryState,
    ) -> Option<MessageAttentionEffect> {
        if channel_id.trim().is_empty() || message.ts.trim().is_empty() {
            return None;
        }
        let conversation = self.conversation(channel_id);
        let conversation_kind = conversation.map_or_else(
            || {
                if channel_id.starts_with('D') {
                    ConversationKind::DirectMessage
                } else {
                    ConversationKind::Unknown
                }
            },
            |conversation| {
                if conversation.is_im.unwrap_or(false) {
                    ConversationKind::DirectMessage
                } else if conversation.is_mpim.unwrap_or(false) {
                    ConversationKind::GroupDirectMessage
                } else {
                    ConversationKind::Channel
                }
            },
        );
        let current_user_id = self.attention_context.current_user_id.as_deref();
        let author_is_self = origin == MutationOrigin::Local
            || message
                .user
                .as_deref()
                .zip(current_user_id)
                .is_some_and(|(author, current)| author == current);
        let has_content = message.has_visible_content();
        let visible_text = message.visible_text();
        // Window focus/navigation is delivered on a separate async lane from
        // realtime events. Keep it as a last-mile blocker so an older context
        // cannot permanently suppress an otherwise relevant notification.
        let actively_reading = false;
        let candidate = AttentionCandidate {
            text: &visible_text,
            subtype: message.subtype.as_deref(),
            mutation: match kind {
                MessageMutationKind::Posted => MessageMutation::Posted,
                MessageMutationKind::Changed => MessageMutation::Changed,
                MessageMutationKind::Deleted => MessageMutation::Deleted,
            },
            author_is_self,
            current_user_id,
            conversation: conversation_kind,
            thread_relationship: self.thread_relationship(channel_id, message),
            has_content,
            no_notifications: message.no_notifications.unwrap_or(false),
            muted: conversation.is_some_and(SlackConversation::is_muted_conversation),
            actively_reading,
            delivery,
        };
        Some(MessageAttentionEffect {
            channel_id: channel_id.to_string(),
            message: message.clone(),
            decision: self.attention_policy.decide(candidate),
            delivery,
        })
    }

    fn reconciled_unread_message_ts(
        &self,
        target: &TimelineTarget,
        messages: &[SlackMessage],
        origin: MutationOrigin,
    ) -> HashSet<String> {
        if !matches!(origin, MutationOrigin::Cache | MutationOrigin::WebApi) {
            return HashSet::new();
        }
        let channel_id = match target {
            TimelineTarget::Channel(channel_id) => channel_id,
            TimelineTarget::Thread { channel_id, .. } => channel_id,
        };
        let Some(conversation) = self.conversation(channel_id) else {
            return HashSet::new();
        };
        if origin == MutationOrigin::Cache && conversation.attention.is_some() {
            return HashSet::new();
        }
        if let TimelineTarget::Thread { thread_ts, .. } = target {
            let Some(record) = self.thread_catalog.iter().find(|record| {
                record.key.channel_id == *channel_id && record.key.root_ts == *thread_ts
            }) else {
                return HashSet::new();
            };
            let ThreadUnreadState::Known { count, last_read } = &record.unread else {
                return HashSet::new();
            };
            if let Some(last_read) = last_read {
                return messages
                    .iter()
                    .filter(|message| {
                        message.ts != *thread_ts && slack_timestamp_is_after(&message.ts, last_read)
                    })
                    .map(|message| message.ts.clone())
                    .collect();
            }
            return newest_message_ts(messages, *count as usize, Some(thread_ts));
        }
        if let Some(last_read) = conversation.last_read_ts() {
            return messages
                .iter()
                .filter(|message| slack_timestamp_is_after(&message.ts, last_read))
                .map(|message| message.ts.clone())
                .collect();
        }
        if !matches!(target, TimelineTarget::Channel(_)) {
            return HashSet::new();
        }
        let raw = conversation.raw_unread_state();
        if !raw.known || !raw.has_unread {
            return HashSet::new();
        }
        newest_message_ts(messages, raw.display_count.max(1) as usize, None)
    }

    fn thread_relationship(&self, channel_id: &str, message: &SlackMessage) -> ThreadRelationship {
        let Some(root_ts) = message.thread_root_ts() else {
            return ThreadRelationship::NotAReply;
        };
        let Some(current_user_id) = self.attention_context.current_user_id.as_deref() else {
            return ThreadRelationship::UnrelatedReply;
        };
        let root = self
            .histories
            .get(channel_id)
            .and_then(|timeline| timeline.messages.get(root_ts))
            .map(|entry| &entry.value);
        let record = self
            .thread_catalog
            .iter()
            .find(|record| record.key.channel_id == channel_id && record.key.root_ts == root_ts);
        let persisted_root = record.and_then(|record| record.root.as_ref());
        if [root, persisted_root]
            .into_iter()
            .flatten()
            .any(|root| root.user.as_deref() == Some(current_user_id))
        {
            return ThreadRelationship::Started;
        }
        let participated = [root, persisted_root]
            .into_iter()
            .flatten()
            .filter_map(|root| root.reply_users.as_ref())
            .flatten()
            .any(|user| user == current_user_id)
            || self
                .threads
                .get(&(channel_id.to_string(), root_ts.to_string()))
                .is_some_and(|timeline| {
                    timeline.messages.values().any(|entry| {
                        entry.value.user.as_deref() == Some(current_user_id)
                            && entry.value.ts != root_ts
                    })
                })
            || record.is_some_and(|record| record.participant_user_ids.contains(current_user_id));
        if participated {
            return ThreadRelationship::Participated;
        }
        if record.is_some_and(|record| record.subscribed == Some(true)) {
            ThreadRelationship::Subscribed
        } else {
            ThreadRelationship::UnrelatedReply
        }
    }

    fn reconcile_channel_roots_for_message(
        &mut self,
        channel_id: &str,
        message: &SlackMessage,
        kind: MessageMutationKind,
        previous_channel_known: bool,
        previous_replies: &[(String, SlackMessage)],
        revision: WorkspaceRevision,
    ) -> Vec<(SlackMessage, Option<SlackMessage>)> {
        let incoming_root = message.thread_root_ts().map(str::to_string);
        let mut root_timestamps = if kind == MessageMutationKind::Posted {
            Vec::new()
        } else {
            previous_replies
                .iter()
                .map(|(root_ts, _)| root_ts.clone())
                .collect::<Vec<_>>()
        };
        if let Some(root_ts) = incoming_root.as_ref() {
            root_timestamps.push(root_ts.clone());
        }
        root_timestamps.sort();
        root_timestamps.dedup();

        let transition_was_known = previous_channel_known || !previous_replies.is_empty();
        let mut changed_roots = Vec::new();
        for root_ts in root_timestamps {
            let previous = if kind == MessageMutationKind::Posted {
                None
            } else {
                previous_replies
                    .iter()
                    .find(|(known_root_ts, _)| known_root_ts == &root_ts)
                    .map(|(_, message)| message)
            };
            let next = (kind != MessageMutationKind::Deleted
                && incoming_root.as_deref() == Some(root_ts.as_str()))
            .then_some(message);
            let deletion_fallback = (kind == MessageMutationKind::Deleted
                && previous.is_none()
                && incoming_root.as_deref() == Some(root_ts.as_str()))
            .then_some(message);
            let old = previous.or(deletion_fallback);
            if old.is_none() && next.is_none() {
                continue;
            }

            let remaining_replies = self
                .threads
                .get(&(channel_id.to_string(), root_ts.clone()))
                .map(TimelineState::messages)
                .unwrap_or_default();
            let latest_remaining = remaining_replies
                .iter()
                .filter(|reply| reply.ts != root_ts)
                .map(|reply| reply.ts.as_str())
                .max()
                .map(str::to_string);
            let Some(root) = self
                .histories
                .get_mut(channel_id)
                .and_then(|timeline| timeline.messages.get_mut(&root_ts))
            else {
                continue;
            };
            let before = root.value.clone();

            match (old, next) {
                (Some(old), None) => {
                    let removal_was_reflected = previous.is_some()
                        || root.value.latest_reply.as_deref() == Some(old.ts.as_str());
                    if removal_was_reflected {
                        root.value.reply_count =
                            Some(root.value.reply_count.unwrap_or_default().saturating_sub(1));
                    }
                }
                (None, Some(_next)) => {
                    let addition_is_new = match kind {
                        MessageMutationKind::Posted => true,
                        MessageMutationKind::Changed => transition_was_known,
                        MessageMutationKind::Deleted => false,
                    };
                    if addition_is_new {
                        root.value.reply_count =
                            Some(root.value.reply_count.unwrap_or_default().saturating_add(1));
                    }
                }
                (Some(_), Some(_)) | (None, None) => {}
            }

            if old.is_some_and(|old| root.value.latest_reply.as_deref() == Some(old.ts.as_str())) {
                root.value.latest_reply.clone_from(&latest_remaining);
            }
            if let Some(next) = next {
                if root
                    .value
                    .latest_reply
                    .as_deref()
                    .is_none_or(|latest| slack_timestamp_is_after(&next.ts, latest))
                {
                    root.value.latest_reply = Some(next.ts.clone());
                }
            }

            let cached_replies = remaining_replies
                .iter()
                .filter(|reply| reply.thread_root_ts() == Some(root_ts.as_str()))
                .collect::<Vec<_>>();
            if root.value.reply_count == Some(0) {
                root.value.reply_users = Some(Vec::new());
            } else if root.value.reply_count == Some(cached_replies.len() as u64) {
                let mut users = Vec::new();
                for user_id in cached_replies
                    .iter()
                    .filter_map(|reply| reply.user.as_ref())
                {
                    if !users.iter().any(|known| known == user_id) {
                        users.push(user_id.clone());
                    }
                }
                root.value.reply_users = Some(users);
            } else if let Some(next_user_id) = next.and_then(|next| next.user.as_deref()) {
                let users = root.value.reply_users.get_or_insert_with(Vec::new);
                if !users.iter().any(|known| known == next_user_id) {
                    users.push(next_user_id.to_string());
                }
            }

            if root.value != before {
                root.revision = revision;
                let updated = root.value.clone();
                let thread_root_change = self
                    .threads
                    .get_mut(&(channel_id.to_string(), root_ts.clone()))
                    .and_then(|timeline| timeline.messages.get_mut(&root_ts))
                    .and_then(|thread_root| {
                        let before = (
                            thread_root.value.reply_count,
                            thread_root.value.latest_reply.clone(),
                            thread_root.value.reply_users.clone(),
                        );
                        thread_root.value.reply_count = updated.reply_count;
                        thread_root
                            .value
                            .latest_reply
                            .clone_from(&updated.latest_reply);
                        thread_root
                            .value
                            .reply_users
                            .clone_from(&updated.reply_users);
                        if before
                            == (
                                thread_root.value.reply_count,
                                thread_root.value.latest_reply.clone(),
                                thread_root.value.reply_users.clone(),
                            )
                        {
                            return None;
                        }
                        thread_root.revision = revision;
                        Some(thread_root.value.clone())
                    });
                changed_roots.push((updated, thread_root_change));
            }
        }
        changed_roots
    }

    fn apply_thread_catalog(
        &mut self,
        mut records: Vec<ThreadRecord>,
    ) -> Option<WorkspaceReduction> {
        records.sort_by(|left, right| {
            left.key
                .channel_id
                .cmp(&right.key.channel_id)
                .then_with(|| left.key.root_ts.cmp(&right.key.root_ts))
        });
        if self.thread_catalog == records {
            return None;
        }
        let revision = self.next_revision();
        self.thread_catalog = records.clone();
        self.commit(
            revision,
            vec![WorkspaceChange::ThreadCatalogChanged(records.clone())],
            vec![StoreChange::ThreadCatalogReplaced(records)],
        )
    }

    fn timeline_mut(&mut self, target: &TimelineTarget) -> &mut TimelineState {
        match target {
            TimelineTarget::Channel(channel_id) => {
                self.histories.entry(channel_id.clone()).or_default()
            }
            TimelineTarget::Thread {
                channel_id,
                thread_ts,
            } => self
                .threads
                .entry((channel_id.clone(), thread_ts.clone()))
                .or_default(),
        }
    }

    fn timeline(&self, target: &TimelineTarget) -> Option<&TimelineState> {
        match target {
            TimelineTarget::Channel(channel_id) => self.histories.get(channel_id),
            TimelineTarget::Thread {
                channel_id,
                thread_ts,
            } => self.threads.get(&(channel_id.clone(), thread_ts.clone())),
        }
    }
}

fn newest_message_ts(
    messages: &[SlackMessage],
    count: usize,
    excluded_ts: Option<&str>,
) -> HashSet<String> {
    let mut newest = messages
        .iter()
        .filter(|message| excluded_ts != Some(message.ts.as_str()))
        .collect::<Vec<_>>();
    newest.sort_by(|left, right| {
        if slack_timestamp_is_after(&left.ts, &right.ts) {
            std::cmp::Ordering::Less
        } else if slack_timestamp_is_after(&right.ts, &left.ts) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    newest
        .into_iter()
        .take(count)
        .map(|message| message.ts.clone())
        .collect()
}

fn timeline_from_messages(messages: &[SlackMessage], revision: WorkspaceRevision) -> TimelineState {
    TimelineState {
        messages: messages
            .iter()
            .cloned()
            .map(|message| {
                (
                    message.ts.clone(),
                    RevisionedValue {
                        value: message,
                        revision,
                    },
                )
            })
            .collect(),
        tombstones: HashMap::new(),
    }
}

fn store_timeline_replacement(target: &TimelineTarget, messages: Vec<SlackMessage>) -> StoreChange {
    match target {
        TimelineTarget::Channel(channel_id) => StoreChange::HistoryReplaced {
            channel_id: channel_id.clone(),
            messages,
        },
        TimelineTarget::Thread {
            channel_id,
            thread_ts,
        } => StoreChange::ThreadReplaced {
            channel_id: channel_id.clone(),
            thread_ts: thread_ts.clone(),
            messages,
        },
    }
}

fn message_belongs_in_target(message: &SlackMessage, target: &TimelineTarget) -> bool {
    match target {
        TimelineTarget::Channel(_) => message.belongs_in_channel_timeline(),
        TimelineTarget::Thread { thread_ts, .. } => message.belongs_to_thread(thread_ts),
    }
}

pub(crate) fn same_message_identity(left: &SlackMessage, right: &SlackMessage) -> bool {
    (!left.ts.trim().is_empty() && left.ts == right.ts)
        || left.client_msg_id.as_deref().is_some_and(|left_id| {
            !left_id.trim().is_empty() && right.client_msg_id.as_deref() == Some(left_id)
        })
}

fn preserve_missing_root_aggregates<'a>(
    message: &mut SlackMessage,
    previous: impl IntoIterator<Item = &'a SlackMessage>,
) {
    let previous = previous.into_iter().collect::<Vec<_>>();
    if message.reply_count.is_none() {
        message.reply_count = previous.iter().filter_map(|root| root.reply_count).max();
    }
    if message.latest_reply.is_none() {
        message.latest_reply = previous
            .iter()
            .filter_map(|root| root.latest_reply.as_ref())
            .max_by(|left, right| left.cmp(right))
            .cloned();
    }
    if message.reply_users.is_none() {
        let mut users = Vec::new();
        for user_id in previous
            .iter()
            .filter_map(|root| root.reply_users.as_ref())
            .flatten()
        {
            if !users.iter().any(|known| known == user_id) {
                users.push(user_id.clone());
            }
        }
        if !users.is_empty()
            || previous
                .iter()
                .any(|root| root.reply_users.as_ref().is_some())
        {
            message.reply_users = Some(users);
        }
    }
}

fn merge_conversation_metadata(current: &mut SlackConversation, incoming: &SlackConversation) {
    macro_rules! merge_option {
        ($field:ident) => {
            if incoming.$field.is_some() {
                current.$field.clone_from(&incoming.$field);
            }
        };
    }
    merge_option!(name);
    merge_option!(user);
    merge_option!(is_channel);
    merge_option!(is_group);
    merge_option!(is_im);
    merge_option!(is_mpim);
    merge_option!(is_private);
    merge_option!(is_archived);
    merge_option!(is_starred);
    for (key, value) in &incoming.extra {
        if !conversation_metadata_key_is_unread_owned(key) {
            current.extra.insert(key.clone(), value.clone());
        }
    }
}

fn sanitize_conversation_refresh_metadata(conversation: &mut SlackConversation) {
    conversation.is_starred = None;
    conversation.unread_count = None;
    conversation.attention = None;
    conversation
        .extra
        .retain(|key, _| !conversation_metadata_key_is_unread_owned(key));
}

fn conversation_supports_stars(conversation: &SlackConversation) -> bool {
    conversation.is_channel.unwrap_or(false)
        || conversation.is_group.unwrap_or(false)
        || conversation.is_private.unwrap_or(false)
        || conversation.is_im.unwrap_or(false)
        || conversation.is_mpim.unwrap_or(false)
}

impl WorkspaceReduction {
    pub(crate) fn new(
        revision: WorkspaceRevision,
        patch_changes: Vec<WorkspaceChange>,
        store_changes: Vec<StoreChange>,
    ) -> Option<Self> {
        Self::new_with_effects(revision, patch_changes, store_changes, Vec::new())
    }

    pub(crate) fn new_with_effects(
        revision: WorkspaceRevision,
        patch_changes: Vec<WorkspaceChange>,
        store_changes: Vec<StoreChange>,
        effects: Vec<WorkspaceEffect>,
    ) -> Option<Self> {
        let patch = WorkspacePatch::new(revision, patch_changes)?;
        let store_batch = StoreBatch::new(revision, store_changes);
        Some(Self {
            patch,
            store_batch,
            effects,
        })
    }

    pub(crate) fn patch(&self) -> &WorkspacePatch {
        &self.patch
    }

    pub(crate) fn store_batch(&self) -> Option<&StoreBatch> {
        self.store_batch.as_ref()
    }

    pub(crate) fn effects(&self) -> &[WorkspaceEffect] {
        &self.effects
    }
}

impl Default for WorkspaceCoordinator {
    fn default() -> Self {
        Self {
            revision: WorkspaceRevision::INITIAL,
            conversations: HashMap::new(),
            users: HashMap::new(),
            histories: HashMap::new(),
            threads: HashMap::new(),
            message_authority_by_ts: HashMap::new(),
            message_authority_by_client_id: HashMap::new(),
            thread_catalog: Vec::new(),
            attention_context: WorkspaceAttentionContext::default(),
            attention_preferences: AttentionPreferences::default(),
            attention_policy: AttentionPolicy::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(id: &str, name: &str) -> SlackConversation {
        SlackConversation {
            id: id.to_string(),
            name: Some(name.to_string()),
            is_channel: Some(true),
            ..Default::default()
        }
    }

    fn message(ts: &str, text: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.to_string(),
            text: Some(text.to_string()),
            ..Default::default()
        }
    }

    fn configure_attention(coordinator: &mut WorkspaceCoordinator) {
        coordinator.apply(WorkspaceMutation::AttentionContextChanged(
            WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".to_string()),
            },
        ));
    }

    fn attention_effect(reduction: &WorkspaceReduction) -> &MessageAttentionEffect {
        let Some(WorkspaceEffect::MessageAttention(effect)) = reduction.effects().last() else {
            panic!("expected a message attention effect");
        };
        effect
    }

    #[test]
    fn patch_and_store_batch_require_changes_and_share_one_revision() {
        let revision = WorkspaceRevision::INITIAL.successor();
        assert!(WorkspacePatch::new(
            WorkspaceRevision::INITIAL,
            vec![WorkspaceChange::ConversationRemoved {
                channel_id: "C1".to_string(),
            }],
        )
        .is_none());
        assert!(WorkspacePatch::new(revision, Vec::new()).is_none());
        assert!(StoreBatch::new(revision, Vec::new()).is_none());

        let reduction = WorkspaceReduction::new(
            revision,
            vec![WorkspaceChange::ConversationRemoved {
                channel_id: "C1".to_string(),
            }],
            vec![StoreChange::ConversationRemoved {
                channel_id: "C1".to_string(),
            }],
        )
        .expect("one logical change should produce one reduction");
        let patch = reduction.patch();
        let batch = reduction
            .store_batch()
            .expect("the persistent half should use the same revision");

        assert_eq!(patch.revision(), batch.revision());
        assert_eq!(patch.changes().len(), 1);
        assert_eq!(batch.changes().len(), 1);
    }

    #[test]
    fn cache_hydration_never_rewrites_an_incomplete_bootstrap_projection() {
        let mut coordinator = WorkspaceCoordinator::default();
        let reduction = coordinator
            .apply_from(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![conversation("C1", "general")],
                    ..Default::default()
                }),
            )
            .expect("cache hydration should update the coordinator");

        assert!(matches!(
            reduction.patch().changes(),
            [WorkspaceChange::BootstrapReset(_)]
        ));
        assert!(
            reduction.store_batch().is_none(),
            "the startup projection omits histories and must not replace persistent cache domains"
        );
    }

    #[test]
    fn coordinator_advances_once_and_suppresses_identical_mutations() {
        let mut coordinator = WorkspaceCoordinator::default();
        let changed = coordinator
            .apply(WorkspaceMutation::ConversationUpsert(conversation(
                "C1", "general",
            )))
            .expect("new conversation should change the workspace");

        assert_eq!(coordinator.revision().value(), 1);
        assert_eq!(changed.patch().revision(), coordinator.revision());
        assert_eq!(
            changed.store_batch().map(StoreBatch::revision),
            Some(coordinator.revision())
        );

        assert!(coordinator
            .apply(WorkspaceMutation::ConversationUpsert(conversation(
                "C1", "general",
            )))
            .is_none());
        assert_eq!(coordinator.revision().value(), 1);
    }

    #[test]
    fn generic_conversation_upsert_cannot_roll_back_an_authoritative_star() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut initial = conversation("C1", "general");
        initial.is_starred = Some(false);
        coordinator.apply(WorkspaceMutation::MembershipSnapshot(
            SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                ConversationMembershipSnapshot {
                    conversations: vec![initial],
                    starred_ids: Some(HashSet::new()),
                },
            ),
        ));
        coordinator.apply(WorkspaceMutation::ConversationStarChanged {
            channel_id: "C1".to_string(),
            starred: true,
        });

        let mut delayed = conversation("C1", "renamed");
        delayed.is_starred = Some(false);
        let reduction = coordinator
            .apply(WorkspaceMutation::ConversationUpsert(delayed.clone()))
            .expect("new metadata should still be applied");

        let current = coordinator.conversation("C1").unwrap();
        assert_eq!(current.name.as_deref(), Some("renamed"));
        assert!(current.is_starred());
        assert!(matches!(
            reduction.patch().changes(),
            [WorkspaceChange::ConversationUpsert(conversation)]
                if conversation.is_starred()
        ));
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::ConversationMetadataUpsert(conversation)]
                if conversation.is_starred()
        ));

        let revision = coordinator.revision();
        assert!(coordinator
            .apply(WorkspaceMutation::ConversationUpsert(delayed))
            .is_none());
        assert_eq!(coordinator.revision(), revision);
    }

    #[test]
    fn membership_star_projection_is_independent_of_metadata_and_newer_local_stars() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut initial = conversation("C1", "cached");
        initial.is_starred = Some(false);
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![initial],
            ..Default::default()
        }));
        let response_base = coordinator.revision();

        coordinator.apply_from(
            MutationOrigin::Realtime,
            WorkspaceMutation::ConversationUpsert(conversation("C1", "realtime rename")),
        );
        let mut opened = conversation("D1", "opened locally");
        opened.is_channel = Some(false);
        opened.is_im = Some(true);
        coordinator.apply_from(
            MutationOrigin::Local,
            WorkspaceMutation::ConversationUpsert(opened),
        );
        coordinator.apply(WorkspaceMutation::MembershipSnapshot(
            SnapshotEnvelope::new(
                response_base,
                ConversationMembershipSnapshot {
                    conversations: vec![conversation("C1", "stale membership")],
                    starred_ids: Some(HashSet::from(["C1".to_string(), "D1".to_string()])),
                },
            ),
        ));

        let current = coordinator.conversation("C1").unwrap();
        assert_eq!(current.name.as_deref(), Some("realtime rename"));
        assert!(current.is_starred());
        assert!(coordinator.conversation("D1").unwrap().is_starred());

        let next_response_base = coordinator.revision();
        coordinator.apply(WorkspaceMutation::ConversationStarChanged {
            channel_id: "C1".to_string(),
            starred: false,
        });
        coordinator.apply(WorkspaceMutation::MembershipSnapshot(
            SnapshotEnvelope::new(
                next_response_base,
                ConversationMembershipSnapshot {
                    conversations: vec![conversation("C1", "membership"), {
                        let mut direct = conversation("D1", "direct");
                        direct.is_channel = Some(false);
                        direct.is_im = Some(true);
                        direct
                    }],
                    starred_ids: Some(HashSet::from(["C1".to_string(), "D1".to_string()])),
                },
            ),
        ));
        assert!(
            !coordinator.conversation("C1").unwrap().is_starred(),
            "a star projection older than a local toggle must not roll it back"
        );
    }

    #[test]
    fn missing_star_projection_preserves_state_while_an_empty_projection_clears_it() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut initial = conversation("C1", "general");
        initial.is_starred = Some(true);
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![initial],
            ..Default::default()
        }));

        let base_revision = coordinator.revision();
        let mut untrusted = conversation("C1", "general");
        untrusted.is_starred = Some(false);
        assert!(coordinator
            .apply(WorkspaceMutation::MembershipSnapshot(
                SnapshotEnvelope::new(
                    base_revision,
                    ConversationMembershipSnapshot {
                        conversations: vec![untrusted.clone()],
                        starred_ids: None,
                    },
                ),
            ))
            .is_none());
        assert!(coordinator.conversation("C1").unwrap().is_starred());

        let reduction = coordinator
            .apply(WorkspaceMutation::MembershipSnapshot(
                SnapshotEnvelope::new(
                    coordinator.revision(),
                    ConversationMembershipSnapshot {
                        conversations: vec![untrusted],
                        starred_ids: Some(HashSet::new()),
                    },
                ),
            ))
            .expect("an authoritative empty star projection should clear the star");
        assert!(!coordinator.conversation("C1").unwrap().is_starred());
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::ConversationStarChanged {
                channel_id,
                starred: false,
            }] if channel_id == "C1"
        ));
    }

    #[test]
    fn stale_membership_snapshot_updates_metadata_without_replacing_unread_overlay() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C1", "general",
        )));
        let snapshot_revision = coordinator.revision();
        coordinator.apply(WorkspaceMutation::UnreadChanged {
            snapshot: SlackConversationUnreadSnapshot {
                channel_id: "C1".to_string(),
                unread_state: SlackUnreadState::from_parts(true, true, 4),
                ..Default::default()
            },
            base_revision: snapshot_revision,
        });
        let mut stale = conversation("C1", "renamed");
        stale.apply_unread_state(SlackUnreadState::from_parts(true, true, 99));

        coordinator.apply(WorkspaceMutation::MembershipSnapshot(
            SnapshotEnvelope::new(
                snapshot_revision,
                ConversationMembershipSnapshot {
                    conversations: vec![stale],
                    starred_ids: None,
                },
            ),
        ));

        let current = coordinator.conversation("C1").unwrap();
        assert_eq!(current.name.as_deref(), Some("renamed"));
        assert_eq!(current.unread_activity_count(), 4);
    }

    #[test]
    fn stale_unread_response_cannot_roll_back_a_newer_local_read() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C1", "general",
        )));
        let response_base = coordinator.revision();
        coordinator.apply(WorkspaceMutation::ReadAdvanced {
            channel_id: "C1".to_string(),
            ts: "20.0".to_string(),
            remaining_unread: 0,
        });
        let read_revision = coordinator.revision();

        assert!(coordinator
            .apply(WorkspaceMutation::UnreadChanged {
                snapshot: SlackConversationUnreadSnapshot {
                    channel_id: "C1".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, true, 5),
                    last_read: Some("10.0".to_string()),
                    latest: Some("30.0".to_string()),
                    ..Default::default()
                },
                base_revision: response_base,
            })
            .is_none());
        assert_eq!(coordinator.revision(), read_revision);
        let current = coordinator.conversation("C1").unwrap();
        assert_eq!(current.unread_activity_count(), 0);
        assert_eq!(
            current
                .extra
                .get("last_read")
                .and_then(|value| value.as_str()),
            Some("20.0")
        );

        let unrelated_base = coordinator.revision();
        coordinator.apply(WorkspaceMutation::UserUpsert(SlackUser {
            id: Some("U1".to_string()),
            name: Some("person".to_string()),
            ..Default::default()
        }));
        assert!(coordinator
            .apply(WorkspaceMutation::UnreadChanged {
                snapshot: SlackConversationUnreadSnapshot {
                    channel_id: "C1".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, true, 2),
                    last_read: Some("21.0".to_string()),
                    latest: Some("30.0".to_string()),
                    ..Default::default()
                },
                base_revision: unrelated_base,
            })
            .is_some());
        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .unread_activity_count(),
            0
        );
        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .raw_unread_activity_count(),
            2
        );
        assert_eq!(
            coordinator.conversation("C1").unwrap().latest_message_ts(),
            Some("30.0")
        );
    }

    #[test]
    fn cursorless_unread_snapshot_cannot_bypass_a_newer_local_read_marker() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C1", "general",
        )));
        coordinator.apply(WorkspaceMutation::ReadAdvanced {
            channel_id: "C1".to_string(),
            ts: "20.0".to_string(),
            remaining_unread: 0,
        });
        let response_base = coordinator.revision();

        assert!(coordinator
            .apply(WorkspaceMutation::UnreadChanged {
                snapshot: SlackConversationUnreadSnapshot {
                    channel_id: "C1".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, true, 5),
                    latest: Some("30.0".to_string()),
                    ..Default::default()
                },
                base_revision: response_base,
            })
            .is_none());
        assert_eq!(coordinator.revision(), response_base);
        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .unread_activity_count(),
            0
        );
        assert_eq!(
            coordinator.conversation("C1").unwrap().local_read_ts(),
            Some("20.0")
        );

        let acknowledgement = coordinator
            .apply(WorkspaceMutation::UnreadChanged {
                snapshot: SlackConversationUnreadSnapshot {
                    channel_id: "C1".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, false, 0),
                    last_read: Some("20.0".to_string()),
                    latest: Some("30.0".to_string()),
                    ..Default::default()
                },
                base_revision: response_base,
            })
            .expect("the server acknowledgement should clear the local marker");
        assert!(matches!(
            acknowledgement.patch().changes(),
            [WorkspaceChange::UnreadChanged { .. }]
        ));
        assert_eq!(
            coordinator.conversation("C1").unwrap().local_read_ts(),
            None
        );
    }

    #[test]
    fn conversation_refresh_batch_commits_metadata_and_unread_once_without_smuggling_state() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut initial = conversation("C1", "old");
        initial.is_starred = Some(true);
        initial.unread_count = Some(1);
        initial.extra.extend(HashMap::from([
            ("has_unreads".to_string(), serde_json::json!(true)),
            ("last_read".to_string(), serde_json::json!("1.0")),
            ("latest".to_string(), serde_json::json!("2.0")),
            ("mention_count".to_string(), serde_json::json!(1)),
            ("is_open".to_string(), serde_json::json!(true)),
            (LOCAL_READ_TS_KEY.to_string(), serde_json::json!("1.5")),
        ]));
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![initial],
            ..Default::default()
        }));
        let base_revision = coordinator.revision();

        let metadata = SlackConversation {
            id: "C1".to_string(),
            name: Some("renamed".to_string()),
            is_starred: Some(false),
            unread_count: Some(99),
            attention: Some(Default::default()),
            extra: HashMap::from([
                ("has_unreads".to_string(), serde_json::json!(true)),
                ("unread_count_display".to_string(), serde_json::json!(99)),
                ("last_read".to_string(), serde_json::json!("0.5")),
                ("latest".to_string(), serde_json::json!("99.0")),
                ("mention_count".to_string(), serde_json::json!(99)),
                ("is_open".to_string(), serde_json::json!(true)),
                (LOCAL_READ_TS_KEY.to_string(), serde_json::json!("99.0")),
            ]),
            ..Default::default()
        };
        let unread = SlackConversationUnreadSnapshot {
            channel_id: "C1".to_string(),
            unread_state: SlackUnreadState::from_parts(true, true, 4),
            last_read: Some("3.0".to_string()),
            latest: Some("4.0".to_string()),
            mention_count: Some(2),
            is_open: Some(false),
        };
        let reduction = coordinator
            .apply(WorkspaceMutation::ConversationRefreshBatch(vec![
                SnapshotEnvelope::new(
                    base_revision,
                    ConversationRefresh {
                        metadata: Some(metadata),
                        unread: Some(unread.clone()),
                    },
                ),
            ]))
            .expect("the refresh should update both independent domains");

        assert_eq!(reduction.patch().revision(), base_revision.successor());
        assert_eq!(coordinator.revision(), base_revision.successor());
        let [WorkspaceChange::ConversationMetadataUpsert(metadata_patch), WorkspaceChange::UnreadChanged {
            snapshot: unread_patch,
        }] = reduction.patch().changes()
        else {
            panic!("one refresh should produce one ordered metadata/unread patch");
        };
        assert_eq!(metadata_patch.name.as_deref(), Some("renamed"));
        assert_eq!(metadata_patch.is_starred, None);
        assert_eq!(metadata_patch.unread_count, None);
        assert_eq!(metadata_patch.attention, None);
        for protected in [
            "has_unreads",
            "unread_count_display",
            "last_read",
            "latest",
            "mention_count",
            "is_open",
            LOCAL_READ_TS_KEY,
        ] {
            assert!(
                !metadata_patch.extra.contains_key(protected),
                "metadata patch leaked unread-owned key {protected}"
            );
        }
        assert_eq!(unread_patch, &unread);

        let store_batch = reduction
            .store_batch()
            .expect("metadata and unread should share one atomic store batch");
        assert_eq!(store_batch.revision(), reduction.patch().revision());
        let [StoreChange::ConversationMetadataUpsert(stored_metadata), StoreChange::UnreadChanged {
            snapshot: stored_unread,
        }] = store_batch.changes()
        else {
            panic!("one refresh should produce one ordered metadata/unread store batch");
        };
        assert_eq!(stored_metadata, metadata_patch);
        assert_eq!(stored_unread, &unread);

        let current = coordinator.conversation("C1").unwrap();
        assert_eq!(current.name.as_deref(), Some("renamed"));
        assert!(current.is_starred());
        assert_eq!(current.raw_unread_activity_count(), 4);
        assert_eq!(current.last_read_ts(), Some("3.0"));
        assert_eq!(current.local_read_ts(), None);
        assert_eq!(current.latest_message_ts(), Some("4.0"));
        assert_eq!(
            current
                .extra
                .get("is_open")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            current
                .extra
                .get("mention_count")
                .and_then(serde_json::Value::as_u64),
            Some(2)
        );
    }

    #[test]
    fn conversation_refresh_rejects_blank_or_mismatched_component_ids_as_one_noop() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![conversation("C1", "one"), conversation("C2", "two")],
            ..Default::default()
        }));
        let base_revision = coordinator.revision();

        let invalid = vec![
            ConversationRefresh {
                metadata: Some(conversation("C1", "blank unread id")),
                unread: Some(SlackConversationUnreadSnapshot {
                    channel_id: " ".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, true, 1),
                    ..Default::default()
                }),
            },
            ConversationRefresh {
                metadata: Some(conversation("", "blank metadata id")),
                unread: Some(SlackConversationUnreadSnapshot {
                    channel_id: "C1".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, true, 2),
                    ..Default::default()
                }),
            },
            ConversationRefresh {
                metadata: Some(conversation("C1", "mismatched")),
                unread: Some(SlackConversationUnreadSnapshot {
                    channel_id: "C2".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, true, 3),
                    ..Default::default()
                }),
            },
        ];
        assert!(coordinator
            .apply(WorkspaceMutation::ConversationRefreshBatch(
                invalid
                    .into_iter()
                    .map(|refresh| SnapshotEnvelope::new(base_revision, refresh))
                    .collect(),
            ))
            .is_none());
        assert_eq!(coordinator.revision(), base_revision);
        assert_eq!(
            coordinator.conversation("C1").unwrap().name.as_deref(),
            Some("one")
        );
        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .unread_activity_count(),
            0
        );
        assert_eq!(
            coordinator.conversation("C2").unwrap().name.as_deref(),
            Some("two")
        );
        assert_eq!(
            coordinator
                .conversation("C2")
                .unwrap()
                .unread_activity_count(),
            0
        );
    }

    #[test]
    fn stale_or_unknown_refresh_unread_cannot_apply_metadata_is_open() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut initial = conversation("D1", "direct");
        initial.is_im = Some(true);
        initial
            .extra
            .insert("is_open".to_string(), serde_json::json!(true));
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![initial],
            ..Default::default()
        }));
        let stale_unread_base = coordinator.revision();
        coordinator.apply(WorkspaceMutation::ReadAdvanced {
            channel_id: "D1".to_string(),
            ts: "20.0".to_string(),
            remaining_unread: 0,
        });

        let stale_unread = coordinator
            .apply(WorkspaceMutation::ConversationRefreshBatch(vec![
                SnapshotEnvelope::new(
                    stale_unread_base,
                    ConversationRefresh {
                        metadata: Some(SlackConversation {
                            id: "D1".to_string(),
                            name: Some("stale unread details".to_string()),
                            extra: HashMap::from([(
                                "is_open".to_string(),
                                serde_json::json!(false),
                            )]),
                            ..Default::default()
                        }),
                        unread: Some(SlackConversationUnreadSnapshot {
                            channel_id: "D1".to_string(),
                            unread_state: SlackUnreadState::from_parts(true, true, 9),
                            last_read: Some("10.0".to_string()),
                            is_open: Some(false),
                            ..Default::default()
                        }),
                    },
                ),
            ]))
            .expect("fresh metadata should apply independently from stale unread");
        assert!(matches!(
            stale_unread.patch().changes(),
            [WorkspaceChange::ConversationMetadataUpsert(metadata)]
                if metadata.name.as_deref() == Some("stale unread details")
                    && !metadata.extra.contains_key("is_open")
        ));

        let unknown_base = coordinator.revision();
        let unknown_unread = coordinator
            .apply(WorkspaceMutation::ConversationRefreshBatch(vec![
                SnapshotEnvelope::new(
                    unknown_base,
                    ConversationRefresh {
                        metadata: Some(SlackConversation {
                            id: "D1".to_string(),
                            name: Some("unknown unread details".to_string()),
                            extra: HashMap::from([(
                                "is_open".to_string(),
                                serde_json::json!(false),
                            )]),
                            ..Default::default()
                        }),
                        unread: Some(SlackConversationUnreadSnapshot {
                            channel_id: "D1".to_string(),
                            unread_state: SlackUnreadState::default(),
                            is_open: Some(false),
                            ..Default::default()
                        }),
                    },
                ),
            ]))
            .expect("metadata should still apply when unread state is unknown");
        assert!(matches!(
            unknown_unread.patch().changes(),
            [WorkspaceChange::ConversationMetadataUpsert(metadata)]
                if metadata.name.as_deref() == Some("unknown unread details")
                    && !metadata.extra.contains_key("is_open")
        ));

        let current = coordinator.conversation("D1").unwrap();
        assert_eq!(current.name.as_deref(), Some("unknown unread details"));
        assert_eq!(
            current
                .extra
                .get("is_open")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(current.last_read_ts(), Some("20.0"));
        assert_eq!(current.local_read_ts(), Some("20.0"));
        assert_eq!(current.unread_activity_count(), 0);
    }

    #[test]
    fn conversation_refresh_domains_accept_independently_and_never_resurrect_removals() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![
                conversation("C1", "cached"),
                conversation("C2", "remove me"),
            ],
            ..Default::default()
        }));
        let stale_metadata_base = coordinator.revision();
        coordinator.apply_from(
            MutationOrigin::Realtime,
            WorkspaceMutation::ConversationUpsert(conversation("C1", "realtime")),
        );

        let unread = SlackConversationUnreadSnapshot {
            channel_id: "C1".to_string(),
            unread_state: SlackUnreadState::from_parts(true, true, 5),
            last_read: Some("2.0".to_string()),
            ..Default::default()
        };
        let unread_only = coordinator
            .apply(WorkspaceMutation::ConversationRefreshBatch(vec![
                SnapshotEnvelope::new(
                    stale_metadata_base,
                    ConversationRefresh {
                        metadata: Some(conversation("C1", "stale details")),
                        unread: Some(unread.clone()),
                    },
                ),
            ]))
            .expect("stale metadata must not block fresh unread state");
        assert!(matches!(
            unread_only.patch().changes(),
            [WorkspaceChange::UnreadChanged { snapshot }] if snapshot == &unread
        ));
        assert!(matches!(
            unread_only.store_batch().unwrap().changes(),
            [StoreChange::UnreadChanged { snapshot }] if snapshot == &unread
        ));
        assert_eq!(
            coordinator.conversation("C1").unwrap().name.as_deref(),
            Some("realtime")
        );

        let stale_unread_base = coordinator.revision();
        coordinator.apply(WorkspaceMutation::ReadAdvanced {
            channel_id: "C1".to_string(),
            ts: "20.0".to_string(),
            remaining_unread: 0,
        });
        let metadata_only = coordinator
            .apply(WorkspaceMutation::ConversationRefreshBatch(vec![
                SnapshotEnvelope::new(
                    stale_unread_base,
                    ConversationRefresh {
                        metadata: Some(conversation("C1", "fresh details")),
                        unread: Some(SlackConversationUnreadSnapshot {
                            channel_id: "C1".to_string(),
                            unread_state: SlackUnreadState::from_parts(true, true, 9),
                            ..Default::default()
                        }),
                    },
                ),
            ]))
            .expect("stale unread state must not block fresh metadata");
        assert!(matches!(
            metadata_only.patch().changes(),
            [WorkspaceChange::ConversationMetadataUpsert(conversation)]
                if conversation.name.as_deref() == Some("fresh details")
                    && conversation.unread_count.is_none()
        ));
        assert!(matches!(
            metadata_only.store_batch().unwrap().changes(),
            [StoreChange::ConversationMetadataUpsert(conversation)]
                if conversation.name.as_deref() == Some("fresh details")
                    && conversation.unread_activity_count() == 0
        ));

        let removed_base = coordinator.revision();
        coordinator.apply(WorkspaceMutation::ConversationRemove {
            channel_id: "C2".to_string(),
        });
        let removal_revision = coordinator.revision();
        assert!(coordinator
            .apply(WorkspaceMutation::ConversationRefreshBatch(vec![
                SnapshotEnvelope::new(
                    removed_base,
                    ConversationRefresh {
                        metadata: Some(conversation("C2", "resurrected")),
                        unread: Some(SlackConversationUnreadSnapshot {
                            channel_id: "C2".to_string(),
                            unread_state: SlackUnreadState::from_parts(true, true, 1),
                            ..Default::default()
                        }),
                    },
                ),
            ]))
            .is_none());
        assert_eq!(coordinator.revision(), removal_revision);
        assert!(coordinator.conversation("C2").is_none());
    }

    #[test]
    fn multi_conversation_refresh_uses_item_bases_and_resolves_duplicates_first() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![conversation("C1", "one"), conversation("C2", "two")],
            ..Default::default()
        }));
        let stale_base = coordinator.revision();
        coordinator.apply_from(
            MutationOrigin::Realtime,
            WorkspaceMutation::ConversationUpsert(conversation("C1", "realtime")),
        );
        let fresh_base = coordinator.revision();

        let reduction = coordinator
            .apply(WorkspaceMutation::ConversationRefreshBatch(vec![
                SnapshotEnvelope::new(
                    stale_base,
                    ConversationRefresh {
                        metadata: Some(conversation("C1", "stale")),
                        unread: Some(SlackConversationUnreadSnapshot {
                            channel_id: "C1".to_string(),
                            unread_state: SlackUnreadState::from_parts(true, true, 1),
                            ..Default::default()
                        }),
                    },
                ),
                SnapshotEnvelope::new(
                    fresh_base,
                    ConversationRefresh {
                        metadata: Some(conversation("C2", "first")),
                        unread: Some(SlackConversationUnreadSnapshot {
                            channel_id: "C2".to_string(),
                            unread_state: SlackUnreadState::from_parts(true, true, 2),
                            ..Default::default()
                        }),
                    },
                ),
                SnapshotEnvelope::new(
                    fresh_base,
                    ConversationRefresh {
                        metadata: Some(conversation("C2", "duplicate")),
                        unread: Some(SlackConversationUnreadSnapshot {
                            channel_id: "C2".to_string(),
                            unread_state: SlackUnreadState::from_parts(true, true, 9),
                            ..Default::default()
                        }),
                    },
                ),
            ]))
            .expect("the bounded refresh should commit all accepted items together");

        assert_eq!(coordinator.revision(), fresh_base.successor());
        assert_eq!(reduction.patch().revision(), fresh_base.successor());
        assert_eq!(reduction.patch().changes().len(), 3);
        assert_eq!(reduction.store_batch().unwrap().changes().len(), 3);
        assert!(matches!(
            reduction.patch().changes(),
            [
                WorkspaceChange::UnreadChanged { snapshot: first },
                WorkspaceChange::ConversationMetadataUpsert(second),
                WorkspaceChange::UnreadChanged { snapshot: third },
            ] if first.channel_id == "C1"
                && second.id == "C2"
                && second.name.as_deref() == Some("first")
                && third.channel_id == "C2"
                && third.unread_state.display_count == 2
        ));
        assert_eq!(
            coordinator.conversation("C1").unwrap().name.as_deref(),
            Some("realtime")
        );
        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .unread_activity_count(),
            1
        );
        assert_eq!(
            coordinator.conversation("C2").unwrap().name.as_deref(),
            Some("first")
        );
        assert_eq!(
            coordinator
                .conversation("C2")
                .unwrap()
                .unread_activity_count(),
            2
        );
    }

    #[test]
    fn acknowledging_thread_attention_ignores_filtered_replies_and_preserves_channel_unreads() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut channel = conversation("C1", "general");
        channel.observe_attention_message_at("2.0", true);
        channel.observe_attention_message_at("3.0", false);
        channel.observe_attention_message_at("10.0", true);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(channel));

        coordinator.apply(WorkspaceMutation::AttentionAcknowledged {
            channel_id: "C1".to_string(),
            message_ts: vec!["2.0".to_string(), "3.0".to_string()],
        });

        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .unread_activity_count(),
            1
        );
    }

    #[test]
    fn stale_history_snapshots_preserve_newer_posts_edits_and_deletes() {
        let mut coordinator = WorkspaceCoordinator::default();
        let empty_base = coordinator.revision();
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: message("10.0", "realtime"),
            kind: MessageMutationKind::Posted,
            origin: MutationOrigin::Realtime,
        });
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".to_string(),
            snapshot: SnapshotEnvelope::new(
                empty_base,
                MessagePage {
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        assert_eq!(
            coordinator.history("C1")[0].text.as_deref(),
            Some("realtime")
        );

        let old_edit_base = coordinator.revision();
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: message("10.0", "new edit"),
            kind: MessageMutationKind::Changed,
            origin: MutationOrigin::Realtime,
        });
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".to_string(),
            snapshot: SnapshotEnvelope::new(
                old_edit_base,
                MessagePage {
                    messages: vec![message("10.0", "old edit")],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        assert_eq!(
            coordinator.history("C1")[0].text.as_deref(),
            Some("new edit")
        );

        let old_delete_base = coordinator.revision();
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: message("10.0", "deleted"),
            kind: MessageMutationKind::Deleted,
            origin: MutationOrigin::Realtime,
        });
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".to_string(),
            snapshot: SnapshotEnvelope::new(
                old_delete_base,
                MessagePage {
                    messages: vec![message("10.0", "resurrected")],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        assert!(coordinator.history("C1").is_empty());
    }

    #[test]
    fn delete_tombstone_prevents_stale_snapshot_resurrection_without_loaded_history() {
        let mut coordinator = WorkspaceCoordinator::default();
        let snapshot_revision = coordinator.revision();
        assert!(coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: message("10.0", "deleted before hydration"),
                kind: MessageMutationKind::Deleted,
                origin: MutationOrigin::Realtime,
            })
            .is_some());

        assert!(coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".to_string(),
                snapshot: SnapshotEnvelope::new(
                    snapshot_revision,
                    MessagePage {
                        messages: vec![message("10.0", "stale")],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .is_none());
        assert!(coordinator.history("C1").is_empty());
    }

    #[test]
    fn local_send_and_realtime_echo_with_one_client_id_reduce_once() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut local = message("10.0", "hello");
        local.client_msg_id = Some("client-1".to_string());
        assert!(coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: local.clone(),
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Local,
            })
            .is_some());
        let revision = coordinator.revision();

        let mut echo = local;
        echo.ts = "10.1".to_string();
        echo.user = Some("U1".to_string());
        assert!(coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: echo,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .is_none());
        assert_eq!(coordinator.revision(), revision);
        assert_eq!(coordinator.history("C1").len(), 1);
    }

    #[test]
    fn posted_redelivery_with_the_same_slack_timestamp_is_a_noop() {
        let mut coordinator = WorkspaceCoordinator::default();
        let posted = message("10.0", "hello");
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: posted.clone(),
            kind: MessageMutationKind::Posted,
            origin: MutationOrigin::Realtime,
        });
        let revision = coordinator.revision();
        let mut redelivery = posted;
        redelivery.user = Some("U1".to_string());

        assert!(coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: redelivery,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .is_none());
        assert_eq!(coordinator.revision(), revision);
    }

    #[test]
    fn coordinator_classifies_realtime_before_unread_and_notification_effects_fan_out() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let mut direct = conversation("D1", "direct");
        direct.is_channel = Some(false);
        direct.is_im = Some(true);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(direct));
        let mut incoming = message("10.0", "hello");
        incoming.user = Some("U_OTHER".to_string());

        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "D1".to_string(),
                message: incoming,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        let effect = attention_effect(&reduction);
        assert!(effect.decision.record_unread);
        assert!(effect.decision.send_notification);
        assert_eq!(
            coordinator
                .conversation("D1")
                .unwrap()
                .raw_unread_activity_count(),
            0
        );
        assert_eq!(
            coordinator
                .conversation("D1")
                .unwrap()
                .unread_activity_count(),
            1
        );
    }

    #[test]
    fn coordinator_applies_live_attention_preferences_to_the_next_message() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let mut direct = conversation("D1", "direct");
        direct.is_channel = Some(false);
        direct.is_im = Some(true);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(direct));

        let classify = |coordinator: &mut WorkspaceCoordinator, ts: &str, text: &str| {
            let mut message = message(ts, text);
            message.user = Some("U_OTHER".to_string());
            coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "D1".to_string(),
                    message,
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Realtime,
                })
                .expect("message should produce a reduction")
        };

        let initial = classify(&mut coordinator, "10.0", "ordinary direct message");
        assert!(attention_effect(&initial).decision.send_notification);

        let revision = coordinator.revision();
        coordinator.apply(WorkspaceMutation::AttentionPreferencesChanged(
            AttentionPreferences {
                direct_messages: false,
                keywords: vec!["page me".to_string()],
                ..AttentionPreferences::default()
            },
        ));
        coordinator.apply(WorkspaceMutation::AttentionContextChanged(
            WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".to_string()),
            },
        ));
        assert_eq!(coordinator.revision(), revision);

        let disabled_direct = classify(&mut coordinator, "11.0", "another ordinary message");
        assert!(attention_effect(&disabled_direct).decision.record_unread);
        assert!(
            !attention_effect(&disabled_direct)
                .decision
                .send_notification
        );

        let keyword = classify(&mut coordinator, "12.0", "please page me now");
        assert!(attention_effect(&keyword).decision.record_unread);
        assert!(attention_effect(&keyword).decision.send_notification);
        assert!(attention_effect(&keyword)
            .decision
            .reasons
            .contains(&crate::attention::AttentionReason::KeywordOrPhrase));

        coordinator.apply(WorkspaceMutation::AttentionPreferencesChanged(
            AttentionPreferences {
                desktop_notifications: false,
                direct_messages: false,
                keywords: vec!["page me".to_string()],
                ..AttentionPreferences::default()
            },
        ));
        let globally_disabled = classify(&mut coordinator, "13.0", "please page me again");
        assert!(attention_effect(&globally_disabled).decision.record_unread);
        assert!(
            !attention_effect(&globally_disabled)
                .decision
                .send_notification
        );
    }

    #[test]
    fn coordinator_records_ordinary_channels_but_filters_membership_noise() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C1", "general",
        )));

        let mut ordinary = message("10.0", "hello channel");
        ordinary.user = Some("U_OTHER".to_string());
        let ordinary = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: ordinary,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(attention_effect(&ordinary).decision.record_unread);
        assert!(!attention_effect(&ordinary).decision.send_notification);

        let mut lifecycle = message("11.0", "joined");
        lifecycle.user = Some("U_OTHER".to_string());
        lifecycle.subtype = Some("channel_join".to_string());
        let lifecycle = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: lifecycle,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(!attention_effect(&lifecycle).decision.record_unread);
        assert!(!attention_effect(&lifecycle).decision.send_notification);
        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .unread_activity_count(),
            1
        );

        coordinator.apply(WorkspaceMutation::UnreadChanged {
            snapshot: SlackConversationUnreadSnapshot {
                channel_id: "C1".to_string(),
                unread_state: SlackUnreadState::from_parts(true, true, 2),
                ..Default::default()
            },
            base_revision: coordinator.revision(),
        });
        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .raw_unread_activity_count(),
            2
        );
        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .unread_activity_count(),
            1
        );
    }

    #[test]
    fn hydrated_message_identity_suppresses_restart_redelivery() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let mut direct = SlackConversation {
            id: "D1".to_string(),
            is_im: Some(true),
            ..Default::default()
        };
        direct.observe_attention_message_at("10.0", true);
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![direct],
            ..Default::default()
        }));
        let mut redelivery = message("10.0", "already delivered");
        redelivery.user = Some("U_OTHER".to_string());

        assert!(coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "D1".to_string(),
                message: redelivery,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .is_none());
    }

    #[test]
    fn attention_preview_is_pure_and_delivery_override_suppresses_rejected_attention() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C1", "general",
        )));
        let mut incoming = message("10.0", "new");
        incoming.user = Some("U_OTHER".to_string());
        let revision = coordinator.revision();

        let preview = coordinator
            .preview_message_attention(
                "C1",
                &incoming,
                MessageMutationKind::Posted,
                MutationOrigin::Realtime,
            )
            .unwrap();
        assert!(preview.decision.record_unread);
        assert_eq!(coordinator.revision(), revision);
        assert!(coordinator.history("C1").is_empty());
        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .unread_activity_count(),
            0
        );

        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChangedWithDelivery {
                channel_id: "C1".to_string(),
                message: incoming,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
                delivery: DeliveryState::Duplicate,
            })
            .unwrap();
        let effect = attention_effect(&reduction);
        assert_eq!(effect.delivery, DeliveryState::Duplicate);
        assert!(!effect.decision.record_unread);
        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .unread_activity_count(),
            0
        );
        let conversation = coordinator.conversation("C1").unwrap();
        assert!(conversation.has_observed_attention_message("10.0"));
        assert_eq!(coordinator.history("C1").len(), 1);
    }

    #[test]
    fn new_direct_message_ids_are_relevant_before_metadata_refresh() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let mut incoming = message("10.0", "hello");
        incoming.user = Some("U_OTHER".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "D_NEW".to_string(),
                message: incoming,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        assert!(attention_effect(&reduction)
            .decision
            .reasons
            .contains(&crate::attention::AttentionReason::DirectMessage));
        assert!(attention_effect(&reduction).decision.send_notification);
    }

    #[test]
    fn history_reconciliation_records_only_eligible_messages_after_last_read() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let mut channel = conversation("C1", "general");
        channel
            .extra
            .insert("last_read".to_string(), serde_json::json!("10.0"));
        channel.unread_count = Some(3);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(channel));

        let mut read = message("9.0", "old");
        read.user = Some("U_OTHER".to_string());
        let mut ordinary = message("11.0", "new");
        ordinary.user = Some("U_OTHER".to_string());
        let mut lifecycle = message("12.0", "joined");
        lifecycle.user = Some("U_OTHER".to_string());
        lifecycle.subtype = Some("channel_join".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".to_string(),
                snapshot: SnapshotEnvelope::new(
                    coordinator.revision(),
                    MessagePage {
                        messages: vec![read, ordinary, lifecycle],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .unwrap();

        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .unread_activity_count(),
            1
        );
        let conversation = coordinator.conversation("C1").unwrap();
        assert!(conversation.has_observed_attention_message("9.0"));
        assert!(conversation.has_observed_attention_message("11.0"));
        assert!(conversation.has_observed_attention_message("12.0"));
        assert_eq!(
            reduction
                .effects()
                .iter()
                .filter(|effect| {
                    matches!(
                        effect,
                        WorkspaceEffect::MessageAttention(effect)
                            if effect.delivery == DeliveryState::Reconciled
                    )
                })
                .count(),
            2
        );
        assert!(reduction.effects().iter().all(|effect| {
            let WorkspaceEffect::MessageAttention(effect) = effect;
            !effect.decision.send_notification
        }));
        coordinator.apply(WorkspaceMutation::UnreadChanged {
            snapshot: SlackConversationUnreadSnapshot {
                channel_id: "C1".to_string(),
                unread_state: SlackUnreadState::from_parts(true, true, 3),
                ..Default::default()
            },
            base_revision: coordinator.revision(),
        });
        assert_eq!(
            coordinator
                .conversation("C1")
                .unwrap()
                .unread_activity_count(),
            1
        );
    }

    #[test]
    fn history_and_posted_attention_companions_are_minimal_and_idempotent() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let mut channel = conversation("C1", "general");
        channel.is_starred = Some(true);
        channel.unread_count = Some(3);
        channel
            .extra
            .insert("last_read".to_string(), serde_json::json!("10.0"));
        channel
            .extra
            .insert("topic".to_string(), serde_json::json!("Keep me"));
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![channel],
            ..Default::default()
        }));

        let mut history_messages = vec![
            message("13.0", "history three"),
            message("11.0", "history one"),
            message("12.0", "history two"),
        ];
        for message in &mut history_messages {
            message.user = Some("U_OTHER".to_string());
        }
        let history = coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".to_string(),
                snapshot: SnapshotEnvelope::new(
                    coordinator.revision(),
                    MessagePage {
                        messages: history_messages,
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .unwrap();
        let [WorkspaceChange::TimelineChanged { .. }, WorkspaceChange::ConversationAttentionObserved {
            channel_id,
            observations,
        }] = history.patch().changes()
        else {
            panic!("history attention must use one semantic patch companion");
        };
        assert_eq!(channel_id, "C1");
        assert_eq!(
            observations,
            &[
                ConversationAttentionObservation {
                    message_ts: "11.0".to_string(),
                    record_unread: true,
                },
                ConversationAttentionObservation {
                    message_ts: "12.0".to_string(),
                    record_unread: true,
                },
                ConversationAttentionObservation {
                    message_ts: "13.0".to_string(),
                    record_unread: true,
                },
            ]
        );
        assert_eq!(
            history
                .effects()
                .iter()
                .map(|effect| {
                    let WorkspaceEffect::MessageAttention(effect) = effect;
                    effect.message.ts.as_str()
                })
                .collect::<Vec<_>>(),
            vec!["11.0", "12.0", "13.0"]
        );
        let [StoreChange::HistoryReplaced { .. }, StoreChange::ConversationAttentionObserved {
            channel_id,
            observations,
        }] = history.store_batch().unwrap().changes()
        else {
            panic!("history attention must use one semantic store companion");
        };
        assert_eq!(channel_id, "C1");
        assert_eq!(
            observations,
            &[
                ConversationAttentionObservation {
                    message_ts: "11.0".to_string(),
                    record_unread: true,
                },
                ConversationAttentionObservation {
                    message_ts: "12.0".to_string(),
                    record_unread: true,
                },
                ConversationAttentionObservation {
                    message_ts: "13.0".to_string(),
                    record_unread: true,
                },
            ]
        );

        let mut posted_message = message("14.0", "posted");
        posted_message.user = Some("U_OTHER".to_string());
        let posted = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: posted_message.clone(),
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(matches!(
            posted.patch().changes(),
            [
                WorkspaceChange::TimelineChanged { .. },
                WorkspaceChange::ConversationAttentionObserved {
                    channel_id,
                    observations,
                },
            ] if channel_id == "C1"
                && observations == &[ConversationAttentionObservation {
                    message_ts: "14.0".to_string(),
                    record_unread: true,
                }]
        ));
        assert!(matches!(
            posted.store_batch().unwrap().changes(),
            [
                StoreChange::MessageDelta { .. },
                StoreChange::ConversationAttentionObserved {
                    channel_id,
                    observations,
                },
            ] if channel_id == "C1"
                && observations == &[ConversationAttentionObservation {
                    message_ts: "14.0".to_string(),
                    record_unread: true,
                }]
        ));
        assert!(coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: posted_message,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .is_none());

        let current = coordinator.conversation("C1").unwrap();
        assert!(current.is_starred());
        assert_eq!(current.name.as_deref(), Some("general"));
        assert_eq!(current.last_read_ts(), Some("10.0"));
        assert_eq!(
            current.extra.get("topic"),
            Some(&serde_json::json!("Keep me"))
        );
        assert_eq!(current.raw_unread_activity_count(), 3);
        assert_eq!(current.unread_activity_count(), 4);
    }

    #[test]
    fn thread_participation_and_subscription_drive_relevance_reasons() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C1", "general",
        )));
        let mut root = message("10.0", "root");
        root.user = Some("U_OTHER".to_string());
        root.reply_users = Some(vec!["U_SELF".to_string()]);
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".to_string(),
            snapshot: SnapshotEnvelope::new(
                coordinator.revision(),
                MessagePage {
                    messages: vec![root],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        let mut reply = message("11.0", "reply");
        reply.user = Some("U_OTHER".to_string());
        reply.thread_ts = Some("10.0".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: reply,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        assert!(attention_effect(&reduction)
            .decision
            .reasons
            .contains(&crate::attention::AttentionReason::ParticipatedThreadReply));
        assert!(attention_effect(&reduction).decision.send_notification);
    }

    #[test]
    fn hydrated_thread_root_preserves_started_thread_relevance() {
        let mut root = message("10.0", "root");
        root.user = Some("U_SELF".to_string());
        root.reply_count = Some(1);
        let mut catalog = crate::thread_catalog::ThreadCatalog::default();
        catalog.observe_history("C1", std::slice::from_ref(&root));

        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![conversation("C1", "general")],
            threads: catalog.into_records(),
            ..Default::default()
        }));
        let mut reply = message("11.0", "reply");
        reply.user = Some("U_OTHER".to_string());
        reply.thread_ts = Some("10.0".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: reply,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        assert!(attention_effect(&reduction)
            .decision
            .reasons
            .contains(&crate::attention::AttentionReason::StartedThreadReply));
        assert!(attention_effect(&reduction).decision.send_notification);
    }

    #[test]
    fn local_reply_immediately_preserves_participated_thread_relevance() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C1", "general",
        )));
        let mut own_reply = message("11.0", "my reply");
        own_reply.user = Some("U_SELF".to_string());
        own_reply.thread_ts = Some("10.0".to_string());
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: own_reply,
            kind: MessageMutationKind::Posted,
            origin: MutationOrigin::Local,
        });

        let mut reply = message("12.0", "later reply");
        reply.user = Some("U_OTHER".to_string());
        reply.thread_ts = Some("10.0".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: reply,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        assert!(attention_effect(&reduction)
            .decision
            .reasons
            .contains(&crate::attention::AttentionReason::ParticipatedThreadReply));
        assert!(attention_effect(&reduction).decision.send_notification);
    }

    #[test]
    fn thread_reply_updates_root_metadata_without_entering_channel_timeline() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".to_string(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![message("10.0", "root")],
                    complete: true,
                    ..Default::default()
                },
            ),
        });

        let mut reply = message("11.0", "reply");
        reply.thread_ts = Some("10.0".to_string());
        reply.user = Some("U1".to_string());
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: reply.clone(),
            kind: MessageMutationKind::Posted,
            origin: MutationOrigin::Realtime,
        });

        let channel = coordinator.history("C1");
        assert_eq!(channel.len(), 1);
        assert_eq!(channel[0].ts, "10.0");
        assert_eq!(channel[0].reply_count, Some(1));
        assert_eq!(channel[0].latest_reply.as_deref(), Some("11.0"));
        assert_eq!(
            channel[0].reply_users.as_deref(),
            Some(&["U1".to_string()][..])
        );

        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: reply,
            kind: MessageMutationKind::Deleted,
            origin: MutationOrigin::Realtime,
        });
        let root = &coordinator.history("C1")[0];
        assert_eq!(root.reply_count, Some(0));
        assert_eq!(root.latest_reply, None);
        assert_eq!(root.reply_users.as_deref(), Some(&[][..]));
    }

    #[test]
    fn thread_broadcast_updates_root_once_and_appears_in_both_timelines() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".to_string(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![message("10.0", "root")],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        let mut broadcast = message("11.0", "broadcast");
        broadcast.thread_ts = Some("10.0".to_string());
        broadcast.subtype = Some("thread_broadcast".to_string());
        broadcast.client_msg_id = Some("broadcast-1".to_string());
        assert!(coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: broadcast.clone(),
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Local,
            })
            .is_some());

        let channel = coordinator.history("C1");
        assert_eq!(channel.len(), 2);
        assert_eq!(channel[0].reply_count, Some(1));
        assert_eq!(
            coordinator
                .threads
                .get(&("C1".to_string(), "10.0".to_string()))
                .unwrap()
                .messages()
                .len(),
            1
        );
        let revision = coordinator.revision();
        assert!(coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: broadcast,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .is_none());
        assert_eq!(coordinator.revision(), revision);
        assert_eq!(coordinator.history("C1")[0].reply_count, Some(1));
    }

    #[test]
    fn message_changes_emit_store_deltas_instead_of_unhydrated_replacements() {
        let mut coordinator = WorkspaceCoordinator::default();
        let posted = message("10.0", "posted");
        let post = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: posted.clone(),
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(matches!(
            post.store_batch().unwrap().changes(),
            [StoreChange::MessageDelta {
                channel_id,
                message,
                kind: MessageMutationKind::Posted,
            }] if channel_id == "C1"
                && message.ts == "10.0"
                && message.text.as_deref() == Some("posted")
        ));

        let edited = message("10.0", "edited");
        let edit = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: edited,
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(matches!(
            edit.store_batch().unwrap().changes(),
            [StoreChange::MessageDelta {
                channel_id,
                message,
                kind: MessageMutationKind::Changed,
            }] if channel_id == "C1"
                && message.ts == "10.0"
                && message.text.as_deref() == Some("edited")
        ));

        let delete = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: posted,
                kind: MessageMutationKind::Deleted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(matches!(
            delete.store_batch().unwrap().changes(),
            [StoreChange::MessageDelta {
                channel_id,
                message,
                kind: MessageMutationKind::Deleted,
            }] if channel_id == "C1" && message.ts == "10.0"
        ));
    }

    #[test]
    fn timeline_snapshots_and_pages_keep_full_store_replacements() {
        let mut coordinator = WorkspaceCoordinator::default();
        let history = coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".to_string(),
                snapshot: SnapshotEnvelope::new(
                    WorkspaceRevision::INITIAL,
                    MessagePage {
                        messages: vec![message("10.0", "history")],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .unwrap();
        assert!(matches!(
            history.store_batch().unwrap().changes(),
            [StoreChange::HistoryReplaced {
                channel_id,
                messages,
            }] if channel_id == "C1"
                && matches!(messages.as_slice(), [message] if message.ts == "10.0")
        ));

        let mut reply = message("11.0", "reply");
        reply.thread_ts = Some("10.0".to_string());
        let thread = coordinator
            .apply(WorkspaceMutation::ThreadPage {
                channel_id: "C1".to_string(),
                thread_ts: "10.0".to_string(),
                page: MessagePage {
                    messages: vec![reply],
                    complete: false,
                    ..Default::default()
                },
            })
            .unwrap();
        assert!(matches!(
            thread.store_batch().unwrap().changes(),
            [StoreChange::ThreadReplaced {
                channel_id,
                thread_ts,
                messages,
            }] if channel_id == "C1"
                && thread_ts == "10.0"
                && matches!(messages.as_slice(), [message] if message.ts == "11.0")
        ));
    }

    #[test]
    fn reply_patches_preserve_target_order_while_store_uses_one_message_delta() {
        let new_coordinator = || {
            let mut coordinator = WorkspaceCoordinator::default();
            coordinator.apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".to_string(),
                snapshot: SnapshotEnvelope::new(
                    WorkspaceRevision::INITIAL,
                    MessagePage {
                        messages: vec![message("10.0", "root")],
                        complete: true,
                        ..Default::default()
                    },
                ),
            });
            coordinator
        };

        let mut coordinator = new_coordinator();
        let mut reply = message("11.0", "reply");
        reply.thread_ts = Some("10.0".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: reply,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::MessageDelta {
                channel_id,
                message,
                kind: MessageMutationKind::Posted,
            }] if channel_id == "C1" && message.ts == "11.0"
        ));
        assert!(matches!(
            reduction.patch().changes(),
            [
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread { thread_ts, .. },
                    changes: thread_changes,
                },
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Channel(_),
                    changes: root_changes,
                },
            ] if thread_ts == "10.0"
                && matches!(
                    thread_changes.as_slice(),
                    [MessageChange::Upsert(message)] if message.ts == "11.0"
                )
                && matches!(
                    root_changes.as_slice(),
                    [MessageChange::Upsert(root)]
                        if root.ts == "10.0" && root.reply_count == Some(1)
                )
        ));

        let mut coordinator = new_coordinator();
        let mut broadcast = message("12.0", "broadcast");
        broadcast.thread_ts = Some("10.0".to_string());
        broadcast.subtype = Some("thread_broadcast".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: broadcast,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::MessageDelta {
                channel_id,
                message,
                kind: MessageMutationKind::Posted,
            }] if channel_id == "C1" && message.ts == "12.0"
        ));
        assert!(matches!(
            reduction.patch().changes(),
            [
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Channel(_),
                    changes: channel_changes,
                },
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread { thread_ts, .. },
                    changes: thread_changes,
                },
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Channel(_),
                    changes: root_changes,
                },
            ] if thread_ts == "10.0"
                && matches!(
                    channel_changes.as_slice(),
                    [MessageChange::Upsert(message)] if message.ts == "12.0"
                )
                && matches!(
                    thread_changes.as_slice(),
                    [MessageChange::Upsert(message)] if message.ts == "12.0"
                )
                && matches!(
                    root_changes.as_slice(),
                    [MessageChange::Upsert(root)]
                        if root.ts == "10.0" && root.reply_count == Some(1)
                )
        ));
    }

    #[test]
    fn changed_replies_remove_old_channel_and_thread_projections() {
        let new_coordinator = || {
            let mut coordinator = WorkspaceCoordinator::default();
            coordinator.apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".to_string(),
                snapshot: SnapshotEnvelope::new(
                    WorkspaceRevision::INITIAL,
                    MessagePage {
                        messages: vec![
                            message("10.0", "first root"),
                            message("20.0", "second root"),
                        ],
                        complete: true,
                        ..Default::default()
                    },
                ),
            });
            coordinator
        };

        let mut coordinator = new_coordinator();
        let mut broadcast = message("11.0", "broadcast");
        broadcast.thread_ts = Some("10.0".to_string());
        broadcast.subtype = Some("thread_broadcast".to_string());
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: broadcast.clone(),
            kind: MessageMutationKind::Posted,
            origin: MutationOrigin::Realtime,
        });

        let mut normal = broadcast;
        normal.subtype = None;
        normal.text = Some("normal".to_string());
        let removal = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: normal.clone(),
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(!coordinator
            .history("C1")
            .iter()
            .any(|message| message.ts == "11.0"));
        assert!(matches!(
            removal.store_batch().unwrap().changes(),
            [StoreChange::MessageDelta {
                channel_id,
                message,
                kind: MessageMutationKind::Changed,
            }] if channel_id == "C1"
                && message.ts == "11.0"
                && message.subtype.is_none()
        ));

        let mut restored = normal;
        restored.subtype = Some("thread_broadcast".to_string());
        let restoration = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: restored,
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(coordinator
            .history("C1")
            .iter()
            .any(|message| message.ts == "11.0"));
        assert!(matches!(
            restoration.store_batch().unwrap().changes(),
            [StoreChange::MessageDelta {
                channel_id,
                message,
                kind: MessageMutationKind::Changed,
            }] if channel_id == "C1"
                && message.ts == "11.0"
                && message.subtype.as_deref() == Some("thread_broadcast")
        ));

        let mut coordinator = new_coordinator();
        let mut reply = message("11.0", "first thread");
        reply.thread_ts = Some("10.0".to_string());
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: reply.clone(),
            kind: MessageMutationKind::Posted,
            origin: MutationOrigin::Realtime,
        });
        reply.thread_ts = Some("20.0".to_string());
        reply.text = Some("second thread".to_string());
        let moved = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: reply,
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(matches!(
            moved.store_batch().unwrap().changes(),
            [StoreChange::MessageDelta {
                channel_id,
                message,
                kind: MessageMutationKind::Changed,
            }] if channel_id == "C1"
                && message.ts == "11.0"
                && message.thread_ts.as_deref() == Some("20.0")
        ));
        assert!(coordinator
            .threads
            .get(&("C1".to_string(), "10.0".to_string()))
            .unwrap()
            .messages()
            .is_empty());
        assert_eq!(
            coordinator
                .threads
                .get(&("C1".to_string(), "20.0".to_string()))
                .unwrap()
                .messages()[0]
                .text
                .as_deref(),
            Some("second thread")
        );
    }

    #[test]
    fn reply_identity_transitions_reconcile_root_aggregates() {
        let new_coordinator = || {
            let mut coordinator = WorkspaceCoordinator::default();
            coordinator.apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".to_string(),
                snapshot: SnapshotEnvelope::new(
                    WorkspaceRevision::INITIAL,
                    MessagePage {
                        messages: vec![
                            message("10.0", "first root"),
                            message("20.0", "second root"),
                        ],
                        complete: true,
                        ..Default::default()
                    },
                ),
            });
            coordinator
        };
        let root = |coordinator: &WorkspaceCoordinator, root_ts: &str| {
            coordinator
                .histories
                .get("C1")
                .unwrap()
                .messages
                .get(root_ts)
                .unwrap()
                .value
                .clone()
        };

        let mut coordinator = new_coordinator();
        let mut reply = message("11.0", "reply");
        reply.thread_ts = Some("10.0".to_string());
        reply.client_msg_id = Some("reply-1".to_string());
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: reply.clone(),
            kind: MessageMutationKind::Posted,
            origin: MutationOrigin::Realtime,
        });
        reply.ts = "12.0".to_string();
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: reply.clone(),
            kind: MessageMutationKind::Changed,
            origin: MutationOrigin::Realtime,
        });
        assert_eq!(root(&coordinator, "10.0").reply_count, Some(1));
        assert_eq!(
            root(&coordinator, "10.0").latest_reply.as_deref(),
            Some("12.0")
        );

        reply.thread_ts = Some("20.0".to_string());
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: reply.clone(),
            kind: MessageMutationKind::Changed,
            origin: MutationOrigin::Realtime,
        });
        assert_eq!(root(&coordinator, "10.0").reply_count, Some(0));
        assert_eq!(root(&coordinator, "10.0").latest_reply, None);
        assert_eq!(root(&coordinator, "20.0").reply_count, Some(1));
        assert_eq!(
            root(&coordinator, "20.0").latest_reply.as_deref(),
            Some("12.0")
        );

        reply.thread_ts = None;
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: reply,
            kind: MessageMutationKind::Changed,
            origin: MutationOrigin::Realtime,
        });
        assert_eq!(root(&coordinator, "20.0").reply_count, Some(0));
        assert_eq!(root(&coordinator, "20.0").latest_reply, None);

        let mut coordinator = new_coordinator();
        let mut reply = message("11.0", "reply");
        reply.thread_ts = Some("10.0".to_string());
        reply.client_msg_id = Some("reply-2".to_string());
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: reply.clone(),
            kind: MessageMutationKind::Posted,
            origin: MutationOrigin::Realtime,
        });
        reply.thread_ts = None;
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: reply,
            kind: MessageMutationKind::Deleted,
            origin: MutationOrigin::Realtime,
        });
        assert_eq!(root(&coordinator, "10.0").reply_count, Some(0));
        assert_eq!(root(&coordinator, "10.0").latest_reply, None);
    }

    #[test]
    fn older_posted_reply_increments_count_and_updates_loaded_root_copies() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut root = message("10.0", "root");
        root.reply_count = Some(2);
        root.latest_reply = Some("20.0".to_string());
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".to_string(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![root.clone()],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        coordinator.apply(WorkspaceMutation::ThreadSnapshot {
            channel_id: "C1".to_string(),
            thread_ts: "10.0".to_string(),
            snapshot: SnapshotEnvelope::new(
                coordinator.revision(),
                MessagePage {
                    messages: vec![root],
                    complete: true,
                    ..Default::default()
                },
            ),
        });

        let mut reply = message("15.0", "older reply");
        reply.thread_ts = Some("10.0".to_string());
        reply.user = Some("U1".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: reply,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        let channel_root = coordinator
            .history("C1")
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        let thread_root = coordinator
            .threads
            .get(&("C1".to_string(), "10.0".to_string()))
            .unwrap()
            .messages()
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        assert_eq!(channel_root.reply_count, Some(3));
        assert_eq!(channel_root.latest_reply.as_deref(), Some("20.0"));
        assert_eq!(thread_root, channel_root);
        assert!(matches!(
            reduction.patch().changes(),
            [
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread { .. },
                    changes: reply_changes,
                },
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Channel(_),
                    changes: channel_root_changes,
                },
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread { .. },
                    changes: thread_root_changes,
                },
            ] if matches!(
                    reply_changes.as_slice(),
                    [MessageChange::Upsert(message)] if message.ts == "15.0"
                )
                && matches!(
                    channel_root_changes.as_slice(),
                    [MessageChange::Upsert(message)] if message.reply_count == Some(3)
                )
                && matches!(
                    thread_root_changes.as_slice(),
                    [MessageChange::Upsert(message)] if message.reply_count == Some(3)
                )
        ));
    }

    #[test]
    fn reply_aggregate_patch_preserves_projection_specific_root_content() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut channel_root = message("10.0", "channel snapshot");
        channel_root.reply_count = Some(0);
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".to_string(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![channel_root.clone()],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        let mut thread_root = channel_root;
        thread_root.text = Some("newer thread snapshot".to_string());
        coordinator.apply(WorkspaceMutation::ThreadSnapshot {
            channel_id: "C1".to_string(),
            thread_ts: "10.0".to_string(),
            snapshot: SnapshotEnvelope::new(
                coordinator.revision(),
                MessagePage {
                    messages: vec![thread_root],
                    complete: true,
                    ..Default::default()
                },
            ),
        });

        let mut reply = message("11.0", "reply");
        reply.thread_ts = Some("10.0".to_string());
        reply.user = Some("U1".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: reply,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        let channel_root = coordinator
            .history("C1")
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        let thread_root = coordinator
            .threads
            .get(&("C1".to_string(), "10.0".to_string()))
            .unwrap()
            .messages()
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        assert_eq!(channel_root.text.as_deref(), Some("channel snapshot"));
        assert_eq!(thread_root.text.as_deref(), Some("newer thread snapshot"));
        assert_eq!(channel_root.reply_count, thread_root.reply_count);
        assert_eq!(channel_root.latest_reply, thread_root.latest_reply);
        assert_eq!(channel_root.reply_users, thread_root.reply_users);
        assert!(matches!(
            reduction.patch().changes().last(),
            Some(WorkspaceChange::TimelineChanged {
                target: TimelineTarget::Thread { .. },
                changes,
            }) if matches!(
                changes.as_slice(),
                [MessageChange::Upsert(root)]
                    if root.text.as_deref() == Some("newer thread snapshot")
            )
        ));
    }

    #[test]
    fn partial_loaded_reply_delete_preserves_users_in_both_root_copies() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut channel_root = message("10.0", "channel root");
        channel_root.reply_count = Some(3);
        channel_root.latest_reply = Some("12.0".to_string());
        channel_root.reply_users = Some(vec!["U1".to_string(), "U2".to_string(), "U3".to_string()]);
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".to_string(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![channel_root.clone()],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        let mut thread_root = channel_root;
        thread_root.text = Some("thread root".to_string());
        let mut first_reply = message("11.0", "first");
        first_reply.thread_ts = Some("10.0".to_string());
        first_reply.user = Some("U1".to_string());
        let mut second_reply = message("12.0", "second");
        second_reply.thread_ts = Some("10.0".to_string());
        second_reply.user = Some("U2".to_string());
        coordinator.apply(WorkspaceMutation::ThreadSnapshot {
            channel_id: "C1".to_string(),
            thread_ts: "10.0".to_string(),
            snapshot: SnapshotEnvelope::new(
                coordinator.revision(),
                MessagePage {
                    messages: vec![thread_root, first_reply.clone(), second_reply],
                    complete: false,
                    ..Default::default()
                },
            ),
        });

        coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: first_reply,
                kind: MessageMutationKind::Deleted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        let channel_root = coordinator
            .history("C1")
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        let thread_root = coordinator
            .threads
            .get(&("C1".to_string(), "10.0".to_string()))
            .unwrap()
            .messages()
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        assert_eq!(channel_root.reply_count, Some(2));
        assert_eq!(channel_root.latest_reply.as_deref(), Some("12.0"));
        assert_eq!(
            channel_root.reply_users.as_deref(),
            Some(&["U1".to_string(), "U2".to_string(), "U3".to_string()][..])
        );
        assert_eq!(channel_root.reply_count, thread_root.reply_count);
        assert_eq!(channel_root.latest_reply, thread_root.latest_reply);
        assert_eq!(channel_root.reply_users, thread_root.reply_users);
        assert_eq!(thread_root.text.as_deref(), Some("thread root"));
    }

    #[test]
    fn edited_thread_root_survives_older_thread_snapshot() {
        let mut coordinator = WorkspaceCoordinator::default();
        let snapshot_base = coordinator.revision();
        let mut old_root = message("10.0", "old root");
        old_root.thread_ts = Some("10.0".to_string());
        let mut current_root = old_root.clone();
        current_root.text = Some("current root".to_string());
        coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: current_root,
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        let mut stale_reply = message("11.0", "stale page reply");
        stale_reply.thread_ts = Some("10.0".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".to_string(),
                thread_ts: "10.0".to_string(),
                snapshot: SnapshotEnvelope::new(
                    snapshot_base,
                    MessagePage {
                        messages: vec![old_root, stale_reply],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .unwrap();
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::ThreadReplaced { messages, .. }]
                if messages.iter().any(|message| {
                    message.ts == "10.0"
                        && message.text.as_deref() == Some("current root")
                })
        ));
    }

    #[test]
    fn unhydrated_sparse_root_edit_survives_older_thread_snapshot() {
        let old_root = message("10.0", "old root");
        let mut stale_reply = message("11.0", "stale page reply");
        stale_reply.thread_ts = Some("10.0".to_string());
        let mut catalog = crate::thread_catalog::ThreadCatalog::default();
        catalog.observe_history("C1", std::slice::from_ref(&stale_reply));

        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            histories: HashMap::from([("C1".to_string(), vec![old_root.clone()])]),
            threads: catalog.into_records(),
            ..Default::default()
        }));
        let snapshot_base = coordinator.revision();

        coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: message("10.0", "current root"),
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        let reduction = coordinator
            .apply(WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".to_string(),
                thread_ts: "10.0".to_string(),
                snapshot: SnapshotEnvelope::new(
                    snapshot_base,
                    MessagePage {
                        messages: vec![old_root, stale_reply],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .unwrap();

        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::ThreadReplaced { messages, .. }]
                if messages.iter().any(|message| {
                    message.ts == "10.0"
                        && message.text.as_deref() == Some("current root")
                })
        ));
    }

    #[test]
    fn root_edit_without_thread_ts_updates_loaded_thread_projection() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut root = message("10.0", "old root");
        root.reply_count = Some(2);
        root.latest_reply = Some("12.0".to_string());
        root.reply_users = Some(vec!["U1".to_string(), "U2".to_string()]);
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".to_string(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![root.clone()],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        coordinator.apply(WorkspaceMutation::ThreadSnapshot {
            channel_id: "C1".to_string(),
            thread_ts: "10.0".to_string(),
            snapshot: SnapshotEnvelope::new(
                coordinator.revision(),
                MessagePage {
                    messages: vec![root],
                    complete: true,
                    ..Default::default()
                },
            ),
        });

        let current_root = message("10.0", "current root");
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: current_root,
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert_eq!(
            coordinator
                .threads
                .get(&("C1".to_string(), "10.0".to_string()))
                .unwrap()
                .messages()
                .into_iter()
                .find(|message| message.ts == "10.0")
                .and_then(|message| message.text),
            Some("current root".to_string())
        );
        for root in [
            coordinator
                .history("C1")
                .into_iter()
                .find(|message| message.ts == "10.0")
                .unwrap(),
            coordinator
                .threads
                .get(&("C1".to_string(), "10.0".to_string()))
                .unwrap()
                .messages()
                .into_iter()
                .find(|message| message.ts == "10.0")
                .unwrap(),
        ] {
            assert_eq!(root.reply_count, Some(2));
            assert_eq!(root.latest_reply.as_deref(), Some("12.0"));
            assert_eq!(
                root.reply_users.as_deref(),
                Some(&["U1".to_string(), "U2".to_string()][..])
            );
        }
        assert!(matches!(
            reduction.patch().changes(),
            [
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Channel(_),
                    ..
                },
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread { thread_ts, .. },
                    ..
                },
            ] if thread_ts == "10.0"
        ));
    }

    #[test]
    fn same_identity_delete_removes_and_tombstones_known_and_incoming_timestamps() {
        let mut coordinator = WorkspaceCoordinator::default();
        let snapshot_base = coordinator.revision();
        let mut optimistic = message("10.0", "optimistic");
        optimistic.client_msg_id = Some("client-1".to_string());
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".to_string(),
            message: optimistic,
            kind: MessageMutationKind::Posted,
            origin: MutationOrigin::Local,
        });

        let mut deleted = message("11.0", "deleted");
        deleted.client_msg_id = Some("client-1".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: deleted.clone(),
                kind: MessageMutationKind::Deleted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::MessageDelta {
                channel_id,
                message,
                kind: MessageMutationKind::Deleted,
            }] if channel_id == "C1" && message.ts == "11.0"
        ));
        assert!(matches!(
            reduction.patch().changes(),
            [WorkspaceChange::TimelineChanged { changes, .. }]
                if matches!(
                    changes.as_slice(),
                    [
                        MessageChange::Remove { message_ts: known },
                        MessageChange::Remove { message_ts: incoming },
                    ] if known == "10.0" && incoming == "11.0"
                )
        ));

        assert!(coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".to_string(),
                snapshot: SnapshotEnvelope::new(
                    snapshot_base,
                    MessagePage {
                        messages: vec![deleted],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .is_none());
        assert!(coordinator.history("C1").is_empty());
    }

    #[test]
    fn unhydrated_projection_mutations_reject_older_cross_projection_snapshots() {
        for kind in [MessageMutationKind::Changed, MessageMutationKind::Deleted] {
            let mut coordinator = WorkspaceCoordinator::default();
            let snapshot_base = coordinator.revision();
            let mut stale_broadcast = message("11.0", "stale broadcast");
            stale_broadcast.thread_ts = Some("10.0".to_string());
            stale_broadcast.subtype = Some("thread_broadcast".to_string());
            stale_broadcast.client_msg_id = Some("client-1".to_string());

            let mut current = stale_broadcast.clone();
            current.subtype = if kind == MessageMutationKind::Changed {
                None
            } else {
                Some("message_deleted".to_string())
            };
            current.text = Some("current".to_string());
            coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "C1".to_string(),
                    message: current,
                    kind,
                    origin: MutationOrigin::Realtime,
                })
                .unwrap();

            assert!(coordinator
                .apply(WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        snapshot_base,
                        MessagePage {
                            messages: vec![stale_broadcast],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                })
                .is_none());
            assert!(coordinator.history("C1").is_empty());
        }
    }
}
