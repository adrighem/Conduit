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

use serde::{Deserialize, Serialize};

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
    SlackConversationUnreadSnapshot, SlackMessage, SlackReaction, SlackUser,
};
use crate::thread_catalog::{
    ThreadCatalog, ThreadCatalogMessageKind, ThreadKey, ThreadRecord, ThreadUnreadState,
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
    pub(crate) reaction_actor_states: Vec<ReactionMutation>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct WorkspaceStoreProjection {
    pub(crate) conversations: Vec<SlackConversation>,
    pub(crate) users: Vec<SlackUser>,
    pub(crate) histories: HashMap<String, Vec<SlackMessage>>,
    pub(crate) thread_timelines: HashMap<(String, String), Vec<SlackMessage>>,
    pub(crate) thread_catalog: Vec<ThreadRecord>,
    pub(crate) reaction_actor_states: Vec<ReactionMutation>,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AttentionDeliveryIdentity {
    pub(crate) channel_id: String,
    pub(crate) message_ts: String,
}

impl AttentionDeliveryIdentity {
    pub(crate) fn new(channel_id: &str, message_ts: &str) -> Option<Self> {
        let channel_id = channel_id.trim();
        let message_ts = message_ts.trim();
        (!channel_id.is_empty() && !message_ts.is_empty()).then(|| Self {
            channel_id: channel_id.to_string(),
            message_ts: message_ts.to_string(),
        })
    }
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

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ReactionMutation {
    pub(crate) channel_id: String,
    pub(crate) message_ts: String,
    pub(crate) name: String,
    pub(crate) user_id: String,
    pub(crate) added: bool,
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
struct ReactionActorKey {
    reaction: ReactionAuthorityKey,
    user_id: String,
}

impl ReactionActorKey {
    fn from_mutation(change: &ReactionMutation) -> Self {
        Self {
            reaction: ReactionAuthorityKey::from_mutation(change),
            user_id: change.user_id.clone(),
        }
    }

    fn to_mutation(&self, added: bool) -> ReactionMutation {
        ReactionMutation {
            channel_id: self.reaction.channel_id.clone(),
            message_ts: self.reaction.message_ts.clone(),
            name: self.reaction.name.clone(),
            user_id: self.user_id.clone(),
            added,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ReactionActorState {
    added: bool,
    revision: WorkspaceRevision,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ReactionProjectionCount {
    Authoritative(u64),
    Delta(i8),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct ReactionProjectionMutation {
    pub(crate) change: ReactionMutation,
    pub(crate) count: ReactionProjectionCount,
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
struct ReactionAuthorityKey {
    channel_id: String,
    message_ts: String,
    name: String,
}

impl ReactionAuthorityKey {
    fn from_mutation(change: &ReactionMutation) -> Self {
        Self {
            channel_id: change.channel_id.clone(),
            message_ts: change.message_ts.clone(),
            name: change.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ReactionCountTransition {
    revision: WorkspaceRevision,
    delta: i8,
    user_id: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ReactionAuthority {
    snapshot_revision: WorkspaceRevision,
    authoritative_count: Option<u64>,
    count_transitions: Vec<ReactionCountTransition>,
    user_states: HashMap<String, bool>,
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
    ThreadReadAdvanced {
        channel_id: String,
        thread_ts: String,
        ts: String,
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
    ReactionChanged(ReactionMutation),
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

    fn revision_only(revision: WorkspaceRevision) -> Option<Self> {
        (revision > WorkspaceRevision::INITIAL).then_some(Self {
            revision,
            changes: Vec::new(),
        })
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
    WorkspaceRepaired(WorkspaceStoreProjection),
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
    AttentionNotificationClaim {
        identity: AttentionDeliveryIdentity,
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
    ReactionChanged(ReactionProjectionMutation),
    ReactionActorStatesReplaced(Vec<ReactionMutation>),
    ReactionActorStatesRepaired(Vec<ReactionMutation>),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkspaceRepairDisposition {
    SubsumedByProjection,
    /// Replay is permitted only for an idempotent change whose durable result
    /// is independent of its ordering relative to the repaired projection.
    ReplayProjectionIndependent,
}

impl StoreChange {
    fn workspace_repair_disposition(&self) -> WorkspaceRepairDisposition {
        // Keep this exhaustive. A new non-projection side effect must not be
        // acknowledged merely because a cache repair advanced the store gate.
        // The replay arm requires a keyed, idempotent operation whose result is
        // independent of projection ordering and therefore safe across retry.
        match self {
            Self::AttentionNotificationClaim { identity: _ } => {
                WorkspaceRepairDisposition::ReplayProjectionIndependent
            }
            Self::BootstrapReplaced(_)
            | Self::WorkspaceRepaired(_)
            | Self::ConversationsReplaced(_)
            | Self::ConversationsRepaired(_)
            | Self::ConversationUpsert(_)
            | Self::ConversationMetadataUpsert(_)
            | Self::ConversationMembershipUpsert(_)
            | Self::ConversationStarChanged {
                channel_id: _,
                starred: _,
            }
            | Self::ConversationAttentionObserved {
                channel_id: _,
                observations: _,
            }
            | Self::ConversationRemoved { channel_id: _ }
            | Self::UnreadChanged { snapshot: _ }
            | Self::UsersReplaced(_)
            | Self::UserUpsert(_)
            | Self::MessageDelta {
                channel_id: _,
                message: _,
                kind: _,
            }
            | Self::ReactionChanged(_)
            | Self::ReactionActorStatesReplaced(_)
            | Self::ReactionActorStatesRepaired(_)
            | Self::HistoryReplaced {
                channel_id: _,
                messages: _,
            }
            | Self::HistoryRemoved { channel_id: _ }
            | Self::ThreadReplaced {
                channel_id: _,
                thread_ts: _,
                messages: _,
            }
            | Self::ThreadCatalogReplaced(_) => WorkspaceRepairDisposition::SubsumedByProjection,
        }
    }
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

    pub(crate) fn notification_claims(&self) -> Vec<AttentionDeliveryIdentity> {
        // The coordinator emits at most one unique claim for a message
        // reduction. Preserve any duplicate identities supplied by a repair
        // batch so the store can return one keyed outcome per replayed intent;
        // the pending journal coalesces a successful identity onto the first
        // matching reduction in FIFO order.
        self.changes
            .iter()
            .filter_map(|change| match change {
                StoreChange::AttentionNotificationClaim { identity } => Some(identity.clone()),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn workspace_repair_replay_changes(&self) -> Vec<StoreChange> {
        self.changes
            .iter()
            .filter(|change| {
                change.workspace_repair_disposition()
                    == WorkspaceRepairDisposition::ReplayProjectionIndependent
            })
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceAttentionContext {
    pub(crate) current_user_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MessageAttentionEffect {
    pub(crate) channel_id: String,
    pub(crate) message: Box<SlackMessage>,
    pub(crate) decision: AttentionDecision,
    pub(crate) delivery: DeliveryState,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThreadReadEffect {
    pub(crate) channel_id: String,
    pub(crate) thread_ts: String,
    pub(crate) ts: String,
    pub(crate) acknowledged_message_ts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WorkspaceEffect {
    MessageAttention(MessageAttentionEffect),
    ThreadRead(ThreadReadEffect),
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

#[derive(Debug)]
struct ReconciledRootProjection {
    canonical: SlackMessage,
    channel: Option<SlackMessage>,
    thread: Option<SlackMessage>,
}

struct ChannelRootReconciliation<'a> {
    channel_id: &'a str,
    message: &'a SlackMessage,
    kind: MessageMutationKind,
    previous_channel_known: bool,
    previous_reply_locations: &'a [(String, String)],
    identity_was_tombstoned: bool,
    revision: WorkspaceRevision,
}

/// Pure owner of one workspace's canonical domain model and global revision.
///
/// Runtime and GTK adapters are deliberately absent here. Data changes produce one
/// revision-stamped reduction. Accepted causal-only user events can advance the revision without
/// emitting an identical UI patch or store write.
#[derive(Debug)]
pub(crate) struct WorkspaceCoordinator {
    revision: WorkspaceRevision,
    conversations: HashMap<String, RevisionedConversation>,
    conversation_membership_tombstones: HashMap<String, WorkspaceRevision>,
    users: HashMap<String, RevisionedValue<SlackUser>>,
    user_snapshot_revisions: HashMap<String, WorkspaceRevision>,
    user_snapshot_tombstones: HashMap<String, WorkspaceRevision>,
    user_realtime_overlays: HashMap<String, RevisionedValue<SlackUser>>,
    histories: HashMap<String, TimelineState>,
    threads: HashMap<(String, String), TimelineState>,
    message_authority_by_ts: HashMap<(String, String), MessageProjectionAuthority>,
    message_authority_by_client_id: HashMap<(String, String), MessageProjectionAuthority>,
    reaction_authority: HashMap<ReactionAuthorityKey, ReactionAuthority>,
    reaction_actor_states: HashMap<ReactionActorKey, ReactionActorState>,
    thread_catalog: Vec<ThreadRecord>,
    thread_catalog_revisions: HashMap<ThreadKey, WorkspaceRevision>,
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

    pub(crate) fn users(&self) -> Vec<SlackUser> {
        let mut users = self
            .users
            .values()
            .map(|entry| entry.value.clone())
            .collect::<Vec<_>>();
        users.sort_by(|left, right| left.id.cmp(&right.id));
        users
    }

    pub(crate) fn store_projection(&self) -> WorkspaceStoreProjection {
        WorkspaceStoreProjection {
            conversations: self.conversations(),
            users: self.users(),
            histories: self
                .histories
                .iter()
                .map(|(channel_id, timeline)| (channel_id.clone(), timeline.messages()))
                .collect(),
            thread_timelines: self
                .threads
                .iter()
                .map(|((channel_id, thread_ts), timeline)| {
                    ((channel_id.clone(), thread_ts.clone()), timeline.messages())
                })
                .collect(),
            thread_catalog: self.thread_catalog.clone(),
            reaction_actor_states: self.reaction_actor_state_records(),
        }
    }

    pub(crate) fn history(&self, channel_id: &str) -> Vec<SlackMessage> {
        self.histories
            .get(channel_id)
            .map(TimelineState::messages)
            .unwrap_or_default()
    }

    pub(crate) fn reaction_actor_state_records(&self) -> Vec<ReactionMutation> {
        reaction_actor_state_records(&self.reaction_actor_states)
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

    pub(crate) fn thread_with_revisions(
        &self,
        channel_id: &str,
        thread_ts: &str,
    ) -> Vec<(SlackMessage, WorkspaceRevision)> {
        self.threads
            .get(&(channel_id.to_string(), thread_ts.to_string()))
            .map(TimelineState::messages_with_revisions)
            .unwrap_or_default()
    }

    pub(crate) fn thread(&self, channel_id: &str, thread_ts: &str) -> Vec<SlackMessage> {
        self.threads
            .get(&(channel_id.to_string(), thread_ts.to_string()))
            .map(TimelineState::messages)
            .unwrap_or_default()
    }

    pub(crate) fn thread_catalog(&self) -> Vec<ThreadRecord> {
        self.thread_catalog.clone()
    }

    pub(crate) fn message_is_at_or_before_read_cursor(
        &self,
        channel_id: &str,
        message_ts: &str,
    ) -> bool {
        self.conversation(channel_id).is_some_and(|conversation| {
            [conversation.last_read_ts(), conversation.local_read_ts()]
                .into_iter()
                .flatten()
                .any(|last_read| !slack_timestamp_is_after(message_ts, last_read))
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
            WorkspaceMutation::UnreadChanged {
                snapshot,
                base_revision,
            } => self.apply_unread(snapshot, base_revision),
            WorkspaceMutation::ReadAdvanced {
                channel_id,
                ts,
                remaining_unread,
            } => self.apply_read_advanced(&channel_id, &ts, remaining_unread),
            WorkspaceMutation::ThreadReadAdvanced {
                channel_id,
                thread_ts,
                ts,
            } => self.apply_thread_read_advanced(&channel_id, &thread_ts, &ts),
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
                SnapshotEnvelope::new(WorkspaceRevision::INITIAL, page),
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
                SnapshotEnvelope::new(WorkspaceRevision::INITIAL, page),
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
            WorkspaceMutation::ReactionChanged(change) => self.apply_reaction(change),
            WorkspaceMutation::ThreadCatalogChanged(records) => {
                self.apply_thread_catalog(records, origin)
            }
        }
    }

    fn next_revision(&self) -> WorkspaceRevision {
        self.revision.successor()
    }

    fn advance_causal_revision(&mut self) -> WorkspaceRevision {
        let revision = self.next_revision();
        self.revision = revision;
        revision
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
        mut data: WorkspaceBootstrapData,
        origin: MutationOrigin,
    ) -> Option<WorkspaceReduction> {
        let revision = self.next_revision();
        let reaction_actor_states = data
            .reaction_actor_states
            .iter()
            .filter(|state| reaction_mutation_is_valid(state))
            .map(|state| {
                (
                    ReactionActorKey::from_mutation(state),
                    ReactionActorState {
                        added: state.added,
                        revision,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let reaction_actor_states_changed =
            reaction_actor_state_records(&self.reaction_actor_states)
                != reaction_actor_state_records(&reaction_actor_states);
        let reaction_authority =
            hydrated_reaction_authority(&reaction_actor_states, &data, revision);
        for (channel_id, messages) in &mut data.histories {
            for message in messages {
                apply_reaction_authorities_to_message(
                    &reaction_authority,
                    channel_id,
                    message,
                    Some(WorkspaceRevision::INITIAL),
                );
            }
        }
        for record in &mut data.threads {
            if let Some(root) = record.root.as_mut() {
                apply_reaction_authorities_to_message(
                    &reaction_authority,
                    &record.key.channel_id,
                    root,
                    Some(WorkspaceRevision::INITIAL),
                );
            }
        }
        data.reaction_actor_states = reaction_actor_state_records(&reaction_actor_states);
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
            && self.thread_catalog == data.threads
            && !reaction_actor_states_changed;
        if unchanged {
            return None;
        }

        self.reaction_actor_states = reaction_actor_states;
        self.reaction_authority = reaction_authority;
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
        self.conversation_membership_tombstones.clear();
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
        self.user_snapshot_revisions = self
            .users
            .keys()
            .cloned()
            .map(|user_id| (user_id, revision))
            .collect();
        self.user_snapshot_tombstones.clear();
        self.user_realtime_overlays.clear();
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
        self.thread_catalog_revisions = self
            .thread_catalog
            .iter()
            .map(|record| (record.key.clone(), revision))
            .collect();
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
        self.conversation_membership_tombstones
            .remove(&conversation.id);
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
        self.conversation_membership_tombstones
            .insert(channel_id.to_string(), revision);
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
        let membership_authority = base_revision.min(self.revision);
        let mut patch_changes = Vec::new();
        let mut store_changes = Vec::new();

        for (channel_id, conversation) in &incoming {
            match self.conversations.get_mut(channel_id) {
                Some(entry) => {
                    // Presence can be authoritative even when the projected data is identical.
                    // Keep that authority internal and bounded by the global revision.
                    entry.membership_revision = entry.membership_revision.max(membership_authority);
                    if entry.metadata_revision <= base_revision {
                        let mut merged = entry.value.clone();
                        merge_conversation_metadata(&mut merged, conversation);
                        if merged != entry.value {
                            entry.value = merged.clone();
                            entry.metadata_revision = revision;
                            patch_changes.push(WorkspaceChange::ConversationUpsert(merged.clone()));
                            store_changes.push(StoreChange::ConversationMembershipUpsert(merged));
                        }
                    }
                }
                None => {
                    if self
                        .conversation_membership_tombstones
                        .get(channel_id)
                        .is_some_and(|removed_at| *removed_at > base_revision)
                    {
                        continue;
                    }
                    self.conversation_membership_tombstones.remove(channel_id);
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
            self.conversation_membership_tombstones
                .insert(channel_id.clone(), revision);
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
        let conversation = &self.conversations.get(channel_id)?.value;
        if conversation
            .local_read_ts()
            .is_some_and(|current| !slack_timestamp_is_after(ts, current))
            || conversation
                .last_read_ts()
                .is_some_and(|current| !slack_timestamp_is_after(ts, current))
        {
            return None;
        }
        let revision = self.next_revision();
        let entry = self.conversations.get_mut(channel_id).unwrap();
        let before = entry.value.clone();
        entry.value.advance_read_cursor(ts, remaining_unread);
        entry.value.set_local_read_ts(ts);
        if entry.value == before {
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

    fn apply_thread_read_advanced(
        &mut self,
        channel_id: &str,
        thread_ts: &str,
        ts: &str,
    ) -> Option<WorkspaceReduction> {
        if channel_id.trim().is_empty() || thread_ts.trim().is_empty() || ts.trim().is_empty() {
            return None;
        }

        let mut catalog = ThreadCatalog::from_records(self.thread_catalog.clone());
        let acknowledged_message_ts = catalog.mark_read(channel_id, thread_ts, ts);
        let records = catalog.into_records();
        let catalog_changed = records != self.thread_catalog;

        let conversation_changed = if acknowledged_message_ts.is_empty() {
            false
        } else {
            self.conversations.get_mut(channel_id).is_some_and(|entry| {
                entry
                    .value
                    .acknowledge_attention_messages(&acknowledged_message_ts)
                    > 0
            })
        };
        if !catalog_changed && !conversation_changed {
            return None;
        }

        let revision = self.next_revision();
        let mut patch_changes = Vec::new();
        let mut store_changes = Vec::new();
        if conversation_changed {
            let entry = self.conversations.get_mut(channel_id).unwrap();
            entry.unread_revision = revision;
            let conversation = entry.value.clone();
            patch_changes.push(WorkspaceChange::ConversationUpsert(conversation.clone()));
            store_changes.push(StoreChange::ConversationUpsert(conversation));
        }
        if catalog_changed {
            let previous_records = self.thread_catalog.clone();
            self.record_thread_catalog_revision_changes(&previous_records, &records, revision);
            self.thread_catalog = records.clone();
            let record = records.iter().find(|record| {
                record.key.channel_id == channel_id && record.key.root_ts == thread_ts
            });
            let mut projection_store_changes = Vec::new();
            if let Some(record) = record {
                if let Some(timeline) = self.histories.get_mut(channel_id) {
                    if let Some(root) =
                        update_thread_root_projection(timeline, thread_ts, record, revision)
                    {
                        patch_changes.push(WorkspaceChange::TimelineChanged {
                            target: TimelineTarget::Channel(channel_id.to_string()),
                            changes: vec![MessageChange::Upsert(Box::new(root))],
                        });
                        projection_store_changes.push(StoreChange::HistoryReplaced {
                            channel_id: channel_id.to_string(),
                            messages: timeline.messages(),
                        });
                    }
                }
                if let Some(timeline) = self
                    .threads
                    .get_mut(&(channel_id.to_string(), thread_ts.to_string()))
                {
                    if let Some(root) =
                        update_thread_root_projection(timeline, thread_ts, record, revision)
                    {
                        patch_changes.push(WorkspaceChange::TimelineChanged {
                            target: TimelineTarget::Thread {
                                channel_id: channel_id.to_string(),
                                thread_ts: thread_ts.to_string(),
                            },
                            changes: vec![MessageChange::Upsert(Box::new(root))],
                        });
                        projection_store_changes.push(StoreChange::ThreadReplaced {
                            channel_id: channel_id.to_string(),
                            thread_ts: thread_ts.to_string(),
                            messages: timeline.messages(),
                        });
                    }
                }
            }
            patch_changes.push(WorkspaceChange::ThreadCatalogChanged(records.clone()));
            projection_store_changes.push(StoreChange::ThreadCatalogReplaced(records));
            projection_store_changes.append(&mut store_changes);
            store_changes = projection_store_changes;
        }

        self.commit_with_effects(
            revision,
            patch_changes,
            store_changes,
            vec![WorkspaceEffect::ThreadRead(ThreadReadEffect {
                channel_id: channel_id.to_string(),
                thread_ts: thread_ts.to_string(),
                ts: ts.to_string(),
                acknowledged_message_ts,
            })],
        )
    }

    fn apply_users_snapshot(
        &mut self,
        snapshot: SnapshotEnvelope<Vec<SlackUser>>,
    ) -> Option<WorkspaceReduction> {
        let base_revision = snapshot.base_revision();
        let revision = self.next_revision();
        let mut incoming = HashMap::new();
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
            incoming.insert(user_id, user);
        }

        let mut data_changed = false;
        let mut first_authoritative_adoption = false;
        let mut accepted_present = Vec::new();
        for (user_id, user) in &incoming {
            if self
                .user_snapshot_revisions
                .get(user_id)
                .is_some_and(|authority| *authority > base_revision)
                || self
                    .user_snapshot_tombstones
                    .get(user_id)
                    .is_some_and(|removed_at| *removed_at > base_revision)
            {
                continue;
            }
            first_authoritative_adoption |= !self.user_snapshot_revisions.contains_key(user_id);
            accepted_present.push(user_id.clone());
            let next_user = self
                .user_realtime_overlays
                .get(user_id)
                .filter(|overlay| overlay.revision > base_revision)
                .map(|overlay| user.clone().merge_sparse_update(overlay.value.clone()))
                .unwrap_or_else(|| user.clone());
            if self
                .users
                .get(user_id)
                .is_none_or(|entry| entry.value != next_user)
            {
                self.users.insert(
                    user_id.clone(),
                    RevisionedValue {
                        value: next_user,
                        revision,
                    },
                );
                data_changed = true;
            }
        }
        let removed = self
            .users
            .iter()
            .filter(|(user_id, _entry)| {
                !incoming.contains_key(*user_id)
                    && self
                        .user_snapshot_revisions
                        .get(*user_id)
                        .is_none_or(|authority| *authority <= base_revision)
                    && self
                        .user_snapshot_tombstones
                        .get(*user_id)
                        .is_none_or(|removed_at| *removed_at <= base_revision)
                    && self
                        .user_realtime_overlays
                        .get(*user_id)
                        .is_none_or(|overlay| overlay.revision <= base_revision)
            })
            .map(|(user_id, _)| user_id.clone())
            .collect::<Vec<_>>();
        for user_id in &removed {
            self.users.remove(user_id);
            self.user_snapshot_revisions.remove(user_id);
            data_changed = true;
        }
        self.user_realtime_overlays
            .retain(|_, overlay| overlay.revision > base_revision);

        let confirmed_tombstones = self
            .user_snapshot_tombstones
            .keys()
            .filter(|user_id| {
                !incoming.contains_key(*user_id)
                    && !self.users.contains_key(*user_id)
                    && self
                        .user_snapshot_tombstones
                        .get(*user_id)
                        .is_some_and(|removed_at| *removed_at <= base_revision)
            })
            .cloned()
            .collect::<Vec<_>>();

        // A realtime user can make the projection identical before the first bulk directory
        // arrives, but that first authoritative adoption still has to replace the durable list.
        let changed = data_changed || first_authoritative_adoption;
        let accepted_any =
            !accepted_present.is_empty() || !confirmed_tombstones.is_empty() || !removed.is_empty();
        let accepted_revision = if changed {
            revision
        } else if accepted_any {
            self.advance_causal_revision()
        } else {
            self.revision
        };
        for user_id in accepted_present {
            self.user_snapshot_revisions
                .insert(user_id.clone(), accepted_revision);
            self.user_snapshot_tombstones.remove(&user_id);
        }
        for user_id in confirmed_tombstones {
            self.user_snapshot_tombstones
                .insert(user_id, accepted_revision);
        }
        for user_id in removed {
            self.user_snapshot_tombstones
                .insert(user_id, accepted_revision);
        }

        if !changed {
            return None;
        }
        let mut users = self
            .users
            .values()
            .map(|entry| entry.value.clone())
            .collect::<Vec<_>>();
        users.sort_by(|left, right| left.id.cmp(&right.id));
        self.commit(
            revision,
            vec![WorkspaceChange::UsersReset(users.clone())],
            vec![StoreChange::UsersReplaced(users)],
        )
    }

    fn apply_user_upsert(&mut self, user: SlackUser) -> Option<WorkspaceReduction> {
        let user_id = user
            .id
            .as_deref()
            .map(str::trim)
            .filter(|user_id| !user_id.is_empty())?
            .to_string();
        let mut user = user;
        user.id = Some(user_id.clone());
        let overlay = self
            .user_realtime_overlays
            .get(&user_id)
            .map(|entry| entry.value.clone().merge_sparse_update(user.clone()))
            .unwrap_or_else(|| user.clone());
        let user = self
            .users
            .get(&user_id)
            .map(|entry| entry.value.clone().merge_sparse_update(user.clone()))
            .unwrap_or(user);
        let user_changed = self
            .users
            .get(&user_id)
            .is_none_or(|entry| entry.value != user);
        if !user_changed {
            let revision = self.advance_causal_revision();
            self.user_realtime_overlays.insert(
                user_id,
                RevisionedValue {
                    value: overlay,
                    revision,
                },
            );
            return None;
        }
        let revision = self.next_revision();
        self.user_realtime_overlays.insert(
            user_id.clone(),
            RevisionedValue {
                value: overlay,
                revision,
            },
        );
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
        authoritative_complete_thread: bool,
    ) -> bool {
        let channel_id = match target {
            TimelineTarget::Channel(channel_id) => channel_id,
            TimelineTarget::Thread { channel_id, .. } => channel_id,
        };
        if self.deleted_reply_authority_supersedes(
            target,
            message,
            base_revision,
            authoritative_complete_thread,
        ) || self.newer_reply_projection_supersedes(target, channel_id, message, base_revision)
        {
            return true;
        }
        self.newer_message_projection_authority(channel_id, message, base_revision)
            .is_some_and(|authority| {
                authority.current_ts != message.ts || !authority.retained_targets.contains(target)
            })
    }

    fn newer_message_projection_authority<'a>(
        &'a self,
        channel_id: &str,
        message: &SlackMessage,
        base_revision: WorkspaceRevision,
    ) -> Option<&'a MessageProjectionAuthority> {
        let timestamp_key = (channel_id.to_string(), message.ts.clone());
        let client_key = message
            .client_msg_id
            .as_deref()
            .filter(|client_id| !client_id.trim().is_empty())
            .map(|client_id| (channel_id.to_string(), client_id.to_string()));
        [
            self.message_authority_by_ts.get(&timestamp_key),
            client_key
                .as_ref()
                .and_then(|key| self.message_authority_by_client_id.get(key)),
        ]
        .into_iter()
        .flatten()
        .filter(|authority| authority.revision > base_revision)
        .max_by_key(|authority| authority.revision)
    }

    fn catalog_snapshot_message_is_superseded(
        &self,
        target: &TimelineTarget,
        message: &SlackMessage,
        base_revision: WorkspaceRevision,
        authoritative_complete_thread: bool,
    ) -> bool {
        let channel_id = match target {
            TimelineTarget::Channel(channel_id) | TimelineTarget::Thread { channel_id, .. } => {
                channel_id
            }
        };
        if self.deleted_reply_authority_supersedes(
            target,
            message,
            base_revision,
            authoritative_complete_thread,
        ) || self.newer_reply_projection_supersedes(target, channel_id, message, base_revision)
            || self
                .newer_message_projection_authority(channel_id, message, base_revision)
                .is_some()
        {
            return true;
        }
        message_belongs_in_target(message, target)
            && self.timeline(target).is_some_and(|timeline| {
                timeline
                    .tombstones
                    .get(&message.ts)
                    .is_some_and(|revision| *revision > base_revision)
                    || timeline
                        .messages
                        .get(&message.ts)
                        .is_some_and(|entry| entry.revision > base_revision)
            })
    }

    fn newer_reply_projection_supersedes(
        &self,
        target: &TimelineTarget,
        channel_id: &str,
        message: &SlackMessage,
        base_revision: WorkspaceRevision,
    ) -> bool {
        if message.thread_root_ts().is_none() {
            return false;
        }
        self.histories
            .get(channel_id)
            .into_iter()
            .chain(
                self.threads
                    .iter()
                    .filter(|((known_channel_id, _), _)| known_channel_id == channel_id)
                    .map(|(_, timeline)| timeline),
            )
            .any(|timeline| {
                timeline
                    .tombstones
                    .get(&message.ts)
                    .is_some_and(|revision| *revision > base_revision)
                    || timeline.messages.values().any(|entry| {
                        entry.revision > base_revision
                            && same_message_identity(&entry.value, message)
                            && (entry.value.thread_root_ts() != message.thread_root_ts()
                                || message_belongs_in_target(&entry.value, target)
                                    != message_belongs_in_target(message, target)
                                || !messages_match_except_reactions(&entry.value, message))
                    })
            })
    }

    fn deleted_reply_authority_supersedes(
        &self,
        target: &TimelineTarget,
        message: &SlackMessage,
        base_revision: WorkspaceRevision,
        authoritative_complete_thread: bool,
    ) -> bool {
        let Some(root_ts) = message.thread_root_ts() else {
            return false;
        };
        let channel_id = match target {
            TimelineTarget::Channel(channel_id) | TimelineTarget::Thread { channel_id, .. } => {
                channel_id
            }
        };
        let identity_timestamps = HashSet::from([message.ts.clone()]);
        let Some(record) = self
            .thread_catalog
            .iter()
            .find(|record| record.key.channel_id == *channel_id && record.key.root_ts == root_ts)
        else {
            return false;
        };
        if !record.has_deleted_reply_identity(&identity_timestamps) {
            return false;
        }
        let complete_snapshot_can_replace = authoritative_complete_thread
            && matches!(
                target,
                TimelineTarget::Thread { thread_ts, .. } if thread_ts == root_ts
            )
            && self
                .thread_catalog_revisions
                .get(&record.key)
                .is_none_or(|revision| *revision <= base_revision);
        !complete_snapshot_can_replace
    }

    fn message_identity_is_tombstoned(&self, channel_id: &str, message: &SlackMessage) -> bool {
        self.histories
            .get(channel_id)
            .is_some_and(|timeline| timeline.tombstones.contains_key(&message.ts))
            || self
                .threads
                .iter()
                .filter(|((known_channel_id, _), _)| known_channel_id == channel_id)
                .any(|(_, timeline)| timeline.tombstones.contains_key(&message.ts))
            || message.thread_root_ts().is_some_and(|root_ts| {
                let identity_timestamps = HashSet::from([message.ts.clone()]);
                self.thread_catalog.iter().any(|record| {
                    record.key.channel_id == channel_id
                        && record.key.root_ts == root_ts
                        && record.has_deleted_reply_identity(&identity_timestamps)
                })
            })
    }

    fn overlay_newer_thread_root_authority(
        &self,
        channel_id: &str,
        mut message: SlackMessage,
        base_revision: WorkspaceRevision,
    ) -> SlackMessage {
        if message.thread_root_ts().is_some() {
            return message;
        }
        let Some(record) = self
            .thread_catalog
            .iter()
            .find(|record| record.key.channel_id == channel_id && record.key.root_ts == message.ts)
        else {
            return message;
        };
        if !self
            .thread_catalog_revisions
            .get(&record.key)
            .is_some_and(|revision| *revision > base_revision)
            && !record.has_deleted_replies()
        {
            return message;
        }
        overlay_thread_record_metadata(&mut message, record);
        message
    }

    fn record_thread_catalog_revision_changes(
        &mut self,
        previous: &[ThreadRecord],
        next: &[ThreadRecord],
        revision: WorkspaceRevision,
    ) {
        let previous = previous
            .iter()
            .map(|record| (&record.key, record))
            .collect::<HashMap<_, _>>();
        let retained_keys = next
            .iter()
            .map(|record| record.key.clone())
            .collect::<HashSet<_>>();
        self.thread_catalog_revisions
            .retain(|key, _| retained_keys.contains(key));
        for record in next {
            if previous
                .get(&record.key)
                .is_none_or(|previous| **previous != *record)
            {
                self.thread_catalog_revisions
                    .insert(record.key.clone(), revision);
            }
        }
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
        let authoritative_complete_thread = page_complete
            && origin != MutationOrigin::Cache
            && matches!(&target, TimelineTarget::Thread { .. });
        let revision = self.next_revision();
        let channel_id = match &target {
            TimelineTarget::Channel(channel_id) => channel_id.clone(),
            TimelineTarget::Thread { channel_id, .. } => channel_id.clone(),
        };
        let page_messages = page
            .messages
            .into_iter()
            .map(|message| {
                self.overlay_newer_thread_root_authority(&channel_id, message, base_revision)
            })
            .collect::<Vec<_>>();
        let catalog_messages = page_messages
            .iter()
            .filter(|message| {
                !self.catalog_snapshot_message_is_superseded(
                    &target,
                    message,
                    base_revision,
                    authoritative_complete_thread,
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut incoming = page_messages
            .into_iter()
            .filter(|message| match &target {
                TimelineTarget::Channel(_) => message.belongs_in_channel_timeline(),
                TimelineTarget::Thread { thread_ts, .. } => message.belongs_to_thread(thread_ts),
            })
            .filter(|message| {
                !self.message_projection_is_superseded(
                    &target,
                    message,
                    base_revision,
                    authoritative_complete_thread,
                )
            })
            .map(|message| (message.ts.clone(), message))
            .collect::<HashMap<_, _>>();
        let accepted_message_ts = {
            let timeline = self.timeline(&target);
            incoming
                .iter()
                .filter(|(message_ts, _)| {
                    timeline.is_none_or(|timeline| {
                        !timeline
                            .tombstones
                            .get(*message_ts)
                            .is_some_and(|deleted_at| *deleted_at > base_revision)
                            && !timeline
                                .messages
                                .get(*message_ts)
                                .is_some_and(|entry| entry.revision > base_revision)
                    })
                })
                .map(|(message_ts, _)| message_ts.clone())
                .collect::<Vec<_>>()
        };
        let mut reaction_authority_changed = false;
        for message_ts in &accepted_message_ts {
            let message = incoming
                .get_mut(message_ts)
                .expect("accepted incoming message must remain available");
            reaction_authority_changed |= self.reconcile_reaction_authority_from_snapshot_message(
                &channel_id,
                message,
                base_revision,
                revision,
            );
        }
        let timeline = self.timeline_mut(&target);
        let mut changes = Vec::new();
        let mut accepted_messages = Vec::new();
        for message_ts in accepted_message_ts {
            let message = incoming
                .get(&message_ts)
                .expect("accepted incoming message must remain available");
            if timeline
                .messages
                .get(&message_ts)
                .is_none_or(|entry| entry.value != *message)
            {
                timeline.messages.insert(
                    message_ts.clone(),
                    RevisionedValue {
                        value: message.clone(),
                        revision,
                    },
                );
                timeline.tombstones.remove(&message_ts);
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
        accepted_messages.sort_by(|left, right| left.ts.cmp(&right.ts));
        let mut messages = timeline.messages();
        let previous_thread_catalog = self.thread_catalog.clone();
        let mut thread_catalog = ThreadCatalog::from_records(previous_thread_catalog.clone());
        match &target {
            TimelineTarget::Channel(channel_id) => {
                thread_catalog.observe_history(channel_id, &catalog_messages);
            }
            TimelineTarget::Thread {
                channel_id,
                thread_ts,
            } if authoritative_complete_thread => {
                thread_catalog.reconcile_complete_thread(channel_id, thread_ts, &messages);
            }
            TimelineTarget::Thread {
                channel_id,
                thread_ts,
            } => {
                thread_catalog.observe_thread(channel_id, thread_ts, &messages, false);
            }
        }
        let mut next_thread_catalog = thread_catalog.into_records();
        self.overlay_reaction_authority_onto_thread_records(
            &mut next_thread_catalog,
            base_revision,
        );
        let thread_catalog_changed = next_thread_catalog != previous_thread_catalog;
        if thread_catalog_changed {
            self.record_thread_catalog_revision_changes(
                &previous_thread_catalog,
                &next_thread_catalog,
                revision,
            );
            self.thread_catalog = next_thread_catalog.clone();
        }
        let mut additional_root_changes = Vec::new();
        if let TimelineTarget::Thread {
            channel_id,
            thread_ts,
        } = &target
        {
            if authoritative_complete_thread {
                if let Some(record) = next_thread_catalog.iter().find(|record| {
                    record.key.channel_id == *channel_id && record.key.root_ts == *thread_ts
                }) {
                    if let Some(thread_root) = self
                        .threads
                        .get_mut(&(channel_id.clone(), thread_ts.clone()))
                        .and_then(|timeline| timeline.messages.get_mut(thread_ts))
                    {
                        let before = thread_root.value.clone();
                        overlay_thread_record_metadata(&mut thread_root.value, record);
                        if thread_root.value != before {
                            thread_root.revision = revision;
                            upsert_message_change(&mut changes, thread_root.value.clone());
                        }
                    }
                    if let Some(channel_root) = self
                        .histories
                        .get_mut(channel_id)
                        .and_then(|timeline| timeline.messages.get_mut(thread_ts))
                    {
                        let before = channel_root.value.clone();
                        overlay_thread_record_metadata(&mut channel_root.value, record);
                        if channel_root.value != before {
                            channel_root.revision = revision;
                            additional_root_changes.push((
                                TimelineTarget::Channel(channel_id.clone()),
                                channel_root.value.clone(),
                            ));
                        }
                    }
                    messages = self
                        .threads
                        .get(&(channel_id.clone(), thread_ts.clone()))
                        .map(TimelineState::messages)
                        .unwrap_or_default();
                }
            }
        }
        if changes.is_empty()
            && additional_root_changes.is_empty()
            && !thread_catalog_changed
            && !reaction_authority_changed
        {
            return None;
        }
        let store_change = (!changes.is_empty() && origin != MutationOrigin::Cache)
            .then(|| store_timeline_replacement(&target, messages.clone()));
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
        let mut patch_changes = Vec::new();
        if !changes.is_empty() {
            patch_changes.push(WorkspaceChange::TimelineChanged {
                target: target.clone(),
                changes,
            });
        }
        for (target, root) in &additional_root_changes {
            patch_changes.push(WorkspaceChange::TimelineChanged {
                target: target.clone(),
                changes: vec![MessageChange::Upsert(Box::new(root.clone()))],
            });
        }
        if thread_catalog_changed {
            patch_changes.push(WorkspaceChange::ThreadCatalogChanged(
                next_thread_catalog.clone(),
            ));
        }
        let mut store_changes = store_change.into_iter().collect::<Vec<_>>();
        if thread_catalog_changed && origin != MutationOrigin::Cache {
            store_changes.push(StoreChange::ThreadCatalogReplaced(next_thread_catalog));
        }
        if reaction_authority_changed && origin != MutationOrigin::Cache {
            store_changes.push(StoreChange::ReactionActorStatesReplaced(
                self.reaction_actor_state_records(),
            ));
        }
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
        if kind != MessageMutationKind::Deleted {
            self.overlay_reaction_authority_onto_message(
                channel_id,
                &mut message,
                WorkspaceRevision::INITIAL,
            );
        }
        let identity_timestamps = HashSet::from([message.ts.clone()]);
        let catalog_identity_is_live = self
            .thread_catalog
            .iter()
            .filter(|record| record.key.channel_id == channel_id)
            .any(|record| {
                record
                    .reply_timestamp_for_identity(&identity_timestamps)
                    .is_some()
            });
        let catalog_identity_was_deleted = self
            .thread_catalog
            .iter()
            .filter(|record| record.key.channel_id == channel_id)
            .any(|record| record.has_deleted_reply_identity(&identity_timestamps));
        if message.thread_root_ts().is_some()
            && !catalog_identity_is_live
            && catalog_identity_was_deleted
        {
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
        let identity_was_tombstoned = self.message_identity_is_tombstoned(channel_id, &message);
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
        let mut previous_identity_messages = previous_channel_message
            .iter()
            .chain(previous_thread_root_message.iter())
            .chain(previous_catalog_root_message.iter())
            .cloned()
            .collect::<Vec<_>>();
        previous_identity_messages.extend(
            previous_replies
                .iter()
                .map(|(_, previous)| previous.clone()),
        );
        if kind == MessageMutationKind::Deleted && !identity_was_tombstoned {
            previous_identity_messages.push(message.clone());
        }
        let identity_timestamps = previous_identity_messages
            .iter()
            .map(|message| message.ts.clone())
            .chain(std::iter::once(message.ts.clone()))
            .collect::<HashSet<_>>();
        let mut previous_reply_locations = previous_replies
            .iter()
            .map(|(root_ts, previous)| (root_ts.clone(), previous.ts.clone()))
            .collect::<Vec<_>>();
        if let Some(previous) = previous_channel_message.as_ref() {
            if let Some(root_ts) = previous.thread_root_ts() {
                previous_reply_locations.push((root_ts.to_string(), previous.ts.clone()));
            }
        }
        previous_reply_locations.extend(
            self.thread_catalog
                .iter()
                .filter(|record| record.key.channel_id == channel_id)
                .filter_map(|record| {
                    record
                        .reply_timestamp_for_identity(&identity_timestamps)
                        .map(|message_ts| (record.key.root_ts.clone(), message_ts.to_string()))
                }),
        );
        previous_reply_locations.sort();
        previous_reply_locations.dedup();
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

        let mut changed_roots =
            self.reconcile_channel_roots_for_message(ChannelRootReconciliation {
                channel_id,
                message: &message,
                kind,
                previous_channel_known,
                previous_reply_locations: &previous_reply_locations,
                identity_was_tombstoned,
                revision,
            });

        let previous_thread_catalog = self.thread_catalog.clone();
        let mut thread_catalog = ThreadCatalog::from_records(previous_thread_catalog.clone());
        thread_catalog.reconcile_message(
            channel_id,
            &message,
            &previous_identity_messages,
            match kind {
                MessageMutationKind::Posted => ThreadCatalogMessageKind::Posted,
                MessageMutationKind::Changed => ThreadCatalogMessageKind::Changed,
                MessageMutationKind::Deleted => ThreadCatalogMessageKind::Deleted,
            },
            self.attention_context.current_user_id.as_deref(),
        );
        for changed_root in &changed_roots {
            thread_catalog.replace_root_projection_after_reply(channel_id, &changed_root.canonical);
        }
        let mut next_thread_catalog = thread_catalog.into_records();
        self.overlay_reaction_authority_onto_thread_records(
            &mut next_thread_catalog,
            WorkspaceRevision::INITIAL,
        );
        let thread_catalog_changed = next_thread_catalog != previous_thread_catalog;
        if thread_catalog_changed {
            self.record_thread_catalog_revision_changes(
                &previous_thread_catalog,
                &next_thread_catalog,
                revision,
            );
            self.thread_catalog = next_thread_catalog.clone();
        }
        for changed_root in &mut changed_roots {
            let Some(record) = next_thread_catalog.iter().find(|record| {
                record.key.channel_id == channel_id
                    && record.key.root_ts == changed_root.canonical.ts
            }) else {
                continue;
            };
            overlay_thread_record_metadata(&mut changed_root.canonical, record);
            if let Some(root) = self
                .histories
                .get_mut(channel_id)
                .and_then(|timeline| timeline.messages.get_mut(&record.key.root_ts))
            {
                let before = root.value.clone();
                overlay_thread_record_metadata(&mut root.value, record);
                if root.value != before || changed_root.channel.is_some() {
                    root.revision = revision;
                    changed_root.channel = Some(root.value.clone());
                }
            }
            if let Some(root) = self
                .threads
                .get_mut(&(channel_id.to_string(), record.key.root_ts.clone()))
                .and_then(|timeline| timeline.messages.get_mut(&record.key.root_ts))
            {
                let before = root.value.clone();
                overlay_thread_record_metadata(&mut root.value, record);
                if root.value != before || changed_root.thread.is_some() {
                    root.revision = revision;
                    changed_root.thread = Some(root.value.clone());
                }
            }
        }
        for changed_root in &changed_roots {
            if let Some(channel_root) = &changed_root.channel {
                patch_changes.push(WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Channel(channel_id.to_string()),
                    changes: vec![MessageChange::Upsert(Box::new(channel_root.clone()))],
                });
            }
            if let Some(thread_root) = &changed_root.thread {
                patch_changes.push(WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread {
                        channel_id: channel_id.to_string(),
                        thread_ts: changed_root.canonical.ts.clone(),
                    },
                    changes: vec![MessageChange::Upsert(Box::new(thread_root.clone()))],
                });
            }
        }
        if thread_catalog_changed {
            patch_changes.push(WorkspaceChange::ThreadCatalogChanged(
                next_thread_catalog.clone(),
            ));
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
        if thread_catalog_changed {
            store_changes.push(StoreChange::ThreadCatalogReplaced(next_thread_catalog));
        }
        if kind == MessageMutationKind::Posted {
            if let Some(effect) = attention_effect.as_ref() {
                let self_authored = effect
                    .decision
                    .reasons
                    .contains(&crate::attention::AttentionReason::SelfAuthored);
                if !self_authored {
                    let at_or_before_read_cursor =
                        self.message_is_at_or_before_read_cursor(channel_id, &effect.message.ts);
                    let observation_accepted = if at_or_before_read_cursor {
                        false
                    } else {
                        let entry = self
                            .conversations
                            .entry(channel_id.to_string())
                            .or_insert_with(|| RevisionedConversation {
                                value: SlackConversation {
                                    id: channel_id.to_string(),
                                    ..Default::default()
                                },
                                // Realtime attention proves unread authority,
                                // not membership or metadata. Keep those empty
                                // domains enrichable by an in-flight snapshot.
                                membership_revision: WorkspaceRevision::INITIAL,
                                metadata_revision: WorkspaceRevision::INITIAL,
                                unread_revision: revision,
                                star_revision: WorkspaceRevision::INITIAL,
                            });
                        if entry.value.observe_attention_message_at(
                            &effect.message.ts,
                            effect.decision.record_unread,
                        ) {
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
                            true
                        } else {
                            false
                        }
                    };
                    if observation_accepted
                        && origin == MutationOrigin::Realtime
                        && effect.decision.send_notification
                    {
                        store_changes.push(StoreChange::AttentionNotificationClaim {
                            identity: AttentionDeliveryIdentity::new(
                                channel_id,
                                &effect.message.ts,
                            )
                            .expect("validated realtime attention identity"),
                        });
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

    fn apply_reaction(&mut self, change: ReactionMutation) -> Option<WorkspaceReduction> {
        if !reaction_mutation_is_valid(&change) {
            return None;
        }

        let key = ReactionAuthorityKey::from_mutation(&change);
        let actor_key = ReactionActorKey::from_mutation(&change);
        if self
            .reaction_actor_states
            .get(&actor_key)
            .is_some_and(|state| state.added == change.added)
        {
            return None;
        }
        let (projection_count, projected_user_present) =
            self.reaction_projection_evidence(&key, &change.user_id);
        let revision = self.next_revision();
        let previous_state = self
            .reaction_actor_states
            .get(&actor_key)
            .map(|state| state.added);
        let count_delta = match (change.added, previous_state, projected_user_present) {
            (true, Some(false), _) | (true, None, false) => 1,
            (false, Some(true), _) | (false, None, _) => -1,
            (true, Some(true), _) | (false, Some(false), _) | (true, None, true) => 0,
        };
        {
            let authority = self
                .reaction_authority
                .entry(key.clone())
                .or_insert_with(|| ReactionAuthority {
                    snapshot_revision: WorkspaceRevision::INITIAL,
                    authoritative_count: None,
                    count_transitions: Vec::new(),
                    user_states: HashMap::new(),
                });
            let count_before = authority.authoritative_count.or_else(|| {
                projection_count.map(|count| {
                    apply_reaction_count_transitions(
                        count,
                        authority
                            .count_transitions
                            .iter()
                            .map(|transition| transition.delta),
                    )
                })
            });
            if let Some(count_before) = count_before {
                authority.authoritative_count =
                    Some(apply_reaction_count_delta(count_before, count_delta));
            }
            if count_delta != 0 {
                authority.count_transitions.push(ReactionCountTransition {
                    revision,
                    delta: count_delta,
                    user_id: change.user_id.clone(),
                });
            }
            authority
                .user_states
                .insert(change.user_id.clone(), change.added);
        }
        self.reaction_actor_states.insert(
            actor_key,
            ReactionActorState {
                added: change.added,
                revision,
            },
        );

        let authority = self
            .reaction_authority
            .get(&key)
            .expect("reaction authority was just installed")
            .clone();
        let mut targets = Vec::new();
        if self
            .histories
            .get(&change.channel_id)
            .is_some_and(|timeline| timeline.messages.contains_key(&change.message_ts))
        {
            targets.push(TimelineTarget::Channel(change.channel_id.clone()));
        }
        let mut thread_targets = self
            .threads
            .iter()
            .filter(|((channel_id, _thread_ts), timeline)| {
                channel_id.as_str() == change.channel_id.as_str()
                    && timeline.messages.contains_key(&change.message_ts)
            })
            .map(
                |((channel_id, thread_ts), _timeline)| TimelineTarget::Thread {
                    channel_id: channel_id.clone(),
                    thread_ts: thread_ts.clone(),
                },
            )
            .collect::<Vec<_>>();
        thread_targets.sort();
        targets.extend(thread_targets);

        let mut patch_changes = Vec::new();
        for target in targets {
            let timeline = self.timeline_mut(&target);
            let Some(entry) = timeline.messages.get_mut(&change.message_ts) else {
                continue;
            };
            if !apply_reaction_authority_to_message(&mut entry.value, &key, &authority, None) {
                continue;
            }
            entry.revision = revision;
            patch_changes.push(WorkspaceChange::TimelineChanged {
                target,
                changes: vec![MessageChange::Upsert(Box::new(entry.value.clone()))],
            });
        }

        let previous_thread_catalog = self.thread_catalog.clone();
        let mut catalog_changed = false;
        for record in self.thread_catalog.iter_mut().filter(|record| {
            record.key.channel_id == change.channel_id
                && record
                    .root
                    .as_ref()
                    .is_some_and(|root| root.ts == change.message_ts)
        }) {
            let Some(root) = record.root.as_mut() else {
                continue;
            };
            catalog_changed |= apply_reaction_authority_to_message(root, &key, &authority, None);
        }
        if catalog_changed {
            let next_thread_catalog = self.thread_catalog.clone();
            self.record_thread_catalog_revision_changes(
                &previous_thread_catalog,
                &next_thread_catalog,
                revision,
            );
            patch_changes.push(WorkspaceChange::ThreadCatalogChanged(next_thread_catalog));
        }

        let mut store_changes = vec![StoreChange::ReactionActorStatesReplaced(
            self.reaction_actor_state_records(),
        )];
        let count = if patch_changes.is_empty() {
            ReactionProjectionCount::Delta(count_delta)
        } else {
            let count = authority
                .authoritative_count
                .expect("a changed reaction projection must have an authoritative count");
            ReactionProjectionCount::Authoritative(count)
        };
        store_changes.push(StoreChange::ReactionChanged(ReactionProjectionMutation {
            change,
            count,
        }));
        self.commit(revision, patch_changes, store_changes)
    }

    fn reaction_projection_evidence(
        &self,
        key: &ReactionAuthorityKey,
        user_id: &str,
    ) -> (Option<u64>, bool) {
        let mut found_projection = false;
        let mut count = 0;
        let mut user_present = false;
        let mut observe = |message: &SlackMessage| {
            if message.ts != key.message_ts {
                return;
            }
            found_projection = true;
            let reaction = message.reactions.as_ref().and_then(|reactions| {
                reactions
                    .iter()
                    .find(|reaction| reaction.name.as_deref() == Some(key.name.as_str()))
            });
            count = count.max(reaction.map_or(0, reaction_authoritative_count));
            user_present |= reaction
                .and_then(|reaction| reaction.users.as_ref())
                .is_some_and(|users| users.iter().any(|user| user == user_id));
        };
        if let Some(timeline) = self.histories.get(&key.channel_id) {
            for entry in timeline.messages.values() {
                observe(&entry.value);
            }
        }
        for ((channel_id, _), timeline) in &self.threads {
            if channel_id != &key.channel_id {
                continue;
            }
            for entry in timeline.messages.values() {
                observe(&entry.value);
            }
        }
        for record in self
            .thread_catalog
            .iter()
            .filter(|record| record.key.channel_id == key.channel_id)
        {
            if let Some(root) = record.root.as_ref() {
                observe(root);
            }
        }
        (found_projection.then_some(count), user_present)
    }

    fn overlay_reaction_authority_onto_message(
        &self,
        channel_id: &str,
        message: &mut SlackMessage,
        base_revision: WorkspaceRevision,
    ) -> bool {
        apply_reaction_authorities_to_message(
            &self.reaction_authority,
            channel_id,
            message,
            Some(base_revision),
        )
    }

    fn reconcile_reaction_authority_from_snapshot_message(
        &mut self,
        channel_id: &str,
        message: &mut SlackMessage,
        base_revision: WorkspaceRevision,
        revision: WorkspaceRevision,
    ) -> bool {
        let keys =
            reaction_authority_keys_for_message(&self.reaction_authority, channel_id, &message.ts);
        let mut changed = false;
        for key in keys {
            let snapshot_is_new = self
                .reaction_authority
                .get(&key)
                .expect("collected reaction authority must remain installed")
                .snapshot_revision
                <= base_revision;
            if !snapshot_is_new {
                let authority = self
                    .reaction_authority
                    .get(&key)
                    .expect("collected reaction authority must remain installed");
                apply_reaction_authority_to_message(message, &key, authority, Some(base_revision));
                continue;
            }

            let incoming_reaction = message.reactions.as_ref().and_then(|reactions| {
                reactions
                    .iter()
                    .find(|reaction| reaction.name.as_deref() == Some(key.name.as_str()))
            });
            let incoming_count = incoming_reaction
                .map(reaction_authoritative_count)
                .unwrap_or_default();
            let incoming_users = incoming_reaction
                .and_then(|reaction| reaction.users.as_ref())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect::<HashSet<_>>();
            let newer_true_actor_floor = self
                .reaction_actor_states
                .iter()
                .filter(|(actor, state)| {
                    actor.reaction == key && state.revision > base_revision && state.added
                })
                .count() as u64;

            let actor_keys = self
                .reaction_actor_states
                .keys()
                .filter(|actor| actor.reaction == key)
                .cloned()
                .collect::<Vec<_>>();
            if incoming_count == 0 {
                for actor_key in actor_keys {
                    if self
                        .reaction_actor_states
                        .get(&actor_key)
                        .is_some_and(|state| state.revision <= base_revision)
                    {
                        self.reaction_actor_states.remove(&actor_key);
                    }
                }
            } else {
                for actor_key in actor_keys {
                    let Some(state) = self.reaction_actor_states.get(&actor_key) else {
                        continue;
                    };
                    if state.revision <= base_revision
                        && incoming_users.contains(&actor_key.user_id)
                    {
                        self.reaction_actor_states.insert(
                            actor_key,
                            ReactionActorState {
                                added: true,
                                revision,
                            },
                        );
                    }
                }
            }
            let user_states = self
                .reaction_actor_states
                .iter()
                .filter(|(actor, _)| actor.reaction == key)
                .map(|(actor, state)| (actor.user_id.clone(), state.added))
                .collect::<HashMap<_, _>>();

            let authority = self
                .reaction_authority
                .get_mut(&key)
                .expect("collected reaction authority must remain installed");
            let before_authority = authority.clone();
            let transition_is_not_reflected = |transition: &ReactionCountTransition| {
                transition.revision > base_revision
                    && !(transition.delta > 0 && incoming_users.contains(&transition.user_id))
            };
            let count = apply_reaction_count_transitions(
                incoming_count,
                authority
                    .count_transitions
                    .iter()
                    .filter(|transition| transition_is_not_reflected(transition))
                    .map(|transition| transition.delta),
            )
            .max(newer_true_actor_floor);
            authority.authoritative_count = Some(count);
            authority
                .count_transitions
                .retain(transition_is_not_reflected);
            authority.user_states = user_states;
            authority.snapshot_revision = revision;
            changed |= *authority != before_authority;
            apply_reaction_authority_to_message(message, &key, authority, None);
        }
        changed
    }

    fn overlay_reaction_authority_onto_thread_records(
        &self,
        records: &mut [ThreadRecord],
        base_revision: WorkspaceRevision,
    ) {
        for record in records {
            let channel_id = record.key.channel_id.clone();
            if let Some(root) = record.root.as_mut() {
                self.overlay_reaction_authority_onto_message(&channel_id, root, base_revision);
            }
        }
    }

    fn message_delivery_state(
        &self,
        channel_id: &str,
        message: &SlackMessage,
        origin: MutationOrigin,
    ) -> DeliveryState {
        if origin == MutationOrigin::Realtime
            && self.message_is_at_or_before_read_cursor(channel_id, &message.ts)
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
                if conversation.is_im.unwrap_or(false) || channel_id.starts_with('D') {
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
            || (origin == MutationOrigin::Realtime && message.user.as_deref() == current_user_id)
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
            message: Box::new(message.clone()),
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
        reconciliation: ChannelRootReconciliation<'_>,
    ) -> Vec<ReconciledRootProjection> {
        let ChannelRootReconciliation {
            channel_id,
            message,
            kind,
            previous_channel_known,
            previous_reply_locations,
            identity_was_tombstoned,
            revision,
        } = reconciliation;
        let incoming_root = message.thread_root_ts().map(str::to_string);
        let mut root_timestamps = if kind == MessageMutationKind::Posted {
            Vec::new()
        } else {
            previous_reply_locations
                .iter()
                .map(|(root_ts, _message_ts)| root_ts.clone())
                .collect::<Vec<_>>()
        };
        if let Some(root_ts) = incoming_root.as_ref() {
            root_timestamps.push(root_ts.clone());
        }
        root_timestamps.sort();
        root_timestamps.dedup();

        let transition_was_known = previous_channel_known || !previous_reply_locations.is_empty();
        let mut changed_roots = Vec::new();
        for root_ts in root_timestamps {
            let previous = if kind == MessageMutationKind::Posted {
                None
            } else {
                previous_reply_locations
                    .iter()
                    .find(|(known_root_ts, _)| known_root_ts == &root_ts)
                    .map(|(_, message_ts)| message_ts.as_str())
            };
            let next = (kind != MessageMutationKind::Deleted
                && incoming_root.as_deref() == Some(root_ts.as_str()))
            .then_some(message);
            let deletion_fallback = (kind == MessageMutationKind::Deleted
                && previous.is_none()
                && previous_reply_locations.is_empty()
                && !identity_was_tombstoned
                && incoming_root.as_deref() == Some(root_ts.as_str()))
            .then_some(message.ts.as_str());
            let old = previous.or(deletion_fallback);
            if old.is_none() && next.is_none() {
                continue;
            }

            let remaining_replies = self
                .threads
                .get(&(channel_id.to_string(), root_ts.clone()))
                .map(TimelineState::messages)
                .unwrap_or_default();
            let identity_timestamps = previous_reply_locations
                .iter()
                .filter(|(known_root_ts, _)| known_root_ts == &root_ts)
                .map(|(_, message_ts)| message_ts.clone())
                .chain(std::iter::once(message.ts.clone()))
                .collect::<HashSet<_>>();
            let catalog_record = self.thread_catalog.iter().find(|record| {
                record.key.channel_id == channel_id && record.key.root_ts == root_ts
            });
            let latest_remaining = remaining_replies
                .iter()
                .filter(|reply| reply.ts != root_ts)
                .filter(|reply| !identity_timestamps.contains(&reply.ts))
                .map(|reply| reply.ts.as_str())
                .max()
                .map(str::to_string);
            let channel_before = self
                .histories
                .get(channel_id)
                .and_then(|timeline| timeline.messages.get(&root_ts))
                .map(|entry| entry.value.clone());
            let thread_before = self
                .threads
                .get(&(channel_id.to_string(), root_ts.clone()))
                .and_then(|timeline| timeline.messages.get(&root_ts))
                .map(|entry| entry.value.clone());
            let Some(mut canonical) = catalog_record
                .and_then(|record| record.root.clone())
                .or_else(|| channel_before.clone())
                .or_else(|| thread_before.clone())
            else {
                continue;
            };
            let aggregate_before = (
                canonical.reply_count,
                canonical.latest_reply.clone(),
                canonical.reply_users.clone(),
            );

            match (old, next) {
                (Some(_old_ts), None) => {
                    canonical.reply_count =
                        Some(canonical.reply_count.unwrap_or_default().saturating_sub(1));
                }
                (None, Some(_next)) => {
                    let addition_is_new = match kind {
                        MessageMutationKind::Posted => true,
                        MessageMutationKind::Changed => transition_was_known,
                        MessageMutationKind::Deleted => false,
                    };
                    if addition_is_new {
                        canonical.reply_count =
                            Some(canonical.reply_count.unwrap_or_default().saturating_add(1));
                    }
                }
                (Some(_), Some(_)) | (None, None) => {}
            }

            if old.is_some_and(|old_ts| canonical.latest_reply.as_deref() == Some(old_ts)) {
                canonical.latest_reply = latest_remaining.or_else(|| {
                    catalog_record
                        .and_then(|record| record.latest_reply_excluding(&identity_timestamps))
                        .map(str::to_string)
                });
            }
            if let Some(next) = next {
                if canonical
                    .latest_reply
                    .as_deref()
                    .is_none_or(|latest| slack_timestamp_is_after(&next.ts, latest))
                {
                    canonical.latest_reply = Some(next.ts.clone());
                }
            }

            let cached_replies = remaining_replies
                .iter()
                .filter(|reply| reply.thread_root_ts() == Some(root_ts.as_str()))
                .collect::<Vec<_>>();
            if canonical.reply_count == Some(0) {
                canonical.reply_users = Some(Vec::new());
            } else if canonical.reply_count == Some(cached_replies.len() as u64) {
                let mut users = Vec::new();
                for user_id in cached_replies
                    .iter()
                    .filter_map(|reply| reply.user.as_ref())
                {
                    if !users.iter().any(|known| known == user_id) {
                        users.push(user_id.clone());
                    }
                }
                canonical.reply_users = Some(users);
            } else if let Some(next_user_id) = next.and_then(|next| next.user.as_deref()) {
                let users = canonical.reply_users.get_or_insert_with(Vec::new);
                if !users.iter().any(|known| known == next_user_id) {
                    users.push(next_user_id.to_string());
                }
            }

            let aggregate_after = (
                canonical.reply_count,
                canonical.latest_reply.clone(),
                canonical.reply_users.clone(),
            );
            let channel = self
                .histories
                .get_mut(channel_id)
                .and_then(|timeline| timeline.messages.get_mut(&root_ts))
                .and_then(|root| {
                    let before = root.value.clone();
                    replace_root_projection_aggregates(&mut root.value, &canonical);
                    if root.value == before {
                        return None;
                    }
                    root.revision = revision;
                    Some(root.value.clone())
                });
            let thread = self
                .threads
                .get_mut(&(channel_id.to_string(), root_ts.clone()))
                .and_then(|timeline| timeline.messages.get_mut(&root_ts))
                .and_then(|thread_root| {
                    let before = thread_root.value.clone();
                    replace_root_projection_aggregates(&mut thread_root.value, &canonical);
                    if thread_root.value == before {
                        return None;
                    }
                    thread_root.revision = revision;
                    Some(thread_root.value.clone())
                });
            if aggregate_before != aggregate_after || channel.is_some() || thread.is_some() {
                changed_roots.push(ReconciledRootProjection {
                    canonical,
                    channel,
                    thread,
                });
            }
        }
        changed_roots
    }

    fn apply_thread_catalog(
        &mut self,
        mut records: Vec<ThreadRecord>,
        origin: MutationOrigin,
    ) -> Option<WorkspaceReduction> {
        self.overlay_reaction_authority_onto_thread_records(
            &mut records,
            WorkspaceRevision::INITIAL,
        );
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
        let previous = self.thread_catalog.clone();
        self.record_thread_catalog_revision_changes(&previous, &records, revision);
        self.thread_catalog = records.clone();
        self.commit(
            revision,
            vec![WorkspaceChange::ThreadCatalogChanged(records.clone())],
            (origin != MutationOrigin::Cache)
                .then_some(StoreChange::ThreadCatalogReplaced(records))
                .into_iter()
                .collect(),
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

fn messages_match_except_reactions(left: &SlackMessage, right: &SlackMessage) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.reactions = None;
    right.reactions = None;
    left == right
}

fn reaction_mutation_is_valid(change: &ReactionMutation) -> bool {
    [
        change.channel_id.as_str(),
        change.message_ts.as_str(),
        change.name.as_str(),
        change.user_id.as_str(),
    ]
    .into_iter()
    .all(|value| !value.trim().is_empty())
}

fn reaction_actor_state_records(
    states: &HashMap<ReactionActorKey, ReactionActorState>,
) -> Vec<ReactionMutation> {
    let mut records = states
        .iter()
        .map(|(key, state)| key.to_mutation(state.added))
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        left.channel_id
            .cmp(&right.channel_id)
            .then_with(|| left.message_ts.cmp(&right.message_ts))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.user_id.cmp(&right.user_id))
    });
    records
}

fn reaction_authority_keys_for_message(
    authorities: &HashMap<ReactionAuthorityKey, ReactionAuthority>,
    channel_id: &str,
    message_ts: &str,
) -> Vec<ReactionAuthorityKey> {
    authorities
        .keys()
        .filter(|key| key.channel_id == channel_id && key.message_ts == message_ts)
        .cloned()
        .collect()
}

fn apply_reaction_authorities_to_message(
    authorities: &HashMap<ReactionAuthorityKey, ReactionAuthority>,
    channel_id: &str,
    message: &mut SlackMessage,
    source_base_revision: Option<WorkspaceRevision>,
) -> bool {
    let keys = reaction_authority_keys_for_message(authorities, channel_id, &message.ts);
    keys.into_iter().fold(false, |changed, key| {
        let authority = authorities
            .get(&key)
            .expect("collected reaction authority must remain installed");
        apply_reaction_authority_to_message(message, &key, authority, source_base_revision)
            || changed
    })
}

fn hydrated_reaction_authority(
    actor_states: &HashMap<ReactionActorKey, ReactionActorState>,
    data: &WorkspaceBootstrapData,
    revision: WorkspaceRevision,
) -> HashMap<ReactionAuthorityKey, ReactionAuthority> {
    let keys = actor_states
        .keys()
        .map(|actor| actor.reaction.clone())
        .collect::<HashSet<_>>();
    keys.into_iter()
        .map(|key| {
            let user_states = actor_states
                .iter()
                .filter(|(actor, _)| actor.reaction == key)
                .map(|(actor, state)| (actor.user_id.clone(), state.added))
                .collect::<HashMap<_, _>>();
            let projection_count = data
                .histories
                .get(&key.channel_id)
                .into_iter()
                .flatten()
                .chain(
                    data.threads
                        .iter()
                        .filter(|record| record.key.channel_id == key.channel_id)
                        .filter_map(|record| record.root.as_ref()),
                )
                .filter(|message| message.ts == key.message_ts)
                .filter_map(|message| {
                    message.reactions.as_ref().and_then(|reactions| {
                        reactions
                            .iter()
                            .find(|reaction| reaction.name.as_deref() == Some(key.name.as_str()))
                            .map(reaction_authoritative_count)
                    })
                })
                .max();
            (
                key,
                ReactionAuthority {
                    snapshot_revision: revision,
                    authoritative_count: projection_count,
                    count_transitions: Vec::new(),
                    user_states,
                },
            )
        })
        .collect()
}

fn reaction_authoritative_count(reaction: &SlackReaction) -> u64 {
    reaction
        .count
        .unwrap_or_else(|| reaction.users.as_ref().map_or(0, Vec::len) as u64)
}

fn apply_reaction_count_delta(count: u64, delta: i8) -> u64 {
    if delta >= 0 {
        count.saturating_add(delta as u64)
    } else {
        count.saturating_sub(delta.unsigned_abs() as u64)
    }
}

fn apply_reaction_count_transitions(count: u64, transitions: impl IntoIterator<Item = i8>) -> u64 {
    transitions
        .into_iter()
        .fold(count, apply_reaction_count_delta)
}

fn apply_reaction_authority_to_message(
    message: &mut SlackMessage,
    key: &ReactionAuthorityKey,
    authority: &ReactionAuthority,
    source_base_revision: Option<WorkspaceRevision>,
) -> bool {
    if message.ts != key.message_ts {
        return false;
    }
    let before = message.reactions.clone();
    let reactions = message.reactions.get_or_insert_with(Vec::new);
    let existing_index = reactions
        .iter()
        .position(|reaction| reaction.name.as_deref() == Some(key.name.as_str()));
    let incoming_count = existing_index
        .map(|index| reaction_authoritative_count(&reactions[index]))
        .unwrap_or_default();
    let count = if let Some(authoritative_count) = authority.authoritative_count {
        authoritative_count
    } else {
        apply_reaction_count_transitions(
            incoming_count,
            authority
                .count_transitions
                .iter()
                .filter(|transition| {
                    source_base_revision.is_none_or(|base| transition.revision > base)
                })
                .map(|transition| transition.delta),
        )
    };

    if count == 0 {
        if let Some(index) = existing_index {
            reactions.remove(index);
        }
    } else {
        let index = existing_index.unwrap_or_else(|| {
            reactions.push(SlackReaction {
                name: Some(key.name.clone()),
                count: Some(count),
                users: None,
            });
            reactions.len() - 1
        });
        let reaction = &mut reactions[index];
        reaction.count = Some(count);
        for (user_id, added) in &authority.user_states {
            if *added {
                let users = reaction.users.get_or_insert_with(Vec::new);
                if !users.iter().any(|user| user == user_id) {
                    users.push(user_id.clone());
                }
            } else if let Some(users) = reaction.users.as_mut() {
                users.retain(|user| user != user_id);
            }
        }
    }
    if message.reactions.as_ref().is_some_and(Vec::is_empty) {
        message.reactions = None;
    }
    message.reactions != before
}

pub(crate) fn apply_reaction_projection_mutation(
    message: &mut SlackMessage,
    projection: &ReactionProjectionMutation,
) -> bool {
    if !reaction_mutation_is_valid(&projection.change) || message.ts != projection.change.message_ts
    {
        return false;
    }
    let key = ReactionAuthorityKey::from_mutation(&projection.change);
    let current_count = message
        .reactions
        .as_ref()
        .and_then(|reactions| {
            reactions
                .iter()
                .find(|reaction| reaction.name.as_deref() == Some(key.name.as_str()))
        })
        .map(reaction_authoritative_count)
        .unwrap_or_default();
    let count = match projection.count {
        ReactionProjectionCount::Authoritative(count) => count,
        ReactionProjectionCount::Delta(delta) => {
            let explicit_actor_is_present = projection.change.added
                && message
                    .reactions
                    .as_ref()
                    .and_then(|reactions| {
                        reactions
                            .iter()
                            .find(|reaction| reaction.name.as_deref() == Some(key.name.as_str()))
                    })
                    .and_then(|reaction| reaction.users.as_ref())
                    .is_some_and(|users| {
                        users.iter().any(|user| user == &projection.change.user_id)
                    });
            let effective_delta = if delta > 0 && explicit_actor_is_present {
                0
            } else {
                delta
            };
            apply_reaction_count_delta(current_count, effective_delta)
        }
    };
    let authority = ReactionAuthority {
        snapshot_revision: WorkspaceRevision::INITIAL,
        authoritative_count: Some(count),
        count_transitions: Vec::new(),
        user_states: HashMap::from([(projection.change.user_id.clone(), projection.change.added)]),
    };
    apply_reaction_authority_to_message(message, &key, &authority, None)
}

pub(crate) fn same_message_identity(left: &SlackMessage, right: &SlackMessage) -> bool {
    (!left.ts.trim().is_empty() && left.ts == right.ts)
        || left.client_msg_id.as_deref().is_some_and(|left_id| {
            !left_id.trim().is_empty() && right.client_msg_id.as_deref() == Some(left_id)
        })
}

fn upsert_message_change(changes: &mut Vec<MessageChange>, message: SlackMessage) {
    if let Some(existing) = changes.iter_mut().find_map(|change| match change {
        MessageChange::Upsert(existing) if existing.ts == message.ts => Some(existing),
        MessageChange::Upsert(_) | MessageChange::Remove { .. } => None,
    }) {
        **existing = message;
    } else {
        changes.push(MessageChange::Upsert(Box::new(message)));
    }
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

fn overlay_thread_record_metadata(message: &mut SlackMessage, record: &ThreadRecord) {
    message.reply_count = Some(record.reply_count);
    message.latest_reply.clone_from(&record.latest_reply);
    if let Some(subscribed) = record.subscribed {
        message.subscribed = Some(subscribed);
    }
    match &record.unread {
        ThreadUnreadState::Known { count, last_read } => {
            message.unread_count = Some(*count);
            message.last_read.clone_from(last_read);
        }
        ThreadUnreadState::Unknown => {}
    }

    if let Some(reply_users) = record
        .root
        .as_ref()
        .and_then(|root| root.reply_users.clone())
    {
        message.reply_users = Some(reply_users);
    } else if record.reply_count == 0 {
        message.reply_users = Some(Vec::new());
    } else {
        let users = message.reply_users.get_or_insert_with(Vec::new);
        let mut participants = record
            .participant_user_ids
            .iter()
            .filter(|user_id| !user_id.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>();
        participants.sort();
        for user_id in participants {
            if !users.iter().any(|known| known == &user_id) {
                users.push(user_id);
            }
        }
    }
}

fn replace_root_projection_aggregates(message: &mut SlackMessage, canonical: &SlackMessage) {
    message.reply_count = canonical.reply_count;
    message.latest_reply.clone_from(&canonical.latest_reply);
    message.reply_users.clone_from(&canonical.reply_users);
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
        let store_batch = StoreBatch::new(revision, store_changes);
        let patch = WorkspacePatch::new(revision, patch_changes).or_else(|| {
            store_batch
                .as_ref()
                .and_then(|_| WorkspacePatch::revision_only(revision))
        })?;
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

fn update_thread_root_projection(
    timeline: &mut TimelineState,
    thread_ts: &str,
    record: &ThreadRecord,
    revision: WorkspaceRevision,
) -> Option<SlackMessage> {
    let root = timeline.messages.get_mut(thread_ts)?;
    let before = root.value.clone();
    overlay_thread_record_metadata(&mut root.value, record);
    if root.value == before {
        return None;
    }
    root.revision = revision;
    Some(root.value.clone())
}

impl Default for WorkspaceCoordinator {
    fn default() -> Self {
        Self {
            revision: WorkspaceRevision::INITIAL,
            conversations: HashMap::new(),
            conversation_membership_tombstones: HashMap::new(),
            users: HashMap::new(),
            user_snapshot_revisions: HashMap::new(),
            user_snapshot_tombstones: HashMap::new(),
            user_realtime_overlays: HashMap::new(),
            histories: HashMap::new(),
            threads: HashMap::new(),
            message_authority_by_ts: HashMap::new(),
            message_authority_by_client_id: HashMap::new(),
            reaction_authority: HashMap::new(),
            reaction_actor_states: HashMap::new(),
            thread_catalog: Vec::new(),
            thread_catalog_revisions: HashMap::new(),
            attention_context: WorkspaceAttentionContext::default(),
            attention_preferences: AttentionPreferences::default(),
            attention_policy: AttentionPolicy::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_projection_subsumption_patterns_name_every_field() {
        let source = include_str!("workspace_pipeline.rs");
        let classifier = source
            .split_once("enum WorkspaceRepairDisposition")
            .unwrap()
            .1
            .split_once("#[derive(Debug, Clone)]\npub(crate) struct StoreBatch")
            .unwrap()
            .0;

        assert!(
            !classifier.contains(".."),
            "named StoreChange variants must reject every struct-rest pattern"
        );
        assert!(
            classifier.contains("ReplayProjectionIndependent"),
            "nonprojectable repair replay must require an explicit disposition"
        );
        assert!(
            classifier.contains("idempotent") && classifier.contains("independent"),
            "the replay disposition must state its ordering and retry contract"
        );
    }

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

    fn loaded_unread_thread() -> (WorkspaceCoordinator, SlackMessage, Vec<SlackMessage>) {
        let mut root = message("1.0", "root");
        root.reply_count = Some(2);
        root.latest_reply = Some("3.0".to_string());
        root.reply_users = Some(vec!["U2".to_string(), "U3".to_string()]);
        root.subscribed = Some(true);
        root.last_read = Some("1.0".to_string());
        root.unread_count = Some(2);
        let replies = [("2.0", "U2"), ("3.0", "U3")]
            .into_iter()
            .map(|(ts, user_id)| {
                let mut reply = message(ts, "reply");
                reply.thread_ts = Some("1.0".to_string());
                reply.user = Some(user_id.to_string());
                reply
            })
            .collect::<Vec<_>>();

        let mut coordinator = WorkspaceCoordinator::default();
        coordinator
            .apply_from(
                MutationOrigin::Cache,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        WorkspaceRevision::INITIAL,
                        MessagePage {
                            messages: vec![root.clone()],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .expect("the channel root must load");
        let thread_base = coordinator.revision();
        coordinator
            .apply_from(
                MutationOrigin::Cache,
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: "C1".to_string(),
                    thread_ts: "1.0".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        thread_base,
                        MessagePage {
                            messages: std::iter::once(root.clone())
                                .chain(replies.iter().cloned())
                                .collect(),
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .expect("the thread timeline must load");
        (coordinator, root, replies)
    }

    fn configure_attention(coordinator: &mut WorkspaceCoordinator) {
        coordinator.apply(WorkspaceMutation::AttentionContextChanged(
            WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".to_string()),
            },
        ));
    }

    fn attention_effect(reduction: &WorkspaceReduction) -> &MessageAttentionEffect {
        let Some(effect) = reduction.effects().iter().find_map(|effect| match effect {
            WorkspaceEffect::MessageAttention(effect) => Some(effect),
            WorkspaceEffect::ThreadRead(_) => None,
        }) else {
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
    fn cached_user_hydration_establishes_snapshot_authority_without_a_rewrite() {
        let mut coordinator = WorkspaceCoordinator::default();
        let user = SlackUser {
            id: Some("U1".to_string()),
            name: Some("ada".to_string()),
            ..Default::default()
        };
        coordinator
            .apply_from(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    users: vec![user.clone()],
                    ..Default::default()
                }),
            )
            .expect("cache hydration should project the stored directory");
        let refresh_base = coordinator.revision();

        assert!(
            coordinator
                .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                    refresh_base,
                    vec![user],
                )))
                .is_none(),
            "an identical refresh must not rewrite an authoritative cached directory"
        );
        assert!(coordinator.revision() > refresh_base);
    }

    #[test]
    fn cache_thread_catalog_projection_never_rewrites_the_durable_catalog() {
        let mut catalog = ThreadCatalog::default();
        let mut root = message("10.0", "root");
        root.reply_count = Some(1);
        catalog.observe_history("C1", &[root]);
        let records = catalog.into_records();
        let mut coordinator = WorkspaceCoordinator::default();

        let reduction = coordinator
            .apply_from(
                MutationOrigin::Cache,
                WorkspaceMutation::ThreadCatalogChanged(records.clone()),
            )
            .expect("cache catalog projection should update the coordinator");

        assert!(matches!(
            reduction.patch().changes(),
            [WorkspaceChange::ThreadCatalogChanged(projected)] if projected == &records
        ));
        assert!(
            reduction.store_batch().is_none(),
            "loading the durable catalog must not enqueue an identical replacement"
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
    fn newer_membership_removal_blocks_stale_resurrection_until_a_newer_snapshot_readds() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::MembershipSnapshot(
            SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                ConversationMembershipSnapshot {
                    conversations: vec![conversation("C1", "general")],
                    starred_ids: None,
                },
            ),
        ));
        let stale_base = coordinator.revision();

        coordinator.apply(WorkspaceMutation::ConversationRemove {
            channel_id: "C1".to_string(),
        });
        let removal_revision = coordinator.revision();

        assert!(coordinator
            .apply(WorkspaceMutation::MembershipSnapshot(
                SnapshotEnvelope::new(
                    stale_base,
                    ConversationMembershipSnapshot {
                        conversations: vec![conversation("C1", "stale")],
                        starred_ids: None,
                    },
                ),
            ))
            .is_none());
        assert_eq!(coordinator.revision(), removal_revision);
        assert!(coordinator.conversation("C1").is_none());

        let rejoined = coordinator
            .apply(WorkspaceMutation::MembershipSnapshot(
                SnapshotEnvelope::new(
                    removal_revision,
                    ConversationMembershipSnapshot {
                        conversations: vec![conversation("C1", "rejoined")],
                        starred_ids: None,
                    },
                ),
            ))
            .expect("a snapshot based after the removal may re-add the conversation");
        assert!(matches!(
            rejoined.patch().changes(),
            [WorkspaceChange::ConversationUpsert(conversation)]
                if conversation.id == "C1"
                    && conversation.name.as_deref() == Some("rejoined")
        ));
        assert_eq!(
            coordinator.conversation("C1").unwrap().name.as_deref(),
            Some("rejoined")
        );
    }

    #[test]
    fn unchanged_newer_membership_presence_blocks_an_older_absent_snapshot() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::MembershipSnapshot(
            SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                ConversationMembershipSnapshot {
                    conversations: vec![conversation("C1", "general")],
                    starred_ids: None,
                },
            ),
        ));
        let older_absent_base = coordinator.revision();

        coordinator.apply(WorkspaceMutation::ConversationStarChanged {
            channel_id: "C1".to_string(),
            starred: true,
        });
        let newer_presence_base = coordinator.revision();

        assert!(coordinator
            .apply(WorkspaceMutation::MembershipSnapshot(
                SnapshotEnvelope::new(
                    newer_presence_base,
                    ConversationMembershipSnapshot {
                        conversations: vec![conversation("C1", "general")],
                        starred_ids: None,
                    },
                ),
            ))
            .is_none());
        assert_eq!(coordinator.revision(), newer_presence_base);

        assert!(coordinator
            .apply(WorkspaceMutation::MembershipSnapshot(
                SnapshotEnvelope::new(
                    older_absent_base,
                    ConversationMembershipSnapshot {
                        conversations: Vec::new(),
                        starred_ids: None,
                    },
                ),
            ))
            .is_none());
        assert_eq!(coordinator.revision(), newer_presence_base);
        assert!(coordinator.conversation("C1").is_some());
        assert_eq!(
            coordinator
                .conversations
                .get("C1")
                .unwrap()
                .membership_revision,
            newer_presence_base
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
    fn authoritative_user_snapshot_removes_absent_cached_directory_entries() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                vec![
                    SlackUser {
                        id: Some("U1".to_string()),
                        name: Some("ada".to_string()),
                        ..Default::default()
                    },
                    SlackUser {
                        id: Some("U2".to_string()),
                        name: Some("grace".to_string()),
                        ..Default::default()
                    },
                ],
            )))
            .unwrap();
        let base_revision = coordinator.revision();

        let reduction = coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                base_revision,
                vec![SlackUser {
                    id: Some("U1".to_string()),
                    name: Some("ada".to_string()),
                    ..Default::default()
                }],
            )))
            .expect("the removed user must produce one reduction");

        assert_eq!(
            coordinator
                .store_projection()
                .users
                .iter()
                .filter_map(|user| user.id.as_deref())
                .collect::<Vec<_>>(),
            vec!["U1"]
        );
        assert!(matches!(
            reduction.patch().changes(),
            [WorkspaceChange::UsersReset(users)] if users.len() == 1
        ));
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::UsersReplaced(users)] if users.len() == 1
        ));
    }

    #[test]
    fn sparse_user_upsert_keeps_the_coordinator_directory_record_complete() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                vec![SlackUser {
                    id: Some("U1".to_string()),
                    name: Some("ada".to_string()),
                    real_name: Some("Ada Lovelace".to_string()),
                    profile: Some(crate::models::SlackUserProfile {
                        display_name: Some("Ada".to_string()),
                        image_72: Some("https://example.test/ada.png".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            )))
            .unwrap();

        let reduction = coordinator
            .apply(WorkspaceMutation::UserUpsert(SlackUser {
                id: Some("U1".to_string()),
                profile: Some(crate::models::SlackUserProfile {
                    status_text: Some("Focusing".to_string()),
                    status_emoji: Some(":headphones:".to_string()),
                    status_expiration: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .unwrap();

        let user = &coordinator.store_projection().users[0];
        assert_eq!(user.name.as_deref(), Some("ada"));
        assert_eq!(user.real_name.as_deref(), Some("Ada Lovelace"));
        let profile = user.profile.as_ref().unwrap();
        assert_eq!(profile.display_name.as_deref(), Some("Ada"));
        assert_eq!(
            profile.image_72.as_deref(),
            Some("https://example.test/ada.png")
        );
        assert_eq!(profile.status_text.as_deref(), Some("Focusing"));
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::UserUpsert(user)]
                if user.name.as_deref() == Some("ada")
                    && user.profile.as_ref().and_then(|profile| profile.image_72.as_deref())
                        == Some("https://example.test/ada.png")
        ));
    }

    #[test]
    fn full_user_snapshot_fills_a_newer_pre_bulk_sparse_realtime_user() {
        let mut coordinator = WorkspaceCoordinator::default();
        let snapshot_base = coordinator.revision();
        coordinator
            .apply(WorkspaceMutation::UserUpsert(SlackUser {
                id: Some("U1".to_string()),
                profile: Some(crate::models::SlackUserProfile {
                    status_text: Some("Focusing".to_string()),
                    status_emoji: Some(":headphones:".to_string()),
                    status_expiration: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .unwrap();

        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                snapshot_base,
                vec![SlackUser {
                    id: Some("U1".to_string()),
                    name: Some("ada".to_string()),
                    real_name: Some("Ada Lovelace".to_string()),
                    profile: Some(crate::models::SlackUserProfile {
                        display_name: Some("Ada".to_string()),
                        image_72: Some("https://example.test/ada.png".to_string()),
                        status_text: Some("Old status".to_string()),
                        status_emoji: Some(":hourglass:".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            )))
            .expect("the full snapshot must fill the sparse user");

        let user = &coordinator.store_projection().users[0];
        assert_eq!(user.name.as_deref(), Some("ada"));
        assert_eq!(user.real_name.as_deref(), Some("Ada Lovelace"));
        let profile = user.profile.as_ref().unwrap();
        assert_eq!(profile.display_name.as_deref(), Some("Ada"));
        assert_eq!(
            profile.image_72.as_deref(),
            Some("https://example.test/ada.png")
        );
        assert_eq!(profile.status_text.as_deref(), Some("Focusing"));
        assert_eq!(profile.status_emoji.as_deref(), Some(":headphones:"));
    }

    #[test]
    fn first_authoritative_user_snapshot_persists_an_identical_pre_bulk_user() {
        let mut coordinator = WorkspaceCoordinator::default();
        let user = SlackUser {
            id: Some("U1".to_string()),
            name: Some("ada".to_string()),
            profile: Some(crate::models::SlackUserProfile {
                status_text: Some("Focusing".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        coordinator
            .apply(WorkspaceMutation::UserUpsert(user.clone()))
            .unwrap();
        let snapshot_base = coordinator.revision();

        let reduction = coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                snapshot_base,
                vec![user],
            )))
            .expect("the first authoritative adoption must persist the full directory");

        assert!(matches!(
            reduction.patch().changes(),
            [WorkspaceChange::UsersReset(users)] if users.len() == 1
        ));
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::UsersReplaced(users)] if users.len() == 1
        ));
    }

    #[test]
    fn identical_sparse_user_upsert_records_overlay_without_a_data_reduction() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                vec![SlackUser {
                    id: Some("U1".to_string()),
                    name: Some("ada".to_string()),
                    profile: Some(crate::models::SlackUserProfile {
                        status_text: Some("Focusing".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            )))
            .unwrap();
        let stale_snapshot_base = coordinator.revision();

        assert!(
            coordinator
                .apply(WorkspaceMutation::UserUpsert(SlackUser {
                    id: Some("U1".to_string()),
                    profile: Some(crate::models::SlackUserProfile {
                        status_text: Some("Focusing".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }))
                .is_none(),
            "identical realtime data must not emit a patch or store batch"
        );
        assert!(
            coordinator.revision() > stale_snapshot_base,
            "the no-op realtime update must still advance causal authority"
        );

        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                stale_snapshot_base,
                vec![SlackUser {
                    id: Some("U1".to_string()),
                    name: Some("ada-renamed".to_string()),
                    profile: Some(crate::models::SlackUserProfile {
                        status_text: Some("Available".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            )))
            .expect("the stale snapshot may still contribute unrelated fields");
        let user = &coordinator.store_projection().users[0];
        assert_eq!(user.name.as_deref(), Some("ada-renamed"));
        assert_eq!(
            user.profile
                .as_ref()
                .and_then(|profile| profile.status_text.as_deref()),
            Some("Focusing")
        );
    }

    #[test]
    fn stale_user_snapshot_overlays_only_newer_sparse_realtime_fields() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                vec![SlackUser {
                    id: Some("U1".to_string()),
                    name: Some("ada".to_string()),
                    profile: Some(crate::models::SlackUserProfile {
                        email: Some("old@example.test".to_string()),
                        status_text: Some("Available".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            )))
            .unwrap();
        let snapshot_base = coordinator.revision();

        coordinator
            .apply(WorkspaceMutation::UserUpsert(SlackUser {
                id: Some("U1".to_string()),
                profile: Some(crate::models::SlackUserProfile {
                    status_text: Some("Focusing".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .unwrap();

        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                snapshot_base,
                vec![SlackUser {
                    id: Some("U1".to_string()),
                    name: Some("ada-renamed".to_string()),
                    profile: Some(crate::models::SlackUserProfile {
                        email: Some("new@example.test".to_string()),
                        status_text: Some("Available".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            )))
            .unwrap();

        let user = &coordinator.store_projection().users[0];
        assert_eq!(user.name.as_deref(), Some("ada-renamed"));
        let profile = user.profile.as_ref().unwrap();
        assert_eq!(profile.email.as_deref(), Some("new@example.test"));
        assert_eq!(profile.status_text.as_deref(), Some("Focusing"));
    }

    #[test]
    fn stale_user_snapshot_cannot_resurrect_a_newer_snapshot_tombstone() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                vec![SlackUser {
                    id: Some("U1".to_string()),
                    name: Some("ada".to_string()),
                    ..Default::default()
                }],
            )))
            .unwrap();
        let stale_snapshot_base = coordinator.revision();

        coordinator
            .apply(WorkspaceMutation::ConversationUpsert(conversation(
                "C1", "general",
            )))
            .unwrap();
        let removal_base = coordinator.revision();
        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                removal_base,
                Vec::new(),
            )))
            .expect("the newer snapshot must remove the absent user");
        let removal_revision = coordinator.revision();

        assert!(
            coordinator
                .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                    stale_snapshot_base,
                    vec![SlackUser {
                        id: Some("U1".to_string()),
                        name: Some("stale".to_string()),
                        ..Default::default()
                    }],
                )))
                .is_none(),
            "a stale snapshot must not resurrect an authoritative removal"
        );
        assert_eq!(coordinator.revision(), removal_revision);
        assert!(coordinator.store_projection().users.is_empty());
    }

    #[test]
    fn stale_user_snapshot_cannot_roll_back_fields_after_overlay_retirement() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                vec![SlackUser {
                    id: Some("U1".to_string()),
                    name: Some("ada".to_string()),
                    profile: Some(crate::models::SlackUserProfile {
                        status_text: Some("Available".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            )))
            .unwrap();
        let stale_snapshot_base = coordinator.revision();

        coordinator
            .apply(WorkspaceMutation::UserUpsert(SlackUser {
                id: Some("U1".to_string()),
                profile: Some(crate::models::SlackUserProfile {
                    status_text: Some("Focusing".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }))
            .unwrap();
        let fresh_snapshot_base = coordinator.revision();
        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                fresh_snapshot_base,
                vec![SlackUser {
                    id: Some("U1".to_string()),
                    name: Some("ada-renamed".to_string()),
                    profile: Some(crate::models::SlackUserProfile {
                        status_text: Some("Focusing".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            )))
            .expect("the fresh snapshot must update the authoritative name");
        let fresh_revision = coordinator.revision();

        assert!(
            coordinator
                .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                    stale_snapshot_base,
                    vec![SlackUser {
                        id: Some("U1".to_string()),
                        name: Some("stale-name".to_string()),
                        profile: Some(crate::models::SlackUserProfile {
                            status_text: Some("Available".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                )))
                .is_none(),
            "a stale snapshot must not roll back accepted authoritative fields"
        );
        assert_eq!(coordinator.revision(), fresh_revision);
        let user = &coordinator.store_projection().users[0];
        assert_eq!(user.name.as_deref(), Some("ada-renamed"));
        assert_eq!(
            user.profile
                .as_ref()
                .and_then(|profile| profile.status_text.as_deref()),
            Some("Focusing")
        );
    }

    #[test]
    fn identical_fresh_user_snapshot_blocks_an_older_field_rollback() {
        let mut coordinator = WorkspaceCoordinator::default();
        let user = SlackUser {
            id: Some("U1".to_string()),
            name: Some("ada".to_string()),
            ..Default::default()
        };
        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                vec![user.clone()],
            )))
            .unwrap();
        let stale_snapshot_base = coordinator.revision();

        coordinator
            .apply(WorkspaceMutation::ConversationUpsert(conversation(
                "C1", "general",
            )))
            .unwrap();
        let fresh_snapshot_base = coordinator.revision();
        assert!(
            coordinator
                .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                    fresh_snapshot_base,
                    vec![user],
                )))
                .is_none(),
            "identical authoritative data must not emit a reduction"
        );
        let fresh_authority_revision = coordinator.revision();
        assert!(fresh_authority_revision > fresh_snapshot_base);

        assert!(
            coordinator
                .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                    stale_snapshot_base,
                    vec![SlackUser {
                        id: Some("U1".to_string()),
                        name: Some("stale-name".to_string()),
                        ..Default::default()
                    }],
                )))
                .is_none(),
            "the accepted fresh snapshot must establish internal authority"
        );
        assert_eq!(coordinator.revision(), fresh_authority_revision);
        assert_eq!(
            coordinator.store_projection().users[0].name.as_deref(),
            Some("ada")
        );
    }

    #[test]
    fn accepted_identical_user_snapshot_blocks_a_conflicting_same_base_response() {
        let mut coordinator = WorkspaceCoordinator::default();
        let user = SlackUser {
            id: Some("U1".to_string()),
            name: Some("ada".to_string()),
            ..Default::default()
        };
        coordinator
            .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                vec![user.clone()],
            )))
            .unwrap();
        let response_base = coordinator.revision();

        assert!(
            coordinator
                .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                    response_base,
                    vec![user],
                )))
                .is_none(),
            "identical authoritative data must not emit a reduction"
        );
        let accepted_revision = coordinator.revision();
        assert!(accepted_revision > response_base);

        assert!(
            coordinator
                .apply(WorkspaceMutation::UsersSnapshot(SnapshotEnvelope::new(
                    response_base,
                    vec![SlackUser {
                        id: Some("U1".to_string()),
                        name: Some("stale-name".to_string()),
                        ..Default::default()
                    }],
                )))
                .is_none(),
            "a conflicting response at the accepted base must be stale"
        );
        assert_eq!(coordinator.revision(), accepted_revision);
        assert_eq!(
            coordinator.store_projection().users[0].name.as_deref(),
            Some("ada")
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
    fn local_conversation_read_has_one_canonical_patch_and_durable_conversation() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut channel = conversation("C1", "general");
        channel.observe_attention_message_at("2.0", true);
        channel.observe_attention_message_at("3.0", true);
        coordinator.apply(WorkspaceMutation::ConversationUpsert(channel));

        let reduction = coordinator
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::ReadAdvanced {
                    channel_id: "C1".to_string(),
                    ts: "3.0".to_string(),
                    remaining_unread: 0,
                },
            )
            .unwrap();

        assert!(matches!(
            reduction.patch().changes(),
            [WorkspaceChange::ConversationUpsert(conversation)]
                if conversation.id == "C1"
                    && conversation.local_read_ts() == Some("3.0")
                    && conversation.raw_unread_activity_count() == 0
                    && conversation.unread_activity_count() == 0
        ));
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::ConversationUpsert(conversation)]
                if conversation.id == "C1"
                    && conversation.local_read_ts() == Some("3.0")
                    && conversation.unread_activity_count() == 0
        ));
        let revision = coordinator.revision();
        assert!(coordinator
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::ReadAdvanced {
                    channel_id: "C1".to_string(),
                    ts: "3.0".to_string(),
                    remaining_unread: 0,
                },
            )
            .is_none());
        assert_eq!(coordinator.revision(), revision);
        assert!(coordinator
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::ReadAdvanced {
                    channel_id: "C1".to_string(),
                    ts: "2.0".to_string(),
                    remaining_unread: 0,
                },
            )
            .is_none());
        assert_eq!(coordinator.revision(), revision);
        assert_eq!(
            coordinator.conversation("C1").unwrap().local_read_ts(),
            Some("3.0")
        );
    }

    #[test]
    fn local_thread_read_reconciles_catalog_and_attention_in_one_reduction() {
        let mut channel = conversation("C1", "general");
        channel.observe_attention_message_at("2.0", true);
        channel.observe_attention_message_at("4.0", true);
        channel.observe_attention_message_at("10.0", true);
        let mut catalog = crate::thread_catalog::ThreadCatalog::default();
        let mut root = message("1.0", "root");
        root.subscribed = Some(true);
        root.unread_count = Some(0);
        catalog.observe_thread("C1", "1.0", &[root], false);
        for ts in ["2.0", "4.0"] {
            let mut reply = message(ts, "reply");
            reply.thread_ts = Some("1.0".to_string());
            reply.user = Some("U_OTHER".to_string());
            catalog.reconcile_message(
                "C1",
                &reply,
                &[],
                ThreadCatalogMessageKind::Posted,
                Some("U_SELF"),
            );
        }
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![channel],
            threads: catalog.into_records(),
            ..Default::default()
        }));

        let reduction = coordinator
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::ThreadReadAdvanced {
                    channel_id: "C1".to_string(),
                    thread_ts: "1.0".to_string(),
                    ts: "3.0".to_string(),
                },
            )
            .unwrap();

        assert!(matches!(
            reduction.patch().changes(),
            [
                WorkspaceChange::ConversationUpsert(conversation),
                WorkspaceChange::ThreadCatalogChanged(records),
            ] if conversation.id == "C1"
                && conversation.unread_activity_count() == 2
                && conversation.has_observed_attention_message("4.0")
                && conversation.has_observed_attention_message("10.0")
                && matches!(
                    records.as_slice(),
                    [record] if record.unread == ThreadUnreadState::Known {
                        count: 1,
                        last_read: Some("3.0".to_string()),
                    }
                )
        ));
        assert!(matches!(
            reduction.effects(),
            [WorkspaceEffect::ThreadRead(effect)]
                if effect.channel_id == "C1"
                    && effect.thread_ts == "1.0"
                    && effect.ts == "3.0"
                    && effect.acknowledged_message_ts == ["2.0".to_string()]
        ));
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [
                StoreChange::ThreadCatalogReplaced(records),
                StoreChange::ConversationUpsert(conversation),
            ] if matches!(
                    records.as_slice(),
                    [record] if record.unread == ThreadUnreadState::Known {
                        count: 1,
                        last_read: Some("3.0".to_string()),
                    }
                )
                && conversation.id == "C1"
                && conversation.unread_activity_count() == 2
                && conversation.has_observed_attention_message("4.0")
                && conversation.has_observed_attention_message("10.0")
        ));
        let revision = coordinator.revision();
        assert!(coordinator
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::ThreadReadAdvanced {
                    channel_id: "C1".to_string(),
                    thread_ts: "1.0".to_string(),
                    ts: "3.0".to_string(),
                },
            )
            .is_none());
        assert_eq!(coordinator.revision(), revision);

        let advanced = coordinator
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::ThreadReadAdvanced {
                    channel_id: "C1".to_string(),
                    thread_ts: "1.0".to_string(),
                    ts: "5.0".to_string(),
                },
            )
            .unwrap();
        assert!(matches!(
            advanced.effects(),
            [WorkspaceEffect::ThreadRead(effect)]
                if effect.acknowledged_message_ts == ["4.0".to_string()]
        ));
        let revision = coordinator.revision();
        assert!(coordinator
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::ThreadReadAdvanced {
                    channel_id: "C1".to_string(),
                    thread_ts: "1.0".to_string(),
                    ts: "4.0".to_string(),
                },
            )
            .is_none());
        assert_eq!(coordinator.revision(), revision);
    }

    #[test]
    fn thread_read_synchronizes_loaded_roots_and_store_projection_intent() {
        let (mut coordinator, _, _) = loaded_unread_thread();
        let reduction = coordinator
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::ThreadReadAdvanced {
                    channel_id: "C1".to_string(),
                    thread_ts: "1.0".to_string(),
                    ts: "2.0".to_string(),
                },
            )
            .expect("the newer thread read must reduce once");

        let projection = coordinator.store_projection();
        let history = projection.histories.get("C1").unwrap();
        let thread = projection
            .thread_timelines
            .get(&("C1".to_string(), "1.0".to_string()))
            .unwrap();
        let catalog = &projection.thread_catalog;
        for root in [
            history.iter().find(|message| message.ts == "1.0").unwrap(),
            thread.iter().find(|message| message.ts == "1.0").unwrap(),
            catalog[0].root.as_ref().unwrap(),
        ] {
            assert_eq!(root.last_read.as_deref(), Some("2.0"));
            assert_eq!(root.unread_count, Some(1));
        }
        assert_eq!(
            catalog[0].unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("2.0".to_string()),
            }
        );

        assert!(matches!(
            reduction.patch().changes(),
            [
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Channel(channel_id),
                    changes: channel_changes,
                },
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread {
                        channel_id: thread_channel_id,
                        thread_ts,
                    },
                    changes: thread_changes,
                },
                WorkspaceChange::ThreadCatalogChanged(records),
            ] if channel_id == "C1"
                && thread_channel_id == "C1"
                && thread_ts == "1.0"
                && matches!(
                    channel_changes.as_slice(),
                    [MessageChange::Upsert(root)]
                        if root.last_read.as_deref() == Some("2.0")
                            && root.unread_count == Some(1)
                )
                && matches!(
                    thread_changes.as_slice(),
                    [MessageChange::Upsert(root)]
                        if root.last_read.as_deref() == Some("2.0")
                            && root.unread_count == Some(1)
                )
                && records == catalog
        ));
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [
                StoreChange::HistoryReplaced {
                    channel_id,
                    messages: stored_history,
                },
                StoreChange::ThreadReplaced {
                    channel_id: thread_channel_id,
                    thread_ts,
                    messages: stored_thread,
                },
                StoreChange::ThreadCatalogReplaced(stored_catalog),
            ] if channel_id == "C1"
                && thread_channel_id == "C1"
                && thread_ts == "1.0"
                && stored_history == history
                && stored_thread == thread
                && stored_catalog == catalog
        ));
    }

    #[test]
    fn stale_snapshots_cannot_roll_back_a_newer_thread_read() {
        let (mut coordinator, stale_root, replies) = loaded_unread_thread();
        let stale_base = coordinator.revision();
        coordinator
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::ThreadReadAdvanced {
                    channel_id: "C1".to_string(),
                    thread_ts: "1.0".to_string(),
                    ts: "2.0".to_string(),
                },
            )
            .expect("the newer thread read must reduce once");
        let read_revision = coordinator.revision();
        let thread_key = ThreadKey::new("C1", "1.0").unwrap();
        assert_eq!(
            coordinator.thread_catalog_revisions.get(&thread_key),
            Some(&read_revision)
        );

        assert!(coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        stale_base,
                        MessagePage {
                            messages: vec![stale_root.clone()],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .is_none());
        let stale_thread = coordinator.apply_from(
            MutationOrigin::WebApi,
            WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".to_string(),
                thread_ts: "1.0".to_string(),
                snapshot: SnapshotEnvelope::new(
                    stale_base,
                    MessagePage {
                        messages: std::iter::once(stale_root).chain(replies).collect(),
                        complete: true,
                        ..Default::default()
                    },
                ),
            },
        );
        if let Some(reduction) = stale_thread {
            for change in reduction.patch().changes() {
                if let WorkspaceChange::ThreadCatalogChanged(records) = change {
                    assert_eq!(
                        records[0].unread,
                        ThreadUnreadState::Known {
                            count: 1,
                            last_read: Some("2.0".to_string()),
                        }
                    );
                }
            }
            for change in reduction.store_batch().unwrap().changes() {
                if let StoreChange::ThreadCatalogReplaced(records) = change {
                    assert_eq!(
                        records[0].unread,
                        ThreadUnreadState::Known {
                            count: 1,
                            last_read: Some("2.0".to_string()),
                        }
                    );
                }
            }
        }
        assert!(coordinator.revision() >= read_revision);

        let history_root = coordinator
            .history("C1")
            .into_iter()
            .find(|message| message.ts == "1.0")
            .unwrap();
        let thread_root = coordinator
            .thread("C1", "1.0")
            .into_iter()
            .find(|message| message.ts == "1.0")
            .unwrap();
        let catalog_root = coordinator.thread_catalog[0].root.as_ref().unwrap();
        for root in [&history_root, &thread_root, catalog_root] {
            assert_eq!(root.last_read.as_deref(), Some("2.0"));
            assert_eq!(root.unread_count, Some(1));
        }
        for (_, revision) in coordinator
            .history_with_revisions("C1")
            .into_iter()
            .chain(coordinator.thread_with_revisions("C1", "1.0"))
            .filter(|(message, _)| message.ts == "1.0")
        {
            assert!(revision >= read_revision);
        }
        assert_eq!(
            coordinator.thread_catalog[0].unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("2.0".to_string()),
            }
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
        let batch = reduction.store_batch().unwrap();
        assert!(matches!(
            batch.changes(),
            [
                StoreChange::MessageDelta { .. },
                StoreChange::ConversationAttentionObserved { .. },
                StoreChange::AttentionNotificationClaim { identity },
            ] if identity.channel_id == "D1" && identity.message_ts == "10.0"
        ));
        assert!(matches!(
            batch.workspace_repair_replay_changes().as_slice(),
            [StoreChange::AttentionNotificationClaim { identity }]
                if identity.channel_id == "D1" && identity.message_ts == "10.0"
        ));
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
    fn newer_read_cursor_rejects_attention_observation_and_notification_claim() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let mut direct = conversation("D1", "direct");
        direct.is_channel = Some(false);
        direct.is_im = Some(true);
        direct.set_local_read_ts("20.0");
        coordinator.apply(WorkspaceMutation::ConversationUpsert(direct));
        let mut incoming = message("10.0", "already read");
        incoming.user = Some("U_OTHER".to_string());

        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChangedWithDelivery {
                channel_id: "D1".to_string(),
                message: incoming,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
                delivery: DeliveryState::Fresh,
            })
            .unwrap();

        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::MessageDelta { .. }]
        ));
        assert!(!reduction.patch().changes().iter().any(|change| matches!(
            change,
            WorkspaceChange::ConversationAttentionObserved { .. }
        )));
        assert!(!coordinator
            .conversation("D1")
            .unwrap()
            .has_observed_attention_message("10.0"));
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
    fn sparse_realtime_attention_placeholder_accepts_in_flight_membership_enrichment() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let snapshot_base = coordinator.revision();
        let mut incoming = message("10.0", "hello before membership refresh");
        incoming.user = Some("U_OTHER".to_string());

        coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "D_NEW".to_string(),
                message: incoming,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .expect("realtime attention should create a sparse placeholder");

        let mut enriched = conversation("D_NEW", "Ada");
        enriched.is_channel = Some(false);
        enriched.is_im = Some(true);
        enriched.user = Some("U_ADA".to_string());
        coordinator
            .apply(WorkspaceMutation::MembershipSnapshot(
                SnapshotEnvelope::new(
                    snapshot_base,
                    ConversationMembershipSnapshot {
                        conversations: vec![enriched],
                        starred_ids: None,
                    },
                ),
            ))
            .expect("the in-flight membership response should enrich empty metadata");

        let current = coordinator.conversation("D_NEW").unwrap();
        assert_eq!(current.name.as_deref(), Some("Ada"));
        assert_eq!(current.user.as_deref(), Some("U_ADA"));
        assert_eq!(current.is_im, Some(true));
        assert!(current.has_observed_attention_message("10.0"));
        assert_eq!(current.unread_activity_count(), 1);
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
            match effect {
                WorkspaceEffect::MessageAttention(effect) => !effect.decision.send_notification,
                WorkspaceEffect::ThreadRead(_) => false,
            }
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
                .filter_map(|effect| match effect {
                    WorkspaceEffect::MessageAttention(effect) => Some(effect.message.ts.as_str()),
                    WorkspaceEffect::ThreadRead(_) => None,
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
    fn cache_history_reuses_durable_timeline_but_persists_semantic_attention() {
        let mut coordinator = WorkspaceCoordinator::default();
        configure_attention(&mut coordinator);
        let mut channel = conversation("C1", "general");
        channel
            .extra
            .insert("last_read".to_string(), serde_json::json!("10.0"));
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            conversations: vec![channel],
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
            [
                WorkspaceChange::TimelineChanged { .. },
                WorkspaceChange::ConversationAttentionObserved {
                    channel_id,
                    observations,
                },
            ] if channel_id == "C1"
                && observations == &[ConversationAttentionObservation {
                    message_ts: "11.0".to_string(),
                    record_unread: true,
                }]
        ));
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [StoreChange::ConversationAttentionObserved {
                channel_id,
                observations,
            }] if channel_id == "C1"
                && observations == &[ConversationAttentionObservation {
                    message_ts: "11.0".to_string(),
                    record_unread: true,
                }]
        ));
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
                WorkspaceChange::ThreadCatalogChanged(records),
            ] if records.iter().any(|record| {
                record.key.channel_id == "C1"
                    && record.key.root_ts == "10.0"
                    && record.reply_count == 1
            })
        ));
        assert!(
            reduction.store_batch().is_none(),
            "cache-origin threads are already durable and must not emit timeline or catalog writes"
        );
    }

    #[test]
    fn history_snapshot_reconciles_thread_catalog_in_the_same_reduction() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut root = message("10.0", "root");
        root.reply_count = Some(1);
        root.latest_reply = Some("11.0".to_string());
        root.reply_users = Some(vec!["U2".to_string()]);
        root.subscribed = Some(true);
        root.last_read = Some("10.0".to_string());
        root.unread_count = Some(1);
        let mut reply = message("11.0", "reply");
        reply.thread_ts = Some("10.0".to_string());
        reply.user = Some("U2".to_string());

        let reduction = coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        WorkspaceRevision::INITIAL,
                        MessagePage {
                            messages: vec![root, reply],
                            complete: false,
                            ..Default::default()
                        },
                    ),
                },
            )
            .unwrap();

        let catalog_records = reduction
            .patch()
            .changes()
            .iter()
            .find_map(|change| match change {
                WorkspaceChange::ThreadCatalogChanged(records) => Some(records),
                _ => None,
            })
            .expect("history and catalog must share one coordinator reduction");
        let record = catalog_records
            .iter()
            .find(|record| record.key.channel_id == "C1" && record.key.root_ts == "10.0")
            .unwrap();
        assert_eq!(record.reply_count, 1);
        assert_eq!(record.latest_reply.as_deref(), Some("11.0"));
        assert!(record.participant_user_ids.contains("U2"));
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [
                StoreChange::HistoryReplaced { .. },
                StoreChange::ThreadCatalogReplaced(stored),
            ] if stored == catalog_records
        ));
    }

    #[test]
    fn stale_history_root_cannot_roll_back_newer_realtime_thread_catalog_unread() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::AttentionContextChanged(
            WorkspaceAttentionContext {
                current_user_id: Some("U_SELF".to_string()),
            },
        ));
        let mut root = message("10.0", "root");
        root.reply_count = Some(1);
        root.latest_reply = Some("10.5".to_string());
        root.subscribed = Some(true);
        root.last_read = Some("10.5".to_string());
        root.unread_count = Some(0);
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
        let snapshot_base = coordinator.revision();

        let mut reply = message("11.0", "new reply");
        reply.thread_ts = Some("10.0".to_string());
        reply.user = Some("U_OTHER".to_string());
        let realtime = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: reply,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        let realtime_record = realtime
            .patch()
            .changes()
            .iter()
            .find_map(|change| match change {
                WorkspaceChange::ThreadCatalogChanged(records) => records
                    .iter()
                    .find(|record| record.key.channel_id == "C1" && record.key.root_ts == "10.0"),
                _ => None,
            })
            .expect("realtime reply did not advance the thread catalog");
        assert_eq!(
            realtime_record.unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("10.5".to_string()),
            }
        );

        assert!(
            coordinator
                .apply(WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        snapshot_base,
                        MessagePage {
                            messages: vec![root],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                })
                .is_none(),
            "stale history must not emit a catalog rollback"
        );
        let record = coordinator
            .thread_catalog
            .iter()
            .find(|record| record.key.channel_id == "C1" && record.key.root_ts == "10.0")
            .unwrap();
        assert_eq!(
            record.unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("10.5".to_string()),
            }
        );
        assert_eq!(
            record.root.as_ref().and_then(|root| root.unread_count),
            Some(1)
        );
    }

    #[test]
    fn stale_history_hydrates_root_content_with_newer_posted_reply_aggregates() {
        let mut earlier_reply = message("10.5", "earlier reply");
        earlier_reply.thread_ts = Some("10.0".to_string());
        earlier_reply.user = Some("U2".to_string());
        let mut catalog = ThreadCatalog::default();
        catalog.observe_history("C1", std::slice::from_ref(&earlier_reply));

        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            threads: catalog.into_records(),
            ..Default::default()
        }));
        let stale_history_base = coordinator.revision();

        let mut realtime_reply = message("11.0", "realtime reply");
        realtime_reply.thread_ts = Some("10.0".to_string());
        realtime_reply.user = Some("U3".to_string());
        coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: realtime_reply,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert!(coordinator.history("C1").is_empty());

        let mut stale_root = message("10.0", "hydrated root content");
        stale_root.user = Some("U_ROOT".to_string());
        stale_root.reply_count = Some(1);
        stale_root.latest_reply = Some("10.5".to_string());
        stale_root.reply_users = Some(vec!["U2".to_string()]);
        coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".to_string(),
                snapshot: SnapshotEnvelope::new(
                    stale_history_base,
                    MessagePage {
                        messages: vec![stale_root],
                        complete: false,
                        ..Default::default()
                    },
                ),
            })
            .unwrap();

        let projected_root = coordinator
            .history("C1")
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        assert_eq!(
            projected_root.text.as_deref(),
            Some("hydrated root content")
        );
        assert_eq!(projected_root.user.as_deref(), Some("U_ROOT"));
        assert_eq!(projected_root.reply_count, Some(2));
        assert_eq!(projected_root.latest_reply.as_deref(), Some("11.0"));
        assert!(["U2", "U3"].iter().all(|user_id| {
            projected_root
                .reply_users
                .as_ref()
                .is_some_and(|users| users.iter().any(|known| known == user_id))
        }));

        let record = coordinator
            .thread_catalog
            .iter()
            .find(|record| record.key.root_ts == "10.0")
            .unwrap();
        assert_eq!(record.reply_count, 2);
        assert_eq!(record.latest_reply.as_deref(), Some("11.0"));
        let catalog_root = record.root.as_ref().unwrap();
        assert_eq!(catalog_root.text.as_deref(), Some("hydrated root content"));
        assert_eq!(catalog_root.reply_count, Some(2));
        assert_eq!(catalog_root.latest_reply.as_deref(), Some("11.0"));
    }

    #[test]
    fn older_history_cannot_restore_reply_removed_by_complete_thread() {
        let mut root = message("10.0", "thread root");
        root.reply_count = Some(2);
        root.latest_reply = Some("12.0".to_string());
        root.reply_users = Some(vec!["U2".to_string(), "U3".to_string()]);
        let mut retained = message("11.0", "retained reply");
        retained.thread_ts = Some("10.0".to_string());
        retained.user = Some("U2".to_string());
        let mut removed_broadcast = message("12.0", "removed broadcast");
        removed_broadcast.thread_ts = Some("10.0".to_string());
        removed_broadcast.subtype = Some("thread_broadcast".to_string());
        removed_broadcast.user = Some("U3".to_string());

        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply_from(
            MutationOrigin::Cache,
            WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".to_string(),
                thread_ts: "10.0".to_string(),
                snapshot: SnapshotEnvelope::new(
                    WorkspaceRevision::INITIAL,
                    MessagePage {
                        messages: vec![root.clone(), retained.clone(), removed_broadcast.clone()],
                        complete: true,
                        ..Default::default()
                    },
                ),
            },
        );
        let older_history_base = coordinator.revision();

        let mut canonical_root = root.clone();
        canonical_root.reply_count = Some(1);
        canonical_root.latest_reply = Some("11.0".to_string());
        canonical_root.reply_users = Some(vec!["U2".to_string()]);
        coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: "C1".to_string(),
                    thread_ts: "10.0".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        older_history_base,
                        MessagePage {
                            messages: vec![canonical_root, retained],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .unwrap();

        let stale_history = coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".to_string(),
                snapshot: SnapshotEnvelope::new(
                    older_history_base,
                    MessagePage {
                        messages: vec![root, removed_broadcast],
                        complete: false,
                        ..Default::default()
                    },
                ),
            })
            .unwrap();
        assert!(stale_history.patch().changes().iter().any(|change| {
            matches!(
                change,
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Channel(_),
                    changes,
                } if changes.iter().any(|change| {
                    matches!(
                        change,
                        MessageChange::Upsert(message)
                            if message.ts == "10.0"
                                && message.reply_count == Some(1)
                                && message.latest_reply.as_deref() == Some("11.0")
                    )
                })
            )
        }));
        assert!(coordinator
            .history("C1")
            .iter()
            .all(|message| message.ts != "12.0"));
        let thread = coordinator
            .threads
            .get(&("C1".to_string(), "10.0".to_string()))
            .unwrap()
            .messages();
        let thread_root = thread.iter().find(|message| message.ts == "10.0").unwrap();
        assert_eq!(thread_root.reply_count, Some(1));
        assert_eq!(thread_root.latest_reply.as_deref(), Some("11.0"));
        assert!(thread.iter().all(|message| message.ts != "12.0"));
        let record = coordinator
            .thread_catalog
            .iter()
            .find(|record| record.key.root_ts == "10.0")
            .unwrap();
        assert_eq!(record.reply_count, 1);
        assert_eq!(record.latest_reply.as_deref(), Some("11.0"));
        assert_eq!(record.root.as_ref().unwrap().reply_count, Some(1));
    }

    #[test]
    fn durable_reply_removal_overlays_same_base_history_root_metadata() {
        let mut root = message("10.0", "cached root");
        root.reply_count = Some(2);
        root.latest_reply = Some("12.0".to_string());
        root.reply_users = Some(vec!["U2".to_string(), "U3".to_string()]);
        let mut retained = message("11.0", "retained");
        retained.thread_ts = Some("10.0".to_string());
        retained.user = Some("U2".to_string());
        let mut removed = message("12.0", "removed");
        removed.thread_ts = Some("10.0".to_string());
        removed.subtype = Some("thread_broadcast".to_string());
        removed.user = Some("U3".to_string());

        let mut catalog = ThreadCatalog::default();
        catalog.observe_thread(
            "C1",
            "10.0",
            &[root, retained.clone(), removed.clone()],
            true,
        );
        catalog.reconcile_message(
            "C1",
            &removed,
            std::slice::from_ref(&removed),
            ThreadCatalogMessageKind::Deleted,
            Some("ME"),
        );
        let records = catalog.into_records();
        let canonical_root = records[0].root.clone().unwrap();
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            histories: HashMap::from([("C1".to_string(), vec![canonical_root])]),
            threads: records,
            ..Default::default()
        }));
        let snapshot_base = coordinator.revision();

        let mut stale_root = message("10.0", "fresh root content");
        stale_root.reply_count = Some(2);
        stale_root.latest_reply = Some("12.0".to_string());
        stale_root.reply_users = Some(vec!["U2".to_string(), "U3".to_string()]);
        coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".to_string(),
                snapshot: SnapshotEnvelope::new(
                    snapshot_base,
                    MessagePage {
                        messages: vec![stale_root, removed],
                        complete: false,
                        ..Default::default()
                    },
                ),
            })
            .unwrap();

        let root = coordinator
            .history("C1")
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        assert_eq!(root.text.as_deref(), Some("fresh root content"));
        assert_eq!(root.reply_count, Some(1));
        assert_eq!(root.latest_reply.as_deref(), Some("11.0"));
        assert!(coordinator
            .history("C1")
            .iter()
            .all(|message| message.ts != "12.0"));
    }

    #[test]
    fn complete_thread_snapshot_reconciles_exact_catalog_metadata_atomically() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut root = message("10.0", "root");
        root.reply_count = Some(3);
        root.latest_reply = Some("13.0".to_string());
        root.reply_users = Some(vec!["U1".to_string(), "U2".to_string(), "U3".to_string()]);
        root.subscribed = Some(true);
        root.last_read = Some("10.0".to_string());
        root.unread_count = Some(3);
        let reply = |ts: &str, user: &str| SlackMessage {
            ts: ts.to_string(),
            thread_ts: Some("10.0".to_string()),
            user: Some(user.to_string()),
            ..Default::default()
        };
        let retained = reply("11.0", "U1");
        coordinator.apply_from(
            MutationOrigin::Cache,
            WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".to_string(),
                thread_ts: "10.0".to_string(),
                snapshot: SnapshotEnvelope::new(
                    WorkspaceRevision::INITIAL,
                    MessagePage {
                        messages: vec![
                            root.clone(),
                            retained.clone(),
                            reply("12.0", "U2"),
                            reply("13.0", "U3"),
                        ],
                        complete: true,
                        ..Default::default()
                    },
                ),
            },
        );

        let reduction = coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: "C1".to_string(),
                    thread_ts: "10.0".to_string(),
                    snapshot: SnapshotEnvelope::new(
                        coordinator.revision(),
                        MessagePage {
                            messages: vec![root, retained],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .unwrap();

        let catalog_records = reduction
            .patch()
            .changes()
            .iter()
            .find_map(|change| match change {
                WorkspaceChange::ThreadCatalogChanged(records) => Some(records),
                _ => None,
            })
            .expect("complete thread snapshot did not reconcile the catalog");
        let record = catalog_records
            .iter()
            .find(|record| record.key.channel_id == "C1" && record.key.root_ts == "10.0")
            .unwrap();
        assert_eq!(record.reply_count, 1);
        assert_eq!(record.latest_reply.as_deref(), Some("11.0"));
        assert_eq!(
            record.unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("10.0".to_string()),
            }
        );
        assert!(["U1", "U2", "U3"]
            .iter()
            .all(|user_id| record.participant_user_ids.contains(*user_id)));
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [
                StoreChange::ThreadReplaced { messages, .. },
                StoreChange::ThreadCatalogReplaced(stored),
            ] if messages.iter().filter(|message| message.is_thread_reply()).count() == 1
                && stored == catalog_records
        ));
    }

    #[test]
    fn web_api_complete_root_only_thread_clears_metadata_only_aggregates_everywhere() {
        let mut root = message("10.0", "root");
        root.reply_count = Some(2);
        root.latest_reply = Some("12.0".to_string());
        root.reply_users = Some(vec!["U2".to_string(), "U3".to_string()]);
        let mut catalog = ThreadCatalog::default();
        catalog.observe_history("C1", std::slice::from_ref(&root));
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply_from(
            MutationOrigin::Cache,
            WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                histories: HashMap::from([("C1".to_string(), vec![root.clone()])]),
                threads: catalog.into_records(),
                ..Default::default()
            }),
        );

        let reduction = coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::ThreadSnapshot {
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
                },
            )
            .unwrap();

        let channel_root = coordinator
            .history("C1")
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        let thread_root = coordinator
            .thread("C1", "10.0")
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        for root in [channel_root, thread_root] {
            assert_eq!(root.reply_count, Some(0));
            assert_eq!(root.latest_reply, None);
            assert_eq!(root.reply_users.as_deref(), Some(&[][..]));
        }
        let record = coordinator
            .thread_catalog
            .iter()
            .find(|record| record.key.root_ts == "10.0")
            .unwrap();
        assert_eq!(record.reply_count, 0);
        assert_eq!(record.latest_reply, None);
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [
                StoreChange::ThreadReplaced { messages, .. },
                StoreChange::ThreadCatalogReplaced(_),
            ] if messages.iter().any(|message| {
                message.ts == "10.0"
                    && message.reply_count == Some(0)
                    && message.latest_reply.is_none()
            })
        ));
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
        assert!(matches!(
            thread.store_batch().unwrap().changes(),
            [
                StoreChange::ThreadReplaced {
                    channel_id,
                    thread_ts,
                    messages,
                },
                StoreChange::ThreadCatalogReplaced(_),
            ] if channel_id == "C1"
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
            [
                StoreChange::MessageDelta {
                    channel_id,
                    message,
                    kind: MessageMutationKind::Posted,
                },
                StoreChange::ThreadCatalogReplaced(_),
            ] if channel_id == "C1" && message.ts == "11.0"
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
                WorkspaceChange::ThreadCatalogChanged(_),
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
            [
                StoreChange::MessageDelta {
                    channel_id,
                    message,
                    kind: MessageMutationKind::Posted,
                },
                StoreChange::ThreadCatalogReplaced(_),
            ] if channel_id == "C1" && message.ts == "12.0"
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
                WorkspaceChange::ThreadCatalogChanged(_),
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
            [
                StoreChange::MessageDelta {
                    channel_id,
                    message,
                    kind: MessageMutationKind::Changed,
                },
                StoreChange::ThreadCatalogReplaced(_),
            ] if channel_id == "C1"
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
    fn unhydrated_broadcast_to_reply_edit_preserves_root_aggregates() {
        let mut root = message("10.0", "root");
        root.reply_count = Some(1);
        root.latest_reply = Some("11.0".to_string());
        root.reply_users = Some(vec!["U2".to_string()]);
        let mut broadcast = message("11.0", "broadcast");
        broadcast.thread_ts = Some("10.0".to_string());
        broadcast.subtype = Some("thread_broadcast".to_string());
        broadcast.user = Some("U2".to_string());
        let mut catalog = ThreadCatalog::default();
        catalog.observe_thread("C1", "10.0", &[root.clone(), broadcast.clone()], true);

        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            histories: HashMap::from([("C1".to_string(), vec![root.clone(), broadcast.clone()])]),
            threads: catalog.into_records(),
            ..Default::default()
        }));

        let normal = SlackMessage {
            subtype: None,
            text: Some("normal reply".to_string()),
            ..broadcast
        };
        coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: normal,
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        let projected_root = coordinator
            .history("C1")
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        assert_eq!(projected_root.reply_count, Some(1));
        assert_eq!(projected_root.latest_reply.as_deref(), Some("11.0"));
        let record = coordinator
            .thread_catalog
            .iter()
            .find(|record| record.key.channel_id == "C1" && record.key.root_ts == "10.0")
            .unwrap();
        assert_eq!(record.reply_count, 1);
        assert_eq!(record.latest_reply.as_deref(), Some("11.0"));
    }

    #[test]
    fn reply_mutation_updates_loaded_thread_root_without_channel_root() {
        let mut root = message("10.0", "thread-only root");
        root.reply_count = Some(0);
        root.reply_users = Some(Vec::new());
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply_from(
            MutationOrigin::Cache,
            WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".to_string(),
                thread_ts: "10.0".to_string(),
                snapshot: SnapshotEnvelope::new(
                    WorkspaceRevision::INITIAL,
                    MessagePage {
                        messages: vec![root],
                        complete: true,
                        ..Default::default()
                    },
                ),
            },
        );
        assert!(coordinator.history("C1").is_empty());

        let mut reply = message("11.0", "reply");
        reply.thread_ts = Some("10.0".to_string());
        reply.user = Some("U2".to_string());
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: reply,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        let thread_root = coordinator
            .threads
            .get(&("C1".to_string(), "10.0".to_string()))
            .unwrap()
            .messages()
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        assert_eq!(thread_root.reply_count, Some(1));
        assert_eq!(thread_root.latest_reply.as_deref(), Some("11.0"));
        let projection_changes = reduction
            .patch()
            .changes()
            .iter()
            .filter(|change| {
                matches!(
                    change,
                    WorkspaceChange::TimelineChanged { .. }
                        | WorkspaceChange::ThreadCatalogChanged(_)
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        assert!(matches!(
            projection_changes.as_slice(),
            [
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread { .. },
                    ..
                },
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread { .. },
                    changes,
                },
                WorkspaceChange::ThreadCatalogChanged(_),
            ] if matches!(
                changes.as_slice(),
                [MessageChange::Upsert(root)]
                    if root.ts == "10.0" && root.reply_count == Some(1)
            )
        ));
    }

    #[test]
    fn catalog_only_reply_move_updates_both_root_projections() {
        let mut first_root = message("10.0", "first root");
        first_root.reply_count = Some(1);
        first_root.latest_reply = Some("11.0".to_string());
        first_root.reply_users = Some(vec!["U2".to_string()]);
        let mut second_root = message("20.0", "second root");
        second_root.reply_count = Some(0);
        second_root.reply_users = Some(Vec::new());
        let mut previous = message("11.0", "first reply");
        previous.thread_ts = Some("10.0".to_string());
        previous.user = Some("U2".to_string());
        let mut catalog = ThreadCatalog::default();
        catalog.observe_thread("C1", "10.0", &[first_root.clone(), previous.clone()], true);
        catalog.observe_thread("C1", "20.0", std::slice::from_ref(&second_root), true);

        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            histories: HashMap::from([("C1".to_string(), vec![first_root, second_root])]),
            threads: catalog.into_records(),
            ..Default::default()
        }));

        let moved = SlackMessage {
            thread_ts: Some("20.0".to_string()),
            text: Some("moved reply".to_string()),
            ..previous
        };
        coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: moved,
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        let roots = coordinator
            .history("C1")
            .into_iter()
            .filter(|message| matches!(message.ts.as_str(), "10.0" | "20.0"))
            .map(|message| (message.ts.clone(), message))
            .collect::<HashMap<_, _>>();
        assert_eq!(roots["10.0"].reply_count, Some(0));
        assert_eq!(roots["10.0"].latest_reply, None);
        assert_eq!(roots["20.0"].reply_count, Some(1));
        assert_eq!(roots["20.0"].latest_reply.as_deref(), Some("11.0"));
        let records = coordinator
            .thread_catalog
            .iter()
            .map(|record| (record.key.root_ts.clone(), record))
            .collect::<HashMap<_, _>>();
        assert_eq!(records["10.0"].reply_count, 0);
        assert_eq!(records["20.0"].reply_count, 1);
    }

    #[test]
    fn root_metadata_only_reply_edit_does_not_increment_catalog_aggregate() {
        let mut root = message("10.0", "root");
        root.reply_count = Some(2);
        root.latest_reply = Some("12.0".to_string());
        root.reply_users = Some(vec!["U2".to_string(), "U3".to_string()]);
        let mut catalog = ThreadCatalog::default();
        catalog.observe_history("C1", std::slice::from_ref(&root));

        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            histories: HashMap::from([("C1".to_string(), vec![root])]),
            threads: catalog.into_records(),
            ..Default::default()
        }));

        let mut edited_reply = message("11.0", "edited reply");
        edited_reply.thread_ts = Some("10.0".to_string());
        edited_reply.user = Some("U2".to_string());
        coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: edited_reply,
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();

        let projected_root = coordinator
            .history("C1")
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        assert_eq!(projected_root.reply_count, Some(2));
        assert_eq!(projected_root.latest_reply.as_deref(), Some("12.0"));
        let record = coordinator
            .thread_catalog
            .iter()
            .find(|record| record.key.root_ts == "10.0")
            .unwrap();
        assert_eq!(record.reply_count, 2);
        assert_eq!(record.latest_reply.as_deref(), Some("12.0"));
        assert!(record.participant_user_ids.contains("U2"));
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
    fn duplicate_reply_delete_is_catalog_and_root_idempotent() {
        let mut root = message("10.0", "root");
        root.reply_count = Some(2);
        root.latest_reply = Some("12.0".to_string());
        root.reply_users = Some(vec!["U2".to_string(), "U3".to_string()]);
        root.subscribed = Some(true);
        root.last_read = Some("10.0".to_string());
        root.unread_count = Some(2);
        let mut first_reply = message("11.0", "first reply");
        first_reply.thread_ts = Some("10.0".to_string());
        first_reply.user = Some("U2".to_string());
        let mut deleted_reply = message("12.0", "deleted reply");
        deleted_reply.thread_ts = Some("10.0".to_string());
        deleted_reply.user = Some("U3".to_string());
        let mut catalog = ThreadCatalog::default();
        catalog.observe_thread(
            "C1",
            "10.0",
            &[root.clone(), first_reply.clone(), deleted_reply.clone()],
            true,
        );

        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            histories: HashMap::from([("C1".to_string(), vec![root.clone()])]),
            threads: catalog.into_records(),
            ..Default::default()
        }));
        coordinator.apply_from(
            MutationOrigin::Cache,
            WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".to_string(),
                thread_ts: "10.0".to_string(),
                snapshot: SnapshotEnvelope::new(
                    coordinator.revision(),
                    MessagePage {
                        messages: vec![root, first_reply, deleted_reply.clone()],
                        complete: true,
                        ..Default::default()
                    },
                ),
            },
        );

        coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: deleted_reply.clone(),
                kind: MessageMutationKind::Deleted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        assert_eq!(
            coordinator
                .thread_catalog
                .iter()
                .find(|record| record.key.root_ts == "10.0")
                .unwrap()
                .unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("10.0".to_string()),
            }
        );

        assert!(
            coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "C1".to_string(),
                    message: deleted_reply,
                    kind: MessageMutationKind::Deleted,
                    origin: MutationOrigin::Realtime,
                })
                .is_none(),
            "duplicate delete must not emit another catalog replacement"
        );
        let projected_root = coordinator
            .history("C1")
            .into_iter()
            .find(|message| message.ts == "10.0")
            .unwrap();
        assert_eq!(projected_root.reply_count, Some(1));
        assert_eq!(projected_root.latest_reply.as_deref(), Some("11.0"));
        let record = coordinator
            .thread_catalog
            .iter()
            .find(|record| record.key.root_ts == "10.0")
            .unwrap();
        assert_eq!(record.reply_count, 1);
        assert_eq!(record.latest_reply.as_deref(), Some("11.0"));
        assert_eq!(
            record.unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("10.0".to_string()),
            }
        );
    }

    #[test]
    fn unhydrated_self_authored_removal_keeps_unread_and_participation() {
        for moved in [false, true] {
            let mut first_root = message("10.0", "first root");
            first_root.reply_count = Some(2);
            first_root.latest_reply = Some("12.0".to_string());
            first_root.reply_users = Some(vec!["U_OTHER".to_string()]);
            first_root.subscribed = Some(true);
            first_root.last_read = Some("10.0".to_string());
            first_root.unread_count = Some(1);
            let mut second_root = message("20.0", "second root");
            second_root.reply_count = Some(0);
            second_root.reply_users = Some(Vec::new());
            second_root.subscribed = Some(true);
            second_root.last_read = Some("10.0".to_string());
            second_root.unread_count = Some(0);
            let mut catalog = ThreadCatalog::default();
            catalog.observe_history("C1", &[first_root.clone(), second_root.clone()]);

            let mut coordinator = WorkspaceCoordinator::default();
            coordinator.apply(WorkspaceMutation::AttentionContextChanged(
                WorkspaceAttentionContext {
                    current_user_id: Some("U_SELF".to_string()),
                },
            ));
            coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                histories: HashMap::from([("C1".to_string(), vec![first_root, second_root])]),
                threads: catalog.into_records(),
                ..Default::default()
            }));

            let mut self_reply = message("12.0", "self reply");
            self_reply.thread_ts = Some(if moved { "20.0" } else { "10.0" }.to_string());
            self_reply.user = Some("U_SELF".to_string());

            coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "C1".to_string(),
                    message: self_reply,
                    kind: if moved {
                        MessageMutationKind::Changed
                    } else {
                        MessageMutationKind::Deleted
                    },
                    origin: MutationOrigin::Realtime,
                })
                .unwrap();

            let record = coordinator
                .thread_catalog
                .iter()
                .find(|record| record.key.root_ts == "10.0")
                .unwrap();
            assert_eq!(
                record.unread,
                ThreadUnreadState::Known {
                    count: 1,
                    last_read: Some("10.0".to_string()),
                }
            );
            assert!(record.participant_user_ids.contains("U_SELF"));
        }
    }

    #[test]
    fn older_posted_reply_increments_count_and_updates_loaded_root_copies() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::AttentionContextChanged(
            WorkspaceAttentionContext {
                current_user_id: Some("U1".to_string()),
            },
        ));
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
                WorkspaceChange::ThreadCatalogChanged(_),
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
        coordinator.apply(WorkspaceMutation::AttentionContextChanged(
            WorkspaceAttentionContext {
                current_user_id: Some("U1".to_string()),
            },
        ));
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
            reduction
                .patch()
                .changes()
                .iter()
                .rev()
                .find(|change| matches!(
                    change,
                    WorkspaceChange::TimelineChanged {
                        target: TimelineTarget::Thread { .. },
                        ..
                    }
                )),
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
    fn realtime_root_edit_and_delete_refresh_then_clear_catalog_projection() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut root = message("10.0", "original root");
        root.reply_count = Some(1);
        root.latest_reply = Some("11.0".to_string());
        root.reply_users = Some(vec!["U2".to_string()]);
        let mut reply = message("11.0", "reply");
        reply.thread_ts = Some("10.0".to_string());
        reply.user = Some("U2".to_string());
        let mut catalog = ThreadCatalog::default();
        catalog.observe_thread("C1", "10.0", &[root.clone(), reply.clone()], true);
        coordinator.apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
            histories: HashMap::from([("C1".to_string(), vec![root.clone()])]),
            threads: catalog.into_records(),
            ..Default::default()
        }));
        coordinator.apply_from(
            MutationOrigin::Cache,
            WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".to_string(),
                thread_ts: "10.0".to_string(),
                snapshot: SnapshotEnvelope::new(
                    coordinator.revision(),
                    MessagePage {
                        messages: vec![root.clone(), reply],
                        complete: true,
                        ..Default::default()
                    },
                ),
            },
        );

        let edited = SlackMessage {
            text: Some("edited root".to_string()),
            reply_count: None,
            latest_reply: None,
            reply_users: None,
            ..root.clone()
        };
        let edit = coordinator
            .apply_from(
                MutationOrigin::Realtime,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C1".to_string(),
                    message: edited.clone(),
                    kind: MessageMutationKind::Changed,
                    origin: MutationOrigin::Realtime,
                },
            )
            .unwrap();
        let edited_records = edit
            .patch()
            .changes()
            .iter()
            .find_map(|change| match change {
                WorkspaceChange::ThreadCatalogChanged(records) => Some(records),
                _ => None,
            })
            .expect("root edit did not update the catalog projection");
        assert_eq!(
            edited_records[0]
                .root
                .as_ref()
                .and_then(|root| root.text.as_deref()),
            Some("edited root")
        );
        assert!(edit
            .store_batch()
            .unwrap()
            .changes()
            .iter()
            .any(|change| matches!(change, StoreChange::ThreadCatalogReplaced(_))));

        let deleted = coordinator
            .apply_from(
                MutationOrigin::Realtime,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C1".to_string(),
                    message: edited,
                    kind: MessageMutationKind::Deleted,
                    origin: MutationOrigin::Realtime,
                },
            )
            .unwrap();
        let deleted_records = deleted
            .patch()
            .changes()
            .iter()
            .find_map(|change| match change {
                WorkspaceChange::ThreadCatalogChanged(records) => Some(records),
                _ => None,
            })
            .expect("root delete did not clear the catalog projection");
        let record = &deleted_records[0];
        assert_eq!(record.root, None);
        assert_eq!(record.reply_count, 1);
        assert_eq!(record.latest_reply.as_deref(), Some("11.0"));
        assert!(record.participant_user_ids.contains("U2"));
        assert!(ThreadCatalog::from_records(deleted_records.clone())
            .inbox_projection(Vec::new())
            .is_empty());
        assert!(coordinator
            .history("C1")
            .iter()
            .all(|message| message.ts != "10.0"));
        assert!(deleted
            .store_batch()
            .unwrap()
            .changes()
            .iter()
            .any(|change| matches!(change, StoreChange::ThreadCatalogReplaced(_))));
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
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [
                StoreChange::ThreadReplaced { messages, .. },
                StoreChange::ThreadCatalogReplaced(_),
            ]
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
                WorkspaceChange::ThreadCatalogChanged(_),
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

    #[test]
    fn reaction_mutation_updates_broadcast_projections_once() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut broadcast = message("11.0", "broadcast");
        broadcast.thread_ts = Some("10.0".to_string());
        broadcast.subtype = Some("thread_broadcast".to_string());
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".to_string(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![broadcast.clone()],
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
                    messages: vec![broadcast],
                    complete: true,
                    ..Default::default()
                },
            ),
        });

        let change = ReactionMutation {
            channel_id: "C1".to_string(),
            message_ts: "11.0".to_string(),
            name: "wave".to_string(),
            user_id: "U1".to_string(),
            added: true,
        };
        let reduction = coordinator
            .apply_from(
                MutationOrigin::Local,
                WorkspaceMutation::ReactionChanged(change.clone()),
            )
            .expect("the first local confirmation must update both projections");

        assert_eq!(
            reduction.patch().revision(),
            reduction.store_batch().unwrap().revision()
        );
        assert!(matches!(
            reduction.patch().changes(),
            [
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Channel(channel_id),
                    changes: channel_changes,
                },
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread {
                        channel_id: thread_channel_id,
                        thread_ts,
                    },
                    changes: thread_changes,
                },
            ] if channel_id == "C1"
                && thread_channel_id == "C1"
                && thread_ts == "10.0"
                && channel_changes.iter().all(message_change_has_one_wave)
                && thread_changes.iter().all(message_change_has_one_wave)
        ));
        assert!(matches!(
            reduction.store_batch().unwrap().changes(),
            [
                StoreChange::ReactionActorStatesReplaced(_),
                StoreChange::ReactionChanged(stored),
            ]
                if stored.change == change
                    && stored.count == ReactionProjectionCount::Authoritative(1)
        ));

        let revision = coordinator.revision();
        assert!(coordinator
            .apply_from(
                MutationOrigin::Realtime,
                WorkspaceMutation::ReactionChanged(change),
            )
            .is_none());
        assert_eq!(
            coordinator.revision(),
            revision,
            "the realtime echo must not create a second patch or store batch"
        );
    }

    #[test]
    fn reaction_mutation_routes_thread_replies_and_retains_unknown_identities() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut reply = message("11.0", "reply");
        reply.thread_ts = Some("10.0".to_string());
        coordinator.apply(WorkspaceMutation::ThreadSnapshot {
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
        });

        let added = ReactionMutation {
            channel_id: "C1".to_string(),
            message_ts: "11.0".to_string(),
            name: "wave".to_string(),
            user_id: "U1".to_string(),
            added: true,
        };
        let reduction = coordinator
            .apply(WorkspaceMutation::ReactionChanged(added.clone()))
            .expect("known reply must change");
        assert!(matches!(
            reduction.patch().changes(),
            [WorkspaceChange::TimelineChanged {
                target: TimelineTarget::Thread {
                    channel_id,
                    thread_ts,
                },
                changes,
            }] if channel_id == "C1"
                && thread_ts == "10.0"
                && changes.iter().all(message_change_has_one_wave)
        ));
        assert!(coordinator.history("C1").is_empty());

        let removed = ReactionMutation {
            added: false,
            ..added
        };
        assert!(coordinator
            .apply(WorkspaceMutation::ReactionChanged(removed.clone()))
            .is_some());
        let revision = coordinator.revision();
        assert!(coordinator
            .apply(WorkspaceMutation::ReactionChanged(removed.clone()))
            .is_none());
        assert_eq!(coordinator.revision(), revision);

        for retained in [
            ReactionMutation {
                message_ts: "unknown".to_string(),
                added: true,
                ..removed.clone()
            },
            ReactionMutation {
                channel_id: "C2".to_string(),
                added: true,
                ..removed.clone()
            },
        ] {
            let before = coordinator.revision();
            let reduction = coordinator
                .apply(WorkspaceMutation::ReactionChanged(retained))
                .expect("unknown reaction identities still require a durable actor-state commit");
            assert!(reduction.patch().changes().is_empty());
            assert!(matches!(
                reduction.store_batch().unwrap().changes(),
                [
                    StoreChange::ReactionActorStatesReplaced(_),
                    StoreChange::ReactionChanged(ReactionProjectionMutation {
                        count: ReactionProjectionCount::Delta(1),
                        ..
                    }),
                ]
            ));
            assert!(coordinator.revision() > before);
        }

        let revision = coordinator.revision();
        for malformed in [
            ReactionMutation {
                name: String::new(),
                added: true,
                ..removed.clone()
            },
            ReactionMutation {
                user_id: String::new(),
                added: true,
                ..removed
            },
        ] {
            assert!(coordinator
                .apply(WorkspaceMutation::ReactionChanged(malformed))
                .is_none());
        }
        assert_eq!(coordinator.revision(), revision);
    }

    #[test]
    fn reaction_removal_uses_authoritative_count_when_actor_is_omitted() {
        for users in [Some(vec!["U_OTHER".to_string()]), None] {
            let mut coordinator = WorkspaceCoordinator::default();
            let mut reacted = message("11.0", "reacted");
            reacted.reactions = Some(vec![SlackReaction {
                name: Some("wave".into()),
                count: Some(3),
                users,
            }]);
            coordinator.apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".into(),
                snapshot: SnapshotEnvelope::new(
                    WorkspaceRevision::INITIAL,
                    MessagePage {
                        messages: vec![reacted],
                        complete: true,
                        ..Default::default()
                    },
                ),
            });

            let removal = ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "11.0".into(),
                name: "wave".into(),
                user_id: "U_OMITTED".into(),
                added: false,
            };
            coordinator
                .apply(WorkspaceMutation::ReactionChanged(removal.clone()))
                .expect("an authoritative removal must not depend on Slack's partial user list");
            let history = coordinator.history("C1");
            let reaction = &history[0].reactions.as_ref().unwrap()[0];
            assert_eq!(reaction.count, Some(2));
            assert!(!reaction
                .users
                .as_ref()
                .is_some_and(|users| users.iter().any(|user| user == "U_OMITTED")));

            let revision = coordinator.revision();
            assert!(coordinator
                .apply_from(
                    MutationOrigin::Realtime,
                    WorkspaceMutation::ReactionChanged(removal),
                )
                .is_none());
            assert_eq!(
                coordinator.revision(),
                revision,
                "a duplicate removal must not decrement the authoritative count twice"
            );
        }
    }

    #[test]
    fn omitted_actor_add_remove_add_cycle_is_idempotent() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut reacted = message("11.0", "reacted");
        reacted.reactions = Some(vec![SlackReaction {
            name: Some("wave".into()),
            count: Some(3),
            users: Some(vec!["U_OTHER".into()]),
        }]);
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".into(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![reacted],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        let mutation = |added| ReactionMutation {
            channel_id: "C1".into(),
            message_ts: "11.0".into(),
            name: "wave".into(),
            user_id: "U_OMITTED".into(),
            added,
        };

        for (added, expected_count) in [(true, 4), (false, 3), (true, 4)] {
            coordinator
                .apply(WorkspaceMutation::ReactionChanged(mutation(added)))
                .expect("each actual actor transition must update the projection once");
            assert_eq!(
                coordinator.history("C1")[0].reactions.as_ref().unwrap()[0].count,
                Some(expected_count)
            );
            let revision = coordinator.revision();
            assert!(coordinator
                .apply_from(
                    MutationOrigin::Realtime,
                    WorkspaceMutation::ReactionChanged(mutation(added)),
                )
                .is_none());
            assert_eq!(coordinator.revision(), revision);
        }
    }

    #[test]
    fn fresh_snapshot_retires_older_reaction_actor_authority() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".into(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![message("11.0", "message")],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "11.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            }))
            .expect("the reaction must be applied before the fresh snapshot");

        let fresh_base = coordinator.revision();
        coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".into(),
                snapshot: SnapshotEnvelope::new(
                    fresh_base,
                    MessagePage {
                        messages: vec![message("11.0", "message")],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .expect("a fresh authoritative snapshot must retire the older reaction");

        assert!(
            coordinator.history("C1")[0].reactions.is_none(),
            "retired actor authority must not recreate a reaction missing from a fresh snapshot"
        );
    }

    #[test]
    fn fresh_snapshot_explicit_actor_supersedes_an_older_removal() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut reacted = message("11.0", "message");
        reacted.reactions = Some(vec![SlackReaction {
            name: Some("wave".into()),
            count: Some(1),
            users: None,
        }]);
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".into(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![reacted],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "11.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: false,
            }))
            .expect("the removal must be applied before the fresh snapshot");

        let fresh_base = coordinator.revision();
        let mut fresh = message("11.0", "message");
        fresh.reactions = Some(vec![SlackReaction {
            name: Some("wave".into()),
            count: Some(1),
            users: Some(vec!["U1".into()]),
        }]);
        coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".into(),
                snapshot: SnapshotEnvelope::new(
                    fresh_base,
                    MessagePage {
                        messages: vec![fresh],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .expect("the fresh actor membership must supersede the older removal");

        assert!(message_has_reaction(
            &coordinator.history("C1")[0],
            "wave",
            1,
            "U1"
        ));
        assert!(coordinator
            .apply(WorkspaceMutation::ReactionChanged(ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "11.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: false,
            }))
            .is_some());
    }

    #[test]
    fn fresh_partial_user_snapshot_preserves_omitted_actor_fact_without_inflating_count() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".into(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![message("11.0", "message")],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        let actor = ReactionMutation {
            channel_id: "C1".into(),
            message_ts: "11.0".into(),
            name: "wave".into(),
            user_id: "U1".into(),
            added: true,
        };
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(actor.clone()))
            .expect("the actor add must be recorded");

        let fresh_base = coordinator.revision();
        let mut fresh = message("11.0", "message");
        fresh.reactions = Some(vec![SlackReaction {
            name: Some("wave".into()),
            count: Some(1),
            users: Some(vec!["U_OTHER".into()]),
        }]);
        coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".into(),
                snapshot: SnapshotEnvelope::new(
                    fresh_base,
                    MessagePage {
                        messages: vec![fresh],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .expect("the fresh count and partial users must be reconciled");

        assert!(message_has_reaction(
            &coordinator.history("C1")[0],
            "wave",
            1,
            "U1"
        ));
        assert!(
            coordinator
                .apply(WorkspaceMutation::ReactionChanged(actor))
                .is_none(),
            "a preserved actor fact must still suppress its replay"
        );
    }

    #[test]
    fn fresh_explicit_users_do_not_inflate_the_authoritative_count() {
        let mut coordinator = WorkspaceCoordinator::default();
        let removal = |user_id: &str| ReactionMutation {
            channel_id: "C1".into(),
            message_ts: "11.0".into(),
            name: "wave".into(),
            user_id: user_id.into(),
            added: false,
        };
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(removal("U1")))
            .unwrap();
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(removal("U2")))
            .unwrap();
        let fresh_base = coordinator.revision();
        let mut fresh = message("11.0", "message");
        fresh.reactions = Some(vec![SlackReaction {
            name: Some("wave".into()),
            count: Some(1),
            users: Some(vec!["U1".into(), "U2".into()]),
        }]);

        coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".into(),
                    snapshot: SnapshotEnvelope::new(
                        fresh_base,
                        MessagePage {
                            messages: vec![fresh],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .unwrap();
        let history = coordinator.history("C1");
        let reaction = &history[0].reactions.as_ref().unwrap()[0];
        assert_eq!(reaction.count, Some(1));
        assert_eq!(
            reaction.users.as_ref().map(Vec::len),
            Some(2),
            "Slack's partial user evidence may be longer than its authoritative count"
        );
    }

    #[test]
    fn zero_count_snapshot_retires_old_actor_fact_from_the_durable_ledger() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".into(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![message("11.0", "message")],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        let removal = ReactionMutation {
            channel_id: "C1".into(),
            message_ts: "11.0".into(),
            name: "wave".into(),
            user_id: "U1".into(),
            added: false,
        };
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(removal.clone()))
            .expect("the first removal must create a durable actor fact");
        let fresh_base = coordinator.revision();

        coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".into(),
                    snapshot: SnapshotEnvelope::new(
                        fresh_base,
                        MessagePage {
                            messages: vec![message("11.0", "message")],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .expect("the authoritative zero count must retire the old actor fact");
        assert!(coordinator.reaction_actor_state_records().is_empty());
        assert!(
            coordinator
                .apply(WorkspaceMutation::ReactionChanged(removal))
                .is_some(),
            "a replay after authoritative retirement must be able to self-correct"
        );
    }

    #[test]
    fn rejected_snapshot_does_not_reconcile_reaction_authority() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".into(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![message("11.0", "message")],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        let actor = ReactionMutation {
            channel_id: "C1".into(),
            message_ts: "11.0".into(),
            name: "wave".into(),
            user_id: "U1".into(),
            added: true,
        };
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(actor.clone()))
            .expect("the reaction must be accepted");
        let stale_base = coordinator.revision();
        coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: message("11.0", "newer edit"),
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .expect("the edit must supersede the pending snapshot");

        assert!(
            coordinator
                .apply_from(
                    MutationOrigin::WebApi,
                    WorkspaceMutation::HistorySnapshot {
                        channel_id: "C1".into(),
                        snapshot: SnapshotEnvelope::new(
                            stale_base,
                            MessagePage {
                                messages: vec![message("11.0", "stale")],
                                complete: true,
                                ..Default::default()
                            },
                        ),
                    },
                )
                .is_none(),
            "the stale canonical message must be rejected"
        );
        assert_eq!(
            coordinator.reaction_actor_state_records(),
            vec![actor.clone()]
        );
        assert!(
            coordinator
                .apply(WorkspaceMutation::ReactionChanged(actor))
                .is_none(),
            "a rejected snapshot must not make a duplicate add effective again"
        );
    }

    #[test]
    fn stale_zero_snapshot_preserves_a_newer_zero_delta_actor_fact() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut reacted = message("11.0", "message");
        reacted.reactions = Some(vec![SlackReaction {
            name: Some("wave".into()),
            count: Some(1),
            users: Some(vec!["U1".into()]),
        }]);
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".into(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![reacted],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        let stale_base = coordinator.revision();
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "11.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            }))
            .expect("the explicit event must establish actor idempotency");

        coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".into(),
                    snapshot: SnapshotEnvelope::new(
                        stale_base,
                        MessagePage {
                            messages: vec![message("11.0", "message")],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .expect("the accepted stale response must retain the post-base actor fact");
        assert!(message_has_reaction(
            &coordinator.history("C1")[0],
            "wave",
            1,
            "U1"
        ));
    }

    #[test]
    fn stale_snapshot_explicit_actor_does_not_reapply_its_newer_add_delta() {
        let mut coordinator = WorkspaceCoordinator::default();
        let request_base = coordinator.revision();
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "11.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            }))
            .expect("the post-request add must be retained");

        let mut response = message("11.0", "message");
        response.reactions = Some(vec![SlackReaction {
            name: Some("wave".into()),
            count: Some(1),
            users: Some(vec!["U1".into()]),
        }]);
        coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".into(),
                    snapshot: SnapshotEnvelope::new(
                        request_base,
                        MessagePage {
                            messages: vec![response],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .expect("the accepted response must materialize the reaction once");

        assert!(message_has_reaction(
            &coordinator.history("C1")[0],
            "wave",
            1,
            "U1"
        ));
    }

    #[test]
    fn snapshot_reconciliation_respects_mixed_actor_revisions() {
        let mut zero_count = WorkspaceCoordinator::default();
        let mutation = |user_id: &str, added| ReactionMutation {
            channel_id: "C1".into(),
            message_ts: "11.0".into(),
            name: "wave".into(),
            user_id: user_id.into(),
            added,
        };
        zero_count
            .apply(WorkspaceMutation::ReactionChanged(mutation("U_OLD", true)))
            .unwrap();
        let zero_base = zero_count.revision();
        zero_count
            .apply(WorkspaceMutation::ReactionChanged(mutation("U_NEW", true)))
            .unwrap();
        zero_count
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".into(),
                    snapshot: SnapshotEnvelope::new(
                        zero_base,
                        MessagePage {
                            messages: vec![message("11.0", "message")],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .unwrap();
        assert_eq!(
            zero_count.reaction_actor_state_records(),
            vec![mutation("U_NEW", true)]
        );
        assert!(message_has_reaction(
            &zero_count.history("C1")[0],
            "wave",
            1,
            "U_NEW"
        ));

        let mut explicit_users = WorkspaceCoordinator::default();
        explicit_users
            .apply(WorkspaceMutation::ReactionChanged(mutation("U_OLD", false)))
            .unwrap();
        let explicit_base = explicit_users.revision();
        explicit_users
            .apply(WorkspaceMutation::ReactionChanged(mutation("U_NEW", false)))
            .unwrap();
        let mut response = message("11.0", "message");
        response.reactions = Some(vec![SlackReaction {
            name: Some("wave".into()),
            count: Some(2),
            users: Some(vec!["U_OLD".into(), "U_NEW".into()]),
        }]);
        explicit_users
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".into(),
                    snapshot: SnapshotEnvelope::new(
                        explicit_base,
                        MessagePage {
                            messages: vec![response],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .unwrap();
        assert_eq!(
            explicit_users.reaction_actor_state_records(),
            vec![mutation("U_NEW", false), mutation("U_OLD", true)]
        );
        assert!(message_has_reaction(
            &explicit_users.history("C1")[0],
            "wave",
            1,
            "U_OLD"
        ));
        assert!(
            !explicit_users.history("C1")[0].reactions.as_ref().unwrap()[0]
                .users
                .as_ref()
                .unwrap()
                .iter()
                .any(|user| user == "U_NEW")
        );
    }

    #[test]
    fn accepted_snapshot_advances_reaction_authority_for_same_base_idempotence() {
        let mut coordinator = WorkspaceCoordinator::default();
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".into(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![message("11.0", "message")],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "11.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            }))
            .expect("the reaction must be accepted");
        let response_base = coordinator.revision();
        let exact_projection = coordinator.history("C1")[0].clone();

        coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".into(),
                    snapshot: SnapshotEnvelope::new(
                        response_base,
                        MessagePage {
                            messages: vec![exact_projection],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .expect("the first accepted response must establish snapshot authority");
        let accepted_revision = coordinator.revision();
        assert!(accepted_revision > response_base);

        assert!(
            coordinator
                .apply_from(
                    MutationOrigin::WebApi,
                    WorkspaceMutation::HistorySnapshot {
                        channel_id: "C1".into(),
                        snapshot: SnapshotEnvelope::new(
                            response_base,
                            MessagePage {
                                messages: vec![message("11.0", "message")],
                                complete: true,
                                ..Default::default()
                            },
                        ),
                    },
                )
                .is_none(),
            "a conflicting second response at the same base must be stale"
        );
        assert_eq!(coordinator.revision(), accepted_revision);
        assert!(message_has_reaction(
            &coordinator.history("C1")[0],
            "wave",
            1,
            "U1"
        ));
    }

    #[test]
    fn accepted_snapshot_revision_blocks_a_newer_base_started_before_its_commit() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut broadcast = message("11.0", "broadcast");
        broadcast.thread_ts = Some("10.0".into());
        broadcast.subtype = Some("thread_broadcast".into());
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".into(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![broadcast.clone()],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "11.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            }))
            .unwrap();
        let channel_response_base = coordinator.revision();
        coordinator
            .apply(WorkspaceMutation::UserUpsert(SlackUser {
                id: Some("U_UNRELATED".into()),
                name: Some("unrelated".into()),
                ..Default::default()
            }))
            .unwrap();
        let thread_response_base = coordinator.revision();

        coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::HistorySnapshot {
                    channel_id: "C1".into(),
                    snapshot: SnapshotEnvelope::new(
                        channel_response_base,
                        MessagePage {
                            messages: vec![broadcast.clone()],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .expect("the first accepted response must retire the covered reaction");
        let accepted_revision = coordinator.revision();
        assert!(accepted_revision > thread_response_base);

        let mut stale_thread = broadcast;
        stale_thread.reactions = Some(vec![SlackReaction {
            name: Some("wave".into()),
            count: Some(1),
            users: Some(vec!["U1".into()]),
        }]);
        coordinator
            .apply_from(
                MutationOrigin::WebApi,
                WorkspaceMutation::ThreadSnapshot {
                    channel_id: "C1".into(),
                    thread_ts: "10.0".into(),
                    snapshot: SnapshotEnvelope::new(
                        thread_response_base,
                        MessagePage {
                            messages: vec![stale_thread],
                            complete: true,
                            ..Default::default()
                        },
                    ),
                },
            )
            .expect("the missing thread target must still materialize");
        assert!(
            coordinator
                .threads
                .get(&("C1".to_string(), "10.0".to_string()))
                .unwrap()
                .messages()[0]
                .reactions
                .is_none(),
            "the later-arriving response predates the accepted snapshot commit"
        );
    }

    #[test]
    fn hydration_rebuilds_unknown_reaction_authority_for_later_projections() {
        let mut coordinator = WorkspaceCoordinator::default();
        let actor = ReactionMutation {
            channel_id: "C1".into(),
            message_ts: "11.0".into(),
            name: "wave".into(),
            user_id: "U1".into(),
            added: true,
        };
        coordinator
            .apply(WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                reaction_actor_states: vec![actor],
                ..Default::default()
            }))
            .expect("the durable actor fact must hydrate");
        let response_base = coordinator.revision();

        let mut broadcast = message("11.0", "broadcast");
        broadcast.thread_ts = Some("10.0".into());
        broadcast.subtype = Some("thread_broadcast".into());
        broadcast.reactions = Some(vec![SlackReaction {
            name: Some("wave".into()),
            count: Some(1),
            users: None,
        }]);
        coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".into(),
                snapshot: SnapshotEnvelope::new(
                    response_base,
                    MessagePage {
                        messages: vec![broadcast.clone()],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .expect("the hydrated fact must materialize in stale history");
        coordinator
            .apply(WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".into(),
                thread_ts: "10.0".into(),
                snapshot: SnapshotEnvelope::new(
                    response_base,
                    MessagePage {
                        messages: vec![broadcast],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .expect("the hydrated fact must materialize in a stale thread");

        let mut catalog = crate::thread_catalog::ThreadCatalog::default();
        let mut root = message("11.0", "root");
        root.reply_count = Some(1);
        root.reactions = Some(vec![SlackReaction {
            name: Some("wave".into()),
            count: Some(1),
            users: None,
        }]);
        catalog.observe_thread("C1", "11.0", std::slice::from_ref(&root), false);
        coordinator
            .apply(WorkspaceMutation::ThreadCatalogChanged(
                catalog.into_records(),
            ))
            .expect("the hydrated fact must materialize in the thread catalog");

        assert!(message_has_reaction(
            &coordinator.history("C1")[0],
            "wave",
            1,
            "U1"
        ));
        assert!(message_has_reaction(
            &coordinator
                .threads
                .get(&("C1".to_string(), "10.0".to_string()))
                .unwrap()
                .messages()[0],
            "wave",
            1,
            "U1"
        ));
        assert!(message_has_reaction(
            coordinator.thread_catalog[0].root.as_ref().unwrap(),
            "wave",
            1,
            "U1"
        ));
    }

    #[test]
    fn unknown_reaction_authority_merges_into_later_stale_projections_and_catalog() {
        let mut coordinator = WorkspaceCoordinator::default();
        let request_base = coordinator.revision();
        let reaction_reduction = coordinator
            .apply_from(
                MutationOrigin::Realtime,
                WorkspaceMutation::ReactionChanged(ReactionMutation {
                    channel_id: "C1".into(),
                    message_ts: "11.0".into(),
                    name: "wave".into(),
                    user_id: "U1".into(),
                    added: true,
                }),
            )
            .expect("the unknown reaction must persist its actor authority");
        assert!(reaction_reduction.patch().changes().is_empty());
        assert!(matches!(
            reaction_reduction.store_batch().unwrap().changes(),
            [
                StoreChange::ReactionActorStatesReplaced(_),
                StoreChange::ReactionChanged(ReactionProjectionMutation {
                    count: ReactionProjectionCount::Delta(1),
                    ..
                }),
            ]
        ));
        assert!(
            coordinator.revision() > request_base,
            "retained authority must participate in snapshot staleness"
        );

        let mut broadcast = message("11.0", "broadcast");
        broadcast.thread_ts = Some("10.0".into());
        broadcast.subtype = Some("thread_broadcast".into());
        coordinator
            .apply(WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".into(),
                snapshot: SnapshotEnvelope::new(
                    request_base,
                    MessagePage {
                        messages: vec![broadcast.clone()],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .expect("the stale history must materialize retained reaction authority");
        coordinator
            .apply(WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".into(),
                thread_ts: "10.0".into(),
                snapshot: SnapshotEnvelope::new(
                    request_base,
                    MessagePage {
                        messages: vec![broadcast.clone()],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .expect("the stale thread must materialize retained reaction authority");

        let mut catalog = crate::thread_catalog::ThreadCatalog::default();
        let mut root = message("11.0", "root");
        root.reply_count = Some(1);
        catalog.observe_thread("C1", "11.0", std::slice::from_ref(&root), false);
        coordinator
            .apply(WorkspaceMutation::ThreadCatalogChanged(
                catalog.into_records(),
            ))
            .expect("a later thread catalog must inherit retained reaction authority");

        assert!(message_has_reaction(
            &coordinator.history("C1")[0],
            "wave",
            1,
            "U1"
        ));
        assert!(message_has_reaction(
            &coordinator
                .threads
                .get(&("C1".to_string(), "10.0".to_string()))
                .unwrap()
                .messages()[0],
            "wave",
            1,
            "U1"
        ));
        assert!(message_has_reaction(
            coordinator.thread_catalog[0].root.as_ref().unwrap(),
            "wave",
            1,
            "U1"
        ));
    }

    #[test]
    fn stale_thread_snapshot_inherits_reaction_from_existing_broadcast_projection() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut broadcast = message("11.0", "broadcast");
        broadcast.thread_ts = Some("10.0".into());
        broadcast.subtype = Some("thread_broadcast".into());
        coordinator.apply(WorkspaceMutation::HistorySnapshot {
            channel_id: "C1".into(),
            snapshot: SnapshotEnvelope::new(
                WorkspaceRevision::INITIAL,
                MessagePage {
                    messages: vec![broadcast.clone()],
                    complete: true,
                    ..Default::default()
                },
            ),
        });
        let thread_request_base = coordinator.revision();
        coordinator
            .apply(WorkspaceMutation::ReactionChanged(ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "11.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            }))
            .unwrap();

        coordinator
            .apply(WorkspaceMutation::ThreadSnapshot {
                channel_id: "C1".into(),
                thread_ts: "10.0".into(),
                snapshot: SnapshotEnvelope::new(
                    thread_request_base,
                    MessagePage {
                        messages: vec![broadcast],
                        complete: true,
                        ..Default::default()
                    },
                ),
            })
            .expect("the absent broadcast projection must inherit newer reaction authority");
        let thread_message = &coordinator
            .threads
            .get(&("C1".to_string(), "10.0".to_string()))
            .unwrap()
            .messages()[0];
        assert!(message_has_reaction(thread_message, "wave", 1, "U1"));
    }

    #[test]
    fn reaction_mutation_updates_thread_catalog_root_without_loaded_timelines() {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut catalog = crate::thread_catalog::ThreadCatalog::default();
        let mut root = message("10.0", "root");
        root.reply_count = Some(1);
        catalog.observe_thread("C1", "10.0", std::slice::from_ref(&root), false);
        coordinator.apply(WorkspaceMutation::ThreadCatalogChanged(
            catalog.into_records(),
        ));

        let reduction = coordinator
            .apply(WorkspaceMutation::ReactionChanged(ReactionMutation {
                channel_id: "C1".into(),
                message_ts: "10.0".into(),
                name: "wave".into(),
                user_id: "U1".into(),
                added: true,
            }))
            .expect("the thread inbox root is a canonical reaction projection");
        assert!(matches!(
            reduction.patch().changes(),
            [WorkspaceChange::ThreadCatalogChanged(records)]
                if message_has_reaction(
                    records[0].root.as_ref().unwrap(),
                    "wave",
                    1,
                    "U1"
                )
        ));
    }

    fn message_change_has_one_wave(change: &MessageChange) -> bool {
        let MessageChange::Upsert(message) = change else {
            return false;
        };
        matches!(
            message.reactions.as_deref(),
            Some([reaction])
                if reaction.name.as_deref() == Some("wave")
                    && reaction.count == Some(1)
                    && reaction.users.as_deref() == Some(&["U1".to_string()][..])
        )
    }

    fn message_has_reaction(message: &SlackMessage, name: &str, count: u64, user_id: &str) -> bool {
        message.reactions.as_ref().is_some_and(|reactions| {
            reactions.iter().any(|reaction| {
                reaction.name.as_deref() == Some(name)
                    && reaction.count == Some(count)
                    && reaction
                        .users
                        .as_ref()
                        .is_some_and(|users| users.iter().any(|user| user == user_id))
            })
        })
    }
}
