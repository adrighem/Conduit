/* workspace_pipeline.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Revisioned contracts shared by workspace producers, the pure reducer, presentation, and
//! persistence. This module intentionally has no dependency on GTK, Slack clients, or
//! SQLite so every input can follow the same deterministic path.

// These contracts are migrated surface-by-surface; the coordinator task wires their consumers.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use crate::attention::{
    AttentionCandidate, AttentionDecision, AttentionPolicy, AttentionPreferences, ConversationKind,
    DeliveryState, MessageMutation, ThreadRelationship,
};
use crate::models::{slack_timestamp_is_after, SlackConversation, SlackMessage, SlackUser};
use crate::thread_catalog::{ThreadCatalog, ThreadKey, ThreadRecord};
use crate::unread_ledger::{
    message_signal, ConversationReadState, MessageSignal, ServerReadCounts, UnreadLedger,
};

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
    pub(crate) read_states: UnreadLedger,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ConversationMembershipSnapshot {
    pub(crate) conversations: Vec<SlackConversation>,
    pub(crate) starred_ids: Option<HashSet<String>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ConversationRefresh {
    pub(crate) metadata: Option<SlackConversation>,
}

impl ConversationRefresh {
    pub(crate) fn potential_change_count(&self) -> usize {
        usize::from(self.metadata.is_some())
    }

    fn channel_id(&self) -> Option<&str> {
        self.metadata
            .as_ref()
            .map(|conversation| conversation.id.as_str())
            .filter(|metadata_id| !metadata_id.trim().is_empty())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct MessagePage {
    pub(crate) messages: Vec<SlackMessage>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) complete: bool,
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
    MessageUpdated {
        channel_id: String,
        original: Box<SlackMessage>,
        updated: SlackMessage,
    },
    ReactionChanged {
        channel_id: String,
        message_ts: String,
        name: String,
        user_id: String,
        added: bool,
    },
    ThreadCatalogChanged(Vec<ThreadRecord>),
    /// Local-only: marks a thread fully read up to its latest known reply.
    /// No Slack API backs thread-level read state.
    ThreadRead {
        channel_id: String,
        root_ts: String,
    },
    /// `client.counts` baseline for every conversation it lists.
    CountsSnapshot(Vec<ServerReadCounts>),
    /// Advances a conversation's read watermark (local read or `*_marked`).
    /// Older marks are ignored.
    ConversationMarked {
        channel_id: String,
        ts: String,
    },
    /// Explicit "mark unread": the only path that moves a watermark back.
    ConversationMarkedUnread {
        channel_id: String,
        ts: String,
    },
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
    ConversationRemoved {
        channel_id: String,
    },
    UsersReset(Vec<SlackUser>),
    UserUpsert(SlackUser),
    TimelineChanged {
        target: TimelineTarget,
        changes: Vec<MessageChange>,
    },
    ThreadCatalogChanged(Vec<ThreadRecord>),
    ReadStatesChanged(Vec<(String, ConversationReadState)>),
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
    ConversationRemoved {
        channel_id: String,
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
    HistoryDelta {
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
    ThreadDelta {
        channel_id: String,
        thread_ts: String,
        messages: Vec<SlackMessage>,
    },
    ThreadCatalogReplaced(Vec<ThreadRecord>),
    ThreadRecordsUpserted(Vec<ThreadRecord>),
    ReadStatesUpserted(Vec<(String, ConversationReadState)>),
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

    fn messages_with_revisions(&self) -> Vec<(SlackMessage, WorkspaceRevision)> {
        let mut messages = self
            .messages
            .values()
            .map(|entry| (entry.value.clone(), entry.revision))
            .collect::<Vec<_>>();
        messages.sort_by(|(left, _), (right, _)| left.ts.cmp(&right.ts));
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
    thread_catalog: ThreadCatalog,
    unread_ledger: UnreadLedger,
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

    pub(crate) fn read_state(&self, channel_id: &str) -> Option<&ConversationReadState> {
        self.unread_ledger.get(channel_id)
    }

    pub(crate) fn history(&self, channel_id: &str) -> Vec<SlackMessage> {
        self.histories
            .get(channel_id)
            .map(TimelineState::messages)
            .unwrap_or_default()
    }

    pub(crate) fn history_with_revisions(
        &self,
        channel_id: &str,
    ) -> Vec<(SlackMessage, WorkspaceRevision)> {
        self.histories
            .get(channel_id)
            .map(TimelineState::messages_with_revisions)
            .unwrap_or_default()
    }

    pub(crate) fn message(&self, channel_id: &str, message_ts: &str) -> Option<SlackMessage> {
        self.histories
            .get(channel_id)
            .and_then(|timeline| timeline.messages.get(message_ts))
            .map(|entry| entry.value.clone())
            .or_else(|| {
                self.threads
                    .iter()
                    .filter(|((known_channel_id, _), _)| known_channel_id == channel_id)
                    .find_map(|(_, timeline)| timeline.messages.get(message_ts))
                    .map(|entry| entry.value.clone())
            })
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
            WorkspaceMutation::MessageUpdated {
                channel_id,
                original,
                updated,
            } => self.apply_message_update(&channel_id, &original, updated),
            WorkspaceMutation::ReactionChanged {
                channel_id,
                message_ts,
                name,
                user_id,
                added,
            } => self.apply_reaction(&channel_id, &message_ts, &name, &user_id, added, origin),
            WorkspaceMutation::ThreadCatalogChanged(records) => self.apply_thread_catalog(records),
            WorkspaceMutation::ThreadRead {
                channel_id,
                root_ts,
            } => self.apply_thread_read(&channel_id, &root_ts),
            WorkspaceMutation::CountsSnapshot(counts) => self.apply_counts_snapshot(&counts),
            WorkspaceMutation::ConversationMarked { channel_id, ts } => {
                self.apply_conversation_marked(&channel_id, &ts)
            }
            WorkspaceMutation::ConversationMarkedUnread { channel_id, ts } => {
                self.apply_conversation_marked_unread(&channel_id, &ts)
            }
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
            && self.thread_catalog.to_records() == data.threads
            && self.unread_ledger == data.read_states;
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
        self.thread_catalog = ThreadCatalog::from_records(data.threads.clone());
        self.unread_ledger = data.read_states.clone();
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
        }

        self.commit(revision, patch_changes, store_changes)
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
        let user = merge_user_update(self.users.get(&user_id).map(|entry| &entry.value), user);
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

    fn apply_reaction(
        &mut self,
        channel_id: &str,
        message_ts: &str,
        name: &str,
        user_id: &str,
        added: bool,
        origin: MutationOrigin,
    ) -> Option<WorkspaceReduction> {
        if channel_id.trim().is_empty()
            || message_ts.trim().is_empty()
            || name.trim().is_empty()
            || user_id.trim().is_empty()
        {
            return None;
        }
        let mut message = self
            .histories
            .get(channel_id)
            .and_then(|timeline| timeline.messages.get(message_ts))
            .map(|entry| entry.value.clone())
            .or_else(|| {
                self.threads
                    .iter()
                    .filter(|((known_channel_id, _), _)| known_channel_id == channel_id)
                    .find_map(|(_, timeline)| timeline.messages.get(message_ts))
                    .map(|entry| entry.value.clone())
            })?;
        if !apply_reaction_to_message(&mut message, name, user_id, added) {
            return None;
        }
        let mut reduction = self.apply_message(
            channel_id,
            message,
            MessageMutationKind::Changed,
            origin,
            None,
        )?;
        reduction.effects.clear();
        Some(reduction)
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
        let page_complete = page.complete;
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
        let mut catalog_messages = Vec::new();
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
            catalog_messages.push(message.clone());
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
        if page_complete {
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
        let timeline_changed = !changes.is_empty();
        accepted_messages.sort_by(|left, right| left.ts.cmp(&right.ts));
        let store_change = if timeline_changed && origin != MutationOrigin::Cache {
            if page_complete {
                let messages = timeline.messages();
                Some(store_timeline_replacement(&target, messages))
            } else {
                Some(store_timeline_delta(&target, accepted_messages.clone()))
            }
        } else {
            None
        };
        let catalog_delta = match &target {
            TimelineTarget::Channel(channel_id) => self
                .thread_catalog
                .observe_history(channel_id, &catalog_messages),
            TimelineTarget::Thread {
                channel_id,
                thread_ts,
            } => self
                .thread_catalog
                .observe_thread(channel_id, thread_ts, &catalog_messages, page_complete)
                .map(|record| vec![record])
                .unwrap_or_default(),
        };
        if !timeline_changed && catalog_delta.is_empty() {
            return None;
        }
        let attention_effects = accepted_messages
            .into_iter()
            .filter_map(|message| {
                let channel_id = match &target {
                    TimelineTarget::Channel(channel_id) => channel_id.as_str(),
                    TimelineTarget::Thread { channel_id, .. } => channel_id.as_str(),
                };
                self.message_attention_effect(
                    channel_id,
                    &message,
                    MessageMutationKind::Posted,
                    origin,
                    DeliveryState::Historical,
                )
            })
            .collect::<Vec<_>>();
        let mut patch_changes = timeline_changed
            .then(|| WorkspaceChange::TimelineChanged { target, changes })
            .into_iter()
            .collect::<Vec<_>>();
        let mut store_changes = store_change.into_iter().collect::<Vec<_>>();
        if !catalog_delta.is_empty() {
            patch_changes.push(WorkspaceChange::ThreadCatalogChanged(catalog_delta.clone()));
            if origin != MutationOrigin::Cache {
                store_changes.push(StoreChange::ThreadRecordsUpserted(catalog_delta));
            }
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
        self.message_attention_effect(channel_id, message, kind, origin, DeliveryState::Fresh)
    }

    fn apply_message_update(
        &mut self,
        channel_id: &str,
        original: &SlackMessage,
        updated: SlackMessage,
    ) -> Option<WorkspaceReduction> {
        let freshest = self
            .histories
            .get(channel_id)
            .and_then(|timeline| timeline.messages.get(&original.ts))
            .map(|entry| entry.value.clone())
            .unwrap_or_else(|| original.clone());
        let message = merge_updated_message_content(freshest, updated);
        self.apply_message(
            channel_id,
            message,
            MessageMutationKind::Changed,
            MutationOrigin::Local,
            None,
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
            .get(channel_id, &message.ts)
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
        let catalog_own_thread_root = self.thread_catalog.get(channel_id, &message.ts).is_some();
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
        let delivery = delivery_override.unwrap_or(DeliveryState::Fresh);
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
        let current_user_id = self.attention_context.current_user_id.clone();
        let catalog_delta = match kind {
            MessageMutationKind::Posted => self
                .thread_catalog
                .observe_realtime(channel_id, &message, current_user_id.as_deref())
                .map(|record| vec![record])
                .unwrap_or_default(),
            MessageMutationKind::Changed => self
                .thread_catalog
                .observe_history(channel_id, std::slice::from_ref(&message)),
            MessageMutationKind::Deleted => Vec::new(),
        };
        if !catalog_delta.is_empty() {
            patch_changes.push(WorkspaceChange::ThreadCatalogChanged(catalog_delta.clone()));
            store_changes.push(StoreChange::ThreadRecordsUpserted(catalog_delta));
        }
        if patch_changes.is_empty() {
            return None;
        }
        if let Some(state) = self.observe_message_read_state(channel_id, &message, kind, origin) {
            let changed = vec![(channel_id.to_string(), state)];
            patch_changes.push(WorkspaceChange::ReadStatesChanged(changed.clone()));
            store_changes.push(StoreChange::ReadStatesUpserted(changed));
        }
        let effects = attention_effect
            .map(WorkspaceEffect::MessageAttention)
            .into_iter()
            .collect();
        self.commit_with_effects(revision, patch_changes, store_changes, effects)
    }

    /// Feeds one message mutation into the unread ledger. Only realtime and
    /// local mutations are live; history may never add inside the baseline.
    fn observe_message_read_state(
        &mut self,
        channel_id: &str,
        message: &SlackMessage,
        kind: MessageMutationKind,
        origin: MutationOrigin,
    ) -> Option<ConversationReadState> {
        let is_direct_message = self.conversation(channel_id).map_or_else(
            || channel_id.starts_with('D'),
            SlackConversation::is_direct_message,
        );
        let signal = match kind {
            MessageMutationKind::Deleted => MessageSignal::Ignore,
            MessageMutationKind::Posted | MessageMutationKind::Changed => message_signal(
                message,
                is_direct_message,
                self.attention_context.current_user_id.as_deref(),
            ),
        };
        let live = matches!(origin, MutationOrigin::Realtime | MutationOrigin::Local);
        self.unread_ledger
            .update(channel_id, |state| state.observe(&message.ts, signal, live))
    }

    fn commit_read_states(
        &mut self,
        changed: Vec<(String, ConversationReadState)>,
    ) -> Option<WorkspaceReduction> {
        if changed.is_empty() {
            return None;
        }
        let revision = self.next_revision();
        self.commit(
            revision,
            vec![WorkspaceChange::ReadStatesChanged(changed.clone())],
            vec![StoreChange::ReadStatesUpserted(changed)],
        )
    }

    fn apply_counts_snapshot(&mut self, counts: &[ServerReadCounts]) -> Option<WorkspaceReduction> {
        let changed = self
            .unread_ledger
            .apply_server_snapshot(counts)
            .into_iter()
            .filter_map(|channel_id| {
                let state = self.unread_ledger.get(&channel_id)?.clone();
                Some((channel_id, state))
            })
            .collect();
        self.commit_read_states(changed)
    }

    fn apply_conversation_marked(
        &mut self,
        channel_id: &str,
        ts: &str,
    ) -> Option<WorkspaceReduction> {
        let state = self
            .unread_ledger
            .update(channel_id, |state| state.mark_read(ts))?;
        self.commit_read_states(vec![(channel_id.to_string(), state)])
    }

    fn apply_conversation_marked_unread(
        &mut self,
        channel_id: &str,
        ts: &str,
    ) -> Option<WorkspaceReduction> {
        let is_direct_message = self.conversation(channel_id).map_or_else(
            || channel_id.starts_with('D'),
            SlackConversation::is_direct_message,
        );
        let current_user_id = self.attention_context.current_user_id.as_deref();
        let badge_ts = self
            .histories
            .get(channel_id)
            .map(|timeline| {
                timeline
                    .messages
                    .values()
                    .filter(|entry| {
                        message_signal(&entry.value, is_direct_message, current_user_id)
                            == MessageSignal::Badge
                    })
                    .map(|entry| entry.value.ts.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let state = self
            .unread_ledger
            .update(channel_id, |state| state.mark_unread(ts, badge_ts))?;
        self.commit_read_states(vec![(channel_id.to_string(), state)])
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
        let record = self.thread_catalog.get(channel_id, root_ts);
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
        if self.thread_catalog.to_records() == records {
            return None;
        }
        let revision = self.next_revision();
        self.thread_catalog = ThreadCatalog::from_records(records.clone());
        self.commit(
            revision,
            vec![WorkspaceChange::ThreadCatalogChanged(records.clone())],
            vec![StoreChange::ThreadCatalogReplaced(records)],
        )
    }

    fn apply_thread_read(&mut self, channel_id: &str, root_ts: &str) -> Option<WorkspaceReduction> {
        let mut records = self.thread_catalog.to_records();
        let record = match records
            .iter_mut()
            .find(|record| record.key.channel_id == channel_id && record.key.root_ts == root_ts)
        {
            Some(record) => record,
            None => {
                let key = ThreadKey::new(channel_id, root_ts)?;
                let mut rec = ThreadRecord::placeholder(key);
                if let Some(state) = self
                    .threads
                    .get(&(channel_id.to_string(), root_ts.to_string()))
                {
                    rec.reply_count = state.messages.len().saturating_sub(1) as u64;
                    rec.latest_reply = state.messages.values().map(|v| v.value.ts.clone()).max();
                }
                records.push(rec);
                records.last_mut()?
            }
        };
        record.mark_read();
        self.apply_thread_catalog(records)
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

fn merge_user_update(existing: Option<&SlackUser>, mut update: SlackUser) -> SlackUser {
    let Some(existing) = existing else {
        return update;
    };
    let mut merged = existing.clone();

    macro_rules! merge_user_fields {
        ($($field:ident),+ $(,)?) => {
            $(
                if update.$field.is_some() {
                    merged.$field = update.$field.take();
                }
            )+
        };
    }
    merge_user_fields!(id, name, real_name, deleted, is_bot, tz, tz_label, tz_offset,);

    let Some(mut profile_update) = update.profile.take() else {
        return merged;
    };
    let Some(profile) = merged.profile.as_mut() else {
        merged.profile = Some(profile_update);
        return merged;
    };

    macro_rules! merge_profile_fields {
        ($($field:ident),+ $(,)?) => {
            $(
                if profile_update.$field.is_some() {
                    profile.$field = profile_update.$field.take();
                }
            )+
        };
    }
    merge_profile_fields!(
        display_name,
        display_name_normalized,
        real_name,
        real_name_normalized,
        status_text,
        status_emoji,
        status_expiration,
        title,
        phone,
        email,
        skype,
        pronouns,
        about,
        location,
        image_72,
        image_192,
        image_512,
        image_original,
        huddle_state_call_id,
        huddle_state_channel_id,
        huddle_state_expiration_ts,
    );
    if profile_update.huddle_state != Default::default() {
        profile.huddle_state = profile_update.huddle_state;
    }
    if !profile_update.fields.is_empty() {
        profile.fields.extend(profile_update.fields);
    }
    merged
}

fn apply_reaction_to_message(
    message: &mut SlackMessage,
    name: &str,
    user_id: &str,
    added: bool,
) -> bool {
    let is_same_reaction = |existing: &str| {
        existing == name
            || matches!(
                (existing, name),
                ("thumbsup", "+1")
                    | ("+1", "thumbsup")
                    | ("thumbsdown", "-1")
                    | ("-1", "thumbsdown")
            )
    };
    let reactions = message.reactions.get_or_insert_with(Vec::new);
    let position = reactions
        .iter()
        .position(|reaction| reaction.name.as_deref().is_some_and(is_same_reaction));
    if added {
        if let Some(position) = position {
            let reaction = &mut reactions[position];
            if reaction.name.as_deref() == Some("thumbsup") && name == "+1" {
                reaction.name = Some("+1".to_string());
            } else if reaction.name.as_deref() == Some("thumbsdown") && name == "-1" {
                reaction.name = Some("-1".to_string());
            }
            let users = reaction.users.get_or_insert_with(Vec::new);
            if users.iter().any(|known| known == user_id) {
                return false;
            }
            users.push(user_id.to_string());
            reaction.count = Some(reaction.count.unwrap_or_default().saturating_add(1));
        } else {
            reactions.push(crate::models::SlackReaction {
                name: Some(name.to_string()),
                count: Some(1),
                users: Some(vec![user_id.to_string()]),
            });
        }
        return true;
    }

    let Some(position) = position else {
        return false;
    };
    let reaction = &mut reactions[position];
    if let Some(users) = reaction.users.as_mut() {
        let previous_len = users.len();
        users.retain(|known| known != user_id);
        if users.len() == previous_len {
            return false;
        }
    }
    let count = reaction.count.unwrap_or_default().saturating_sub(1);
    reaction.count = Some(count);
    if count == 0 {
        reactions.remove(position);
    }
    true
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

fn store_timeline_delta(target: &TimelineTarget, messages: Vec<SlackMessage>) -> StoreChange {
    match target {
        TimelineTarget::Channel(channel_id) => StoreChange::HistoryDelta {
            channel_id: channel_id.clone(),
            messages,
        },
        TimelineTarget::Thread {
            channel_id,
            thread_ts,
        } => StoreChange::ThreadDelta {
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
        current.extra.insert(key.clone(), value.clone());
    }
}

fn sanitize_conversation_refresh_metadata(conversation: &mut SlackConversation) {
    conversation.is_starred = None;
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

fn merge_updated_message_content(
    mut freshest: SlackMessage,
    updated: SlackMessage,
) -> SlackMessage {
    freshest.text = updated.text;
    freshest.blocks = updated.blocks;
    freshest.edited = updated.edited;
    if updated.user.is_some() {
        freshest.user = updated.user;
    }
    freshest.refresh_canonical_content();
    freshest
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
            thread_catalog: ThreadCatalog::default(),
            unread_ledger: UnreadLedger::default(),
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
    fn hydration_restores_persisted_read_state_without_deriving_from_history() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let read_states = UnreadLedger::from_entries([(
            "C1".to_string(),
            ConversationReadState {
                last_read: Some("1000.000000".to_string()),
                server_latest: Some("3000.000000".to_string()),
                server_mention_count: 1,
                server_has_unreads: true,
                ..Default::default()
            },
        )]);
        let histories = HashMap::from([(
            "C1".to_string(),
            vec![message("2000.000000", "hey <@U_SELF> check this out")],
        )]);

        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![conversation("C1", "chris")],
            histories,
            read_states,
            ..Default::default()
        }));

        let state = coordinator.read_state("C1").expect("read state hydrated");
        assert_eq!(state.badge_count(), 1);
        assert!(state.live_mentions.is_empty());
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
    fn stale_membership_snapshot_still_updates_conversation_metadata() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C1", "general",
        )));
        let snapshot_revision = coordinator.revision();
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C2", "random",
        )));

        coordinator.apply(WorkspaceMutation::MembershipSnapshot(
            SnapshotEnvelope::new(
                snapshot_revision,
                ConversationMembershipSnapshot {
                    conversations: vec![conversation("C1", "renamed")],
                    starred_ids: None,
                },
            ),
        ));

        let current = coordinator.conversation("C1").unwrap();
        assert_eq!(current.name.as_deref(), Some("renamed"));
        assert!(
            coordinator.conversation("C2").is_some(),
            "a stale snapshot must not remove a membership committed after its base"
        );
    }

    #[test]
    fn partial_user_upsert_preserves_known_identity_and_profile_fields() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator
            .apply(WorkspaceMutation::UserUpsert(SlackUser {
                id: Some("U1".into()),
                name: Some("person".into()),
                real_name: Some("Person One".into()),
                profile: Some(crate::models::SlackUserProfile {
                    display_name: Some("Person".into()),
                    image_72: Some("https://example.invalid/avatar.png".into()),
                    status_text: Some("Busy".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .unwrap();

        let reduction = coordinator
            .apply(WorkspaceMutation::UserUpsert(SlackUser {
                id: Some("U1".into()),
                profile: Some(crate::models::SlackUserProfile {
                    status_text: Some(String::new()),
                    status_emoji: Some(":white_check_mark:".into()),
                    status_expiration: Some(42),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .unwrap();

        let [WorkspaceChange::UserUpsert(user)] = reduction.patch().changes() else {
            panic!("expected one merged user upsert");
        };
        assert_eq!(user.name.as_deref(), Some("person"));
        assert_eq!(user.real_name.as_deref(), Some("Person One"));
        let profile = user.profile.as_ref().unwrap();
        assert_eq!(profile.display_name.as_deref(), Some("Person"));
        assert_eq!(
            profile.image_72.as_deref(),
            Some("https://example.invalid/avatar.png")
        );
        assert_eq!(profile.status_text.as_deref(), Some(""));
        assert_eq!(profile.status_emoji.as_deref(), Some(":white_check_mark:"));
        assert_eq!(profile.status_expiration, Some(42));
    }

    #[test]
    fn reaction_mutation_updates_the_canonical_message_once() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".into(),
                snapshot: SnapshotEnvelope::new(
                    WorkspaceRevision::INITIAL,
                    MessagePage {
                        messages: vec![message("1", "hello")],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .unwrap();

        let added = coordinator
            .apply(WorkspaceMutation::ReactionChanged {
                channel_id: "C1".into(),
                message_ts: "1".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            })
            .unwrap();
        assert!(matches!(
            added.patch().changes(),
            [WorkspaceChange::TimelineChanged { changes, .. }]
                if matches!(changes.as_slice(), [MessageChange::Upsert(message)]
                    if message.reactions.as_ref().is_some_and(|reactions| {
                        reactions.iter().any(|reaction| reaction.name.as_deref() == Some("wave")
                            && reaction.count == Some(1)
                            && reaction.users.as_ref().is_some_and(|users| users == &["U1"]))
                    }))
        ));
        assert!(coordinator
            .apply(WorkspaceMutation::ReactionChanged {
                channel_id: "C1".into(),
                message_ts: "1".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            })
            .is_none());

        coordinator
            .apply(WorkspaceMutation::ReactionChanged {
                channel_id: "C1".into(),
                message_ts: "1".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: false,
            })
            .unwrap();
        assert!(coordinator.history("C1")[0]
            .reactions
            .as_ref()
            .is_some_and(Vec::is_empty));
    }

    #[test]
    fn conversation_refresh_commits_metadata_without_smuggling_star_state() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut initial = conversation("C1", "old");
        initial.is_starred = Some(true);
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![initial],
            ..Default::default()
        }));
        let base_revision = coordinator.revision();

        let metadata = SlackConversation {
            id: "C1".to_string(),
            name: Some("renamed".to_string()),
            is_starred: Some(false),
            extra: HashMap::from([("topic".to_string(), serde_json::json!("standup"))]),
            ..Default::default()
        };
        let reduction = coordinator
            .apply(WorkspaceMutation::ConversationRefreshBatch(vec![
                SnapshotEnvelope::new(
                    base_revision,
                    ConversationRefresh {
                        metadata: Some(metadata),
                    },
                ),
            ]))
            .expect("the refresh should update conversation metadata");

        assert_eq!(reduction.patch().revision(), base_revision.successor());
        assert_eq!(coordinator.revision(), base_revision.successor());
        let [WorkspaceChange::ConversationMetadataUpsert(metadata_patch)] =
            reduction.patch().changes()
        else {
            panic!("one refresh should produce one metadata patch");
        };
        assert_eq!(metadata_patch.name.as_deref(), Some("renamed"));
        assert_eq!(
            metadata_patch.is_starred, None,
            "a metadata refresh must not carry authoritative star state"
        );

        let store_batch = reduction
            .store_batch()
            .expect("the refresh should produce one atomic store batch");
        assert_eq!(store_batch.revision(), reduction.patch().revision());
        let [StoreChange::ConversationMetadataUpsert(stored_metadata)] = store_batch.changes()
        else {
            panic!("one refresh should produce one metadata store change");
        };
        assert_eq!(stored_metadata, metadata_patch);

        let current = coordinator.conversation("C1").unwrap();
        assert_eq!(current.name.as_deref(), Some("renamed"));
        assert!(current.is_starred());
        assert_eq!(
            current
                .extra
                .get("topic")
                .and_then(serde_json::Value::as_str),
            Some("standup")
        );
    }

    #[test]
    fn conversation_refresh_rejects_blank_metadata_ids_as_one_noop() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![conversation("C1", "one"), conversation("C2", "two")],
            ..Default::default()
        }));
        let base_revision = coordinator.revision();

        let invalid = vec![
            ConversationRefresh {
                metadata: Some(conversation("", "blank metadata id")),
            },
            ConversationRefresh {
                metadata: Some(conversation("   ", "whitespace metadata id")),
            },
            ConversationRefresh { metadata: None },
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
            coordinator.conversation("C2").unwrap().name.as_deref(),
            Some("two")
        );
    }

    #[test]
    fn stale_conversation_refresh_yields_to_newer_state_and_never_resurrects_removals() {
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

        assert!(coordinator
            .apply(WorkspaceMutation::ConversationRefreshBatch(vec![
                SnapshotEnvelope::new(
                    stale_metadata_base,
                    ConversationRefresh {
                        metadata: Some(conversation("C1", "stale details")),
                    },
                ),
            ]))
            .is_none());
        assert_eq!(
            coordinator.conversation("C1").unwrap().name.as_deref(),
            Some("realtime")
        );

        let fresh_base = coordinator.revision();
        let metadata_only = coordinator
            .apply(WorkspaceMutation::ConversationRefreshBatch(vec![
                SnapshotEnvelope::new(
                    fresh_base,
                    ConversationRefresh {
                        metadata: Some(conversation("C1", "fresh details")),
                    },
                ),
            ]))
            .expect("a refresh based on current state must apply");
        assert!(matches!(
            metadata_only.patch().changes(),
            [WorkspaceChange::ConversationMetadataUpsert(conversation)]
                if conversation.name.as_deref() == Some("fresh details")
        ));
        assert!(matches!(
            metadata_only.store_batch().unwrap().changes(),
            [StoreChange::ConversationMetadataUpsert(conversation)]
                if conversation.name.as_deref() == Some("fresh details")
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
                    },
                ),
                SnapshotEnvelope::new(
                    fresh_base,
                    ConversationRefresh {
                        metadata: Some(conversation("C2", "first")),
                    },
                ),
                SnapshotEnvelope::new(
                    fresh_base,
                    ConversationRefresh {
                        metadata: Some(conversation("C2", "duplicate")),
                    },
                ),
            ]))
            .expect("the bounded refresh should commit all accepted items together");

        assert_eq!(coordinator.revision(), fresh_base.successor());
        assert_eq!(reduction.patch().revision(), fresh_base.successor());
        assert!(
            matches!(
                reduction.patch().changes(),
                [WorkspaceChange::ConversationMetadataUpsert(second)]
                    if second.id == "C2" && second.name.as_deref() == Some("first")
            ),
            "the stale item is dropped and the first of two duplicates wins"
        );
        assert_eq!(reduction.store_batch().unwrap().changes().len(), 1);
        assert_eq!(
            coordinator.conversation("C1").unwrap().name.as_deref(),
            Some("realtime")
        );
        assert_eq!(
            coordinator.conversation("C2").unwrap().name.as_deref(),
            Some("first")
        );
    }

    fn counts(channel_id: &str, last_read: &str, latest: &str, mentions: u64) -> ServerReadCounts {
        ServerReadCounts {
            channel_id: channel_id.to_string(),
            last_read: Some(last_read.to_string()),
            latest: Some(latest.to_string()),
            mention_count: mentions,
            unread_count: 0,
            has_unreads: mentions > 0,
        }
    }

    fn posted(
        coordinator: &mut WorkspaceCoordinator,
        channel_id: &str,
        message: SlackMessage,
        kind: MessageMutationKind,
        origin: MutationOrigin,
    ) -> Option<WorkspaceReduction> {
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: channel_id.to_string(),
            message,
            kind,
            origin,
        })
    }

    fn badge(coordinator: &WorkspaceCoordinator, channel_id: &str) -> u64 {
        coordinator
            .read_state(channel_id)
            .map_or(0, ConversationReadState::badge_count)
    }

    #[test]
    fn channel_mentions_badge_and_edits_or_deletes_remove_them() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C1", "general",
        )));
        let reduction = coordinator
            .apply(WorkspaceMutation::CountsSnapshot(vec![counts(
                "C1", "1.0", "1.0", 0,
            )]))
            .expect("snapshot seeds the ledger");
        assert!(reduction
            .store_batch()
            .is_some_and(|batch| matches!(batch.changes(), [StoreChange::ReadStatesUpserted(_)])));

        posted(
            &mut coordinator,
            "C1",
            message("9.0", "hello all"),
            MessageMutationKind::Posted,
            MutationOrigin::Realtime,
        );
        assert_eq!(badge(&coordinator, "C1"), 0);
        assert!(coordinator.read_state("C1").unwrap().has_unreads());

        let reduction = posted(
            &mut coordinator,
            "C1",
            message("10.0", "hey <@U_SELF> check this out"),
            MessageMutationKind::Posted,
            MutationOrigin::Realtime,
        )
        .expect("mention is a change");
        assert!(reduction
            .patch()
            .changes()
            .iter()
            .any(|change| matches!(change, WorkspaceChange::ReadStatesChanged(_))));
        assert_eq!(badge(&coordinator, "C1"), 1);

        posted(
            &mut coordinator,
            "C1",
            message("10.0", "never mind"),
            MessageMutationKind::Changed,
            MutationOrigin::Realtime,
        );
        assert_eq!(badge(&coordinator, "C1"), 0);

        posted(
            &mut coordinator,
            "C1",
            message("11.0", "ping <@U_SELF>"),
            MessageMutationKind::Posted,
            MutationOrigin::Realtime,
        );
        assert_eq!(badge(&coordinator, "C1"), 1);
        posted(
            &mut coordinator,
            "C1",
            message("11.0", "ping <@U_SELF>"),
            MessageMutationKind::Deleted,
            MutationOrigin::Realtime,
        );
        assert_eq!(badge(&coordinator, "C1"), 0);
    }

    #[test]
    fn direct_messages_badge_every_message_from_others_but_not_self() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let mut dm = conversation("D1", "ada");
        dm.is_channel = Some(false);
        dm.is_im = Some(true);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(dm));
        coordinator.apply(WorkspaceMutation::CountsSnapshot(vec![counts(
            "D1", "1.0", "1.0", 0,
        )]));

        posted(
            &mut coordinator,
            "D1",
            message("2.0", "hi"),
            MessageMutationKind::Posted,
            MutationOrigin::Realtime,
        );
        let mut own = message("3.0", "note to self <@U_SELF>");
        own.user = Some("U_SELF".to_string());
        posted(
            &mut coordinator,
            "D1",
            own,
            MessageMutationKind::Posted,
            MutationOrigin::Realtime,
        );
        assert_eq!(badge(&coordinator, "D1"), 1);
    }

    #[test]
    fn backfill_inside_the_baseline_does_not_double_count() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C1", "general",
        )));
        coordinator.apply(WorkspaceMutation::CountsSnapshot(vec![counts(
            "C1", "1.0", "5.0", 1,
        )]));

        posted(
            &mut coordinator,
            "C1",
            message("4.0", "<@U_SELF>"),
            MessageMutationKind::Changed,
            MutationOrigin::WebApi,
        );
        posted(
            &mut coordinator,
            "C1",
            message("4.5", "<@U_SELF>"),
            MessageMutationKind::Posted,
            MutationOrigin::Realtime,
        );
        assert_eq!(badge(&coordinator, "C1"), 1);

        posted(
            &mut coordinator,
            "C1",
            message("6.0", "<@U_SELF>"),
            MessageMutationKind::Posted,
            MutationOrigin::Realtime,
        );
        assert_eq!(badge(&coordinator, "C1"), 2);
    }

    #[test]
    fn marked_events_clear_counts_and_stale_marks_are_noops() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        coordinator.apply(WorkspaceMutation::CountsSnapshot(vec![counts(
            "C1", "1.0", "5.0", 2,
        )]));

        assert!(coordinator
            .apply(WorkspaceMutation::ConversationMarked {
                channel_id: "C1".to_string(),
                ts: "5.0".to_string(),
            })
            .is_some());
        assert_eq!(badge(&coordinator, "C1"), 0);
        assert!(!coordinator.read_state("C1").unwrap().has_unreads());

        let revision = coordinator.revision();
        assert!(coordinator
            .apply(WorkspaceMutation::ConversationMarked {
                channel_id: "C1".to_string(),
                ts: "3.0".to_string(),
            })
            .is_none());
        assert_eq!(coordinator.revision(), revision);
    }

    #[test]
    fn thread_read_mutation_marks_thread_read_and_updates_catalog() {
        let mut coordinator = WorkspaceCoordinator::default();
        let key = ThreadKey::new("C1", "100.0").unwrap();
        let mut record = ThreadRecord::placeholder(key);
        record.reply_count = 2;
        record.latest_reply = Some("120.0".to_string());
        coordinator.apply(WorkspaceMutation::ThreadCatalogChanged(vec![record]));

        let rec = coordinator.thread_catalog.get("C1", "100.0").unwrap();
        assert!(rec.has_unread_replies());
        assert_eq!(rec.unread_reply_count(), 2);

        let reduction = coordinator
            .apply(WorkspaceMutation::ThreadRead {
                channel_id: "C1".to_string(),
                root_ts: "100.0".to_string(),
            })
            .expect("reduction expected");

        assert!(reduction
            .patch()
            .changes()
            .iter()
            .any(|c| matches!(c, WorkspaceChange::ThreadCatalogChanged(_))));

        let rec = coordinator.thread_catalog.get("C1", "100.0").unwrap();
        assert!(!rec.has_unread_replies());
        assert_eq!(rec.unread_reply_count(), 0);
        assert_eq!(rec.last_read.as_deref(), Some("120.0"));
    }

    #[test]
    fn mark_unread_moves_back_and_derives_badges_from_loaded_history() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![conversation("C1", "general")],
            histories: HashMap::from([(
                "C1".to_string(),
                vec![
                    message("2.0", "<@U_SELF> old"),
                    message("4.0", "plain"),
                    message("5.0", "<@U_SELF> new"),
                ],
            )]),
            ..Default::default()
        }));
        coordinator.apply(WorkspaceMutation::CountsSnapshot(vec![counts(
            "C1", "9.0", "9.0", 0,
        )]));

        coordinator.apply(WorkspaceMutation::ConversationMarkedUnread {
            channel_id: "C1".to_string(),
            ts: "3.0".to_string(),
        });
        let state = coordinator.read_state("C1").unwrap();
        assert_eq!(state.last_read.as_deref(), Some("3.0"));
        assert!(state.has_unreads());
        assert_eq!(state.badge_count(), 1);
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
    fn coordinator_classifies_realtime_messages_and_notification_effects_fan_out() {
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
        assert!(effect.decision.send_notification);
        assert_eq!(effect.delivery, DeliveryState::Fresh);
        assert_eq!(effect.channel_id, "D1");
        assert_eq!(effect.message.ts, "10.0");
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
        assert!(
            !attention_effect(&disabled_direct)
                .decision
                .send_notification
        );

        let keyword = classify(&mut coordinator, "12.0", "please page me now");
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
        assert!(!attention_effect(&lifecycle).decision.send_notification);
        assert!(attention_effect(&lifecycle)
            .decision
            .reasons
            .contains(&crate::attention::AttentionReason::MembershipLifecycle));
    }

    #[test]
    fn attention_preview_is_pure_and_delivery_override_suppresses_rejected_attention() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(conversation(
            "C1", "general",
        )));
        let mut incoming = message("10.0", "<@U_SELF> are you there?");
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
        assert!(preview.decision.send_notification);
        assert_eq!(preview.delivery, DeliveryState::Fresh);
        assert_eq!(coordinator.revision(), revision);
        assert!(coordinator.history("C1").is_empty());

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
        assert!(
            !effect.decision.send_notification,
            "a duplicate delivery must never notify"
        );
        assert!(effect
            .decision
            .reasons
            .contains(&crate::attention::AttentionReason::DuplicateDelivery));
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
    fn history_and_posted_attention_companions_are_minimal_and_idempotent() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let mut channel = conversation("C1", "general");
        channel.is_starred = Some(true);
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
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        coordinator.revision(),
                        MessagePage {
                            messages: history_messages,
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .unwrap();
        assert!(
            matches!(
                history.patch().changes(),
                [WorkspaceChange::TimelineChanged { .. }]
            ),
            "a history page needs no companion beyond its timeline patch"
        );
        assert_eq!(
            history
                .effects()
                .iter()
                .map(|effect| {
                    let WorkspaceEffect::MessageAttention(effect) = effect;
                    (effect.message.ts.as_str(), effect.delivery)
                })
                .collect::<Vec<_>>(),
            vec![
                ("11.0", DeliveryState::Historical),
                ("12.0", DeliveryState::Historical),
                ("13.0", DeliveryState::Historical),
            ],
            "history fans out one ordered effect per message, none of them notifiable"
        );
        assert!(matches!(
            history.store_batch().unwrap().changes(),
            [StoreChange::HistoryReplaced { .. }]
        ));

        let mut posted_message = message("14.0", "posted <@U_SELF>");
        posted_message.user = Some("U_OTHER".to_string());
        let posted = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: posted_message.clone(),
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        // The only companion is the unread-ledger badge for the mention.
        assert!(matches!(
            posted.patch().changes(),
            [
                WorkspaceChange::TimelineChanged { .. },
                WorkspaceChange::ReadStatesChanged(_)
            ]
        ));
        assert!(matches!(
            posted.store_batch().unwrap().changes(),
            [
                StoreChange::MessageDelta { .. },
                StoreChange::ReadStatesUpserted(_)
            ]
        ));
        let [WorkspaceEffect::MessageAttention(effect)] = posted.effects() else {
            panic!("a realtime post fans out exactly one attention effect");
        };
        assert_eq!(effect.delivery, DeliveryState::Fresh);
        assert!(
            effect.decision.send_notification,
            "a fresh realtime mention still notifies"
        );
        assert!(
            coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "C1".to_string(),
                    message: posted_message,
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Realtime,
                })
                .is_none(),
            "redelivering an identical message is a noop"
        );

        let current = coordinator.conversation("C1").unwrap();
        assert!(current.is_starred());
        assert_eq!(current.name.as_deref(), Some("general"));
        assert_eq!(
            current.extra.get("topic"),
            Some(&serde_json::json!("Keep me"))
        );
    }

    #[test]
    fn cache_history_reuses_the_durable_timeline_without_rewriting_it() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![conversation("C1", "general")],
            ..Default::default()
        }));
        let mut cached = message("11.0", "cached");
        cached.user = Some("U_OTHER".to_string());

        let reduction = coordinator
            .apply_from(
                MutationOrigin::Cache,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        WorkspaceRevision::INITIAL,
                        MessagePage {
                            messages: vec![cached],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .unwrap();

        assert!(matches!(
            reduction.patch().changes(),
            [WorkspaceChange::TimelineChanged { .. }]
        ));
        assert!(
            reduction.store_batch().is_none(),
            "a cache-origin page is already durable and must not be written back"
        );
        let [WorkspaceEffect::MessageAttention(effect)] = reduction.effects() else {
            panic!("a cached message still classifies exactly once");
        };
        assert_eq!(effect.delivery, DeliveryState::Historical);
        assert!(!effect.decision.send_notification);
    }

    #[test]
    fn cache_thread_snapshot_does_not_rewrite_the_durable_thread() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut reply = message("11.0", "cached reply");
        reply.thread_ts = Some("10.0".to_string());

        let reduction = coordinator
            .apply_from(
                MutationOrigin::Cache,
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: "C1".to_string(),
                    thread_ts: "10.0".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        WorkspaceRevision::INITIAL,
                        MessagePage {
                            messages: vec![reply],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .unwrap();

        assert!(matches!(
            reduction.patch().changes(),
            [
                WorkspaceChange::TimelineChanged { .. },
                WorkspaceChange::ThreadCatalogChanged(_),
            ]
        ));
        assert!(
            reduction.store_batch().is_none(),
            "cache-origin threads are already durable and must not emit ThreadReplaced"
        );
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
    fn timeline_snapshots_replace_while_intermediate_pages_use_delta() {
        let mut coordinator = WorkspaceCoordinator::default();
        let history = coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        WorkspaceRevision::INITIAL,
                        MessagePage {
                            messages: vec![message("10.0", "history")],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
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
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::ThreadPage {
                    channel_id: "C1".to_string(),
                    thread_ts: "10.0".to_string(),
                    page: MessagePage {
                        messages: vec![reply],
                        complete: false,
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        let store_changes = thread.store_batch().unwrap().changes();
        assert!(store_changes.iter().any(|change| matches!(
            change,
            StoreChange::ThreadDelta {
                channel_id,
                thread_ts,
                messages,
            } if channel_id == "C1"
                && thread_ts == "10.0"
                && matches!(messages.as_slice(), [message] if message.ts == "11.0")
        )));
        assert!(store_changes
            .iter()
            .any(|change| matches!(change, StoreChange::ThreadRecordsUpserted(_))));
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
        assert!(reduction
            .store_batch()
            .unwrap()
            .changes()
            .iter()
            .any(|change| matches!(
                change,
                StoreChange::MessageDelta {
                    channel_id,
                    message,
                    kind: MessageMutationKind::Posted,
                } if channel_id == "C1" && message.ts == "11.0"
            )));
        assert!(reduction
            .store_batch()
            .unwrap()
            .changes()
            .iter()
            .any(|change| matches!(change, StoreChange::ThreadRecordsUpserted(_))));
        let patch_changes = reduction.patch().changes();
        assert!(matches!(
            &patch_changes[..2],
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
        assert!(matches!(
            patch_changes.last(),
            Some(WorkspaceChange::ThreadCatalogChanged(_))
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
        assert!(reduction
            .store_batch()
            .unwrap()
            .changes()
            .iter()
            .any(|change| matches!(
                change,
                StoreChange::MessageDelta {
                    channel_id,
                    message,
                    kind: MessageMutationKind::Posted,
                } if channel_id == "C1" && message.ts == "12.0"
            )));
        let patch_changes = reduction.patch().changes();
        assert!(matches!(
            &patch_changes[..3],
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
        assert!(matches!(
            patch_changes.last(),
            Some(WorkspaceChange::ThreadCatalogChanged(_))
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
        assert!(moved
            .store_batch()
            .unwrap()
            .changes()
            .iter()
            .any(|change| matches!(
                change,
                StoreChange::MessageDelta {
                    channel_id,
                    message,
                    kind: MessageMutationKind::Changed,
                } if channel_id == "C1"
                    && message.ts == "11.0"
                    && message.thread_ts.as_deref() == Some("20.0")
            )));
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
        let patch_changes = reduction.patch().changes();
        assert!(matches!(
            &patch_changes[..3],
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
        assert!(matches!(
            patch_changes.last(),
            Some(WorkspaceChange::ThreadCatalogChanged(_))
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
        assert!(reduction.patch().changes().iter().any(|change| matches!(
            change,
            WorkspaceChange::TimelineChanged {
                target: TimelineTarget::Thread { .. },
                changes,
            } if matches!(
                changes.as_slice(),
                [MessageChange::Upsert(root)]
                    if root.text.as_deref() == Some("newer thread snapshot")
            )
        )));
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
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::ThreadSnapshot {
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
                },
            )
            .unwrap();
        assert!(reduction
            .store_batch()
            .unwrap()
            .changes()
            .iter()
            .any(|change| matches!(
                change,
                StoreChange::ThreadReplaced { messages, .. }
                    if messages.iter().any(|message| {
                        message.ts == "10.0"
                            && message.text.as_deref() == Some("current root")
                    })
            )));
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
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::ThreadSnapshot {
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
                },
            )
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
        let patch_changes = reduction.patch().changes();
        assert!(matches!(
            &patch_changes[..2],
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
        assert!(matches!(
            patch_changes.last(),
            Some(WorkspaceChange::ThreadCatalogChanged(_))
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
