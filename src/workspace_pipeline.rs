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

use std::collections::HashMap;

#[cfg(test)]
use crate::models::SlackUnreadState;
use crate::models::{
    slack_timestamp_is_after, SlackConversation, SlackConversationUnreadSnapshot, SlackMessage,
    SlackUser,
};
use crate::thread_catalog::ThreadRecord;

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
    Hydrate(WorkspaceBootstrapData),
    MembershipSnapshot(SnapshotEnvelope<Vec<SlackConversation>>),
    ConversationUpsert(SlackConversation),
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
    ThreadCatalogChanged(Vec<ThreadRecord>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
pub(crate) struct WorkspacePatch {
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
    ConversationUpsert(SlackConversation),
    ConversationRemoved {
        channel_id: String,
    },
    UnreadChanged {
        snapshot: SlackConversationUnreadSnapshot,
    },
    UsersReplaced(Vec<SlackUser>),
    UserUpsert(SlackUser),
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

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceReduction {
    patch: WorkspacePatch,
    store_batch: Option<StoreBatch>,
}

#[derive(Debug, Clone)]
struct RevisionedConversation {
    value: SlackConversation,
    membership_revision: WorkspaceRevision,
    metadata_revision: WorkspaceRevision,
    unread_revision: WorkspaceRevision,
}

#[derive(Debug, Clone)]
struct RevisionedValue<T> {
    value: T,
    revision: WorkspaceRevision,
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
}

/// Pure owner of one workspace's canonical domain model and global revision.
///
/// Runtime and GTK adapters are deliberately absent here. A mutation either changes the model
/// once and produces one revision-stamped reduction, or is a no-op that leaves the revision
/// untouched.
#[derive(Debug, Default)]
pub(crate) struct WorkspaceCoordinator {
    revision: WorkspaceRevision,
    conversations: HashMap<String, RevisionedConversation>,
    users: HashMap<String, RevisionedValue<SlackUser>>,
    histories: HashMap<String, TimelineState>,
    threads: HashMap<(String, String), TimelineState>,
    thread_catalog: Vec<ThreadRecord>,
}

impl WorkspaceCoordinator {
    pub(crate) fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    pub(crate) fn conversation(&self, channel_id: &str) -> Option<&SlackConversation> {
        self.conversations.get(channel_id).map(|entry| &entry.value)
    }

    pub(crate) fn history(&self, channel_id: &str) -> Vec<SlackMessage> {
        self.histories
            .get(channel_id)
            .map(TimelineState::messages)
            .unwrap_or_default()
    }

    pub(crate) fn apply(&mut self, mutation: WorkspaceMutation) -> Option<WorkspaceReduction> {
        match mutation {
            WorkspaceMutation::Hydrate(data) => self.apply_hydration(data),
            WorkspaceMutation::MembershipSnapshot(snapshot) => {
                self.apply_membership_snapshot(snapshot)
            }
            WorkspaceMutation::ConversationUpsert(conversation) => {
                self.apply_conversation_upsert(conversation)
            }
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
            WorkspaceMutation::UsersSnapshot(snapshot) => self.apply_users_snapshot(snapshot),
            WorkspaceMutation::UserUpsert(user) => self.apply_user_upsert(user),
            WorkspaceMutation::HistorySnapshot {
                channel_id,
                snapshot,
            } => self.apply_timeline_snapshot(TimelineTarget::Channel(channel_id), snapshot),
            WorkspaceMutation::HistoryPage { channel_id, page } => self.apply_timeline_snapshot(
                TimelineTarget::Channel(channel_id),
                SnapshotEnvelope::new(self.revision, page),
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
            ),
            WorkspaceMutation::MessageChanged {
                channel_id,
                message,
                kind,
                origin: _,
            } => self.apply_message(&channel_id, message, kind),
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
        let reduction = WorkspaceReduction::new(revision, patch_changes, store_changes)?;
        self.revision = revision;
        Some(reduction)
    }

    fn apply_hydration(&mut self, data: WorkspaceBootstrapData) -> Option<WorkspaceReduction> {
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
        self.thread_catalog = data.threads.clone();
        self.commit(
            revision,
            vec![WorkspaceChange::BootstrapReset(data.clone())],
            vec![StoreChange::BootstrapReplaced(data)],
        )
    }

    fn apply_conversation_upsert(
        &mut self,
        conversation: SlackConversation,
    ) -> Option<WorkspaceReduction> {
        if conversation.id.trim().is_empty() {
            return None;
        }
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
            vec![StoreChange::ConversationUpsert(current)],
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

    fn apply_membership_snapshot(
        &mut self,
        snapshot: SnapshotEnvelope<Vec<SlackConversation>>,
    ) -> Option<WorkspaceReduction> {
        let base_revision = snapshot.base_revision();
        let incoming = snapshot
            .into_data()
            .into_iter()
            .filter(|conversation| !conversation.id.trim().is_empty())
            .map(|conversation| (conversation.id.clone(), conversation))
            .collect::<HashMap<_, _>>();
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
                        store_changes.push(StoreChange::ConversationUpsert(merged));
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
                        },
                    );
                    patch_changes.push(WorkspaceChange::ConversationUpsert(conversation.clone()));
                    store_changes.push(StoreChange::ConversationUpsert(conversation.clone()));
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
            .and_then(|entry| entry.value.last_read_ts())
            .is_some_and(|current| {
                snapshot
                    .last_read
                    .as_deref()
                    .is_some_and(|incoming| slack_timestamp_is_after(current, incoming))
            })
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
            });
        let before = entry.value.clone();
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

    fn apply_timeline_snapshot(
        &mut self,
        target: TimelineTarget,
        snapshot: SnapshotEnvelope<MessagePage>,
    ) -> Option<WorkspaceReduction> {
        let base_revision = snapshot.base_revision();
        let page = snapshot.into_data();
        let revision = self.next_revision();
        let timeline = self.timeline_mut(&target);
        let incoming = page
            .messages
            .into_iter()
            .filter(|message| match &target {
                TimelineTarget::Channel(_) => message.belongs_in_channel_timeline(),
                TimelineTarget::Thread { thread_ts, .. } => message.belongs_to_thread(thread_ts),
            })
            .map(|message| (message.ts.clone(), message))
            .collect::<HashMap<_, _>>();
        let mut changes = Vec::new();
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
        let messages = timeline.messages();
        let store_change = store_timeline_replacement(&target, messages);
        self.commit(
            revision,
            vec![WorkspaceChange::TimelineChanged { target, changes }],
            vec![store_change],
        )
    }

    fn apply_message(
        &mut self,
        channel_id: &str,
        message: SlackMessage,
        kind: MessageMutationKind,
    ) -> Option<WorkspaceReduction> {
        if channel_id.trim().is_empty() || message.ts.trim().is_empty() {
            return None;
        }
        let mut targets = Vec::new();
        if message.belongs_in_channel_timeline() {
            targets.push(TimelineTarget::Channel(channel_id.to_string()));
        }
        if let Some(thread_ts) = message.thread_root_ts() {
            targets.push(TimelineTarget::Thread {
                channel_id: channel_id.to_string(),
                thread_ts: thread_ts.to_string(),
            });
        }
        if kind == MessageMutationKind::Posted
            && targets.iter().any(|target| {
                self.timeline(target)
                    .is_some_and(|timeline| timeline.contains_identity(&message))
            })
        {
            return None;
        }

        let reply_root = message.thread_root_ts().map(str::to_string);
        let reply_was_known = reply_root.as_deref().is_some_and(|thread_ts| {
            self.threads
                .get(&(channel_id.to_string(), thread_ts.to_string()))
                .is_some_and(|timeline| timeline.contains_identity(&message))
        });
        let revision = self.next_revision();
        let mut patch_changes = Vec::new();
        let mut changed_targets = Vec::new();
        for target in targets {
            let timeline = self.timeline_mut(&target);
            let changed = match kind {
                MessageMutationKind::Deleted => {
                    let already_deleted = timeline.tombstones.contains_key(&message.ts);
                    let removed = timeline.messages.remove(&message.ts).is_some();
                    if removed || !already_deleted {
                        timeline.tombstones.insert(message.ts.clone(), revision);
                    }
                    removed || !already_deleted
                }
                MessageMutationKind::Posted | MessageMutationKind::Changed => {
                    if timeline
                        .messages
                        .get(&message.ts)
                        .is_some_and(|entry| entry.value == message)
                    {
                        false
                    } else {
                        timeline.messages.insert(
                            message.ts.clone(),
                            RevisionedValue {
                                value: message.clone(),
                                revision,
                            },
                        );
                        timeline.tombstones.remove(&message.ts);
                        true
                    }
                }
            };
            if !changed {
                continue;
            }
            let message_change = match kind {
                MessageMutationKind::Deleted => MessageChange::Remove {
                    message_ts: message.ts.clone(),
                },
                _ => MessageChange::Upsert(Box::new(message.clone())),
            };
            patch_changes.push(WorkspaceChange::TimelineChanged {
                target: target.clone(),
                changes: vec![message_change],
            });
            changed_targets.push(target);
        }

        if let Some(root_ts) = reply_root {
            if let Some(root) = self.update_channel_root_for_reply(
                channel_id,
                &root_ts,
                &message,
                kind,
                reply_was_known,
                revision,
            ) {
                let channel_target = TimelineTarget::Channel(channel_id.to_string());
                patch_changes.push(WorkspaceChange::TimelineChanged {
                    target: channel_target.clone(),
                    changes: vec![MessageChange::Upsert(Box::new(root))],
                });
                if !changed_targets.contains(&channel_target) {
                    changed_targets.push(channel_target);
                }
            }
        }

        let store_changes = changed_targets
            .iter()
            .filter_map(|target| {
                self.timeline(target)
                    .map(|timeline| store_timeline_replacement(target, timeline.messages()))
            })
            .collect();
        self.commit(revision, patch_changes, store_changes)
    }

    fn update_channel_root_for_reply(
        &mut self,
        channel_id: &str,
        root_ts: &str,
        reply: &SlackMessage,
        kind: MessageMutationKind,
        reply_was_known: bool,
        revision: WorkspaceRevision,
    ) -> Option<SlackMessage> {
        let remaining_replies = self
            .threads
            .get(&(channel_id.to_string(), root_ts.to_string()))
            .map(TimelineState::messages)
            .unwrap_or_default();
        let root = self
            .histories
            .get_mut(channel_id)?
            .messages
            .get_mut(root_ts)?;
        let before = root.value.clone();

        match kind {
            MessageMutationKind::Posted => {
                let not_reflected = root
                    .value
                    .latest_reply
                    .as_deref()
                    .is_none_or(|latest| latest < reply.ts.as_str());
                if not_reflected {
                    root.value.reply_count =
                        Some(root.value.reply_count.unwrap_or_default().saturating_add(1));
                    root.value.latest_reply = Some(reply.ts.clone());
                }
                if let Some(user_id) = reply.user.as_deref() {
                    let users = root.value.reply_users.get_or_insert_with(Vec::new);
                    if !users.iter().any(|known| known == user_id) {
                        users.push(user_id.to_string());
                    }
                }
            }
            MessageMutationKind::Changed => {
                if let Some(user_id) = reply.user.as_deref() {
                    let users = root.value.reply_users.get_or_insert_with(Vec::new);
                    if !users.iter().any(|known| known == user_id) {
                        users.push(user_id.to_string());
                    }
                }
            }
            MessageMutationKind::Deleted => {
                let deletion_is_reflected = reply_was_known
                    || root.value.latest_reply.as_deref() == Some(reply.ts.as_str());
                if deletion_is_reflected {
                    root.value.reply_count =
                        Some(root.value.reply_count.unwrap_or_default().saturating_sub(1));
                }
                if root.value.latest_reply.as_deref() == Some(reply.ts.as_str()) {
                    root.value.latest_reply = remaining_replies
                        .iter()
                        .filter(|message| message.ts != root_ts)
                        .map(|message| message.ts.as_str())
                        .max()
                        .map(str::to_string);
                }
                if let Some(user_id) = reply.user.as_deref() {
                    let user_still_replied = remaining_replies
                        .iter()
                        .any(|message| message.user.as_deref() == Some(user_id));
                    if !user_still_replied {
                        if let Some(users) = root.value.reply_users.as_mut() {
                            users.retain(|known| known != user_id);
                        }
                    }
                }
            }
        }

        if root.value == before {
            return None;
        }
        root.revision = revision;
        Some(root.value.clone())
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

fn same_message_identity(left: &SlackMessage, right: &SlackMessage) -> bool {
    (!left.ts.trim().is_empty() && left.ts == right.ts)
        || left.client_msg_id.as_deref().is_some_and(|left_id| {
            !left_id.trim().is_empty() && right.client_msg_id.as_deref() == Some(left_id)
        })
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
    for (key, value) in &incoming.extra {
        if !key.to_ascii_lowercase().contains("unread") && key != "last_read" {
            current.extra.insert(key.clone(), value.clone());
        }
    }
}

impl WorkspaceReduction {
    pub(crate) fn new(
        revision: WorkspaceRevision,
        patch_changes: Vec<WorkspaceChange>,
        store_changes: Vec<StoreChange>,
    ) -> Option<Self> {
        let patch = WorkspacePatch::new(revision, patch_changes)?;
        let store_batch = StoreBatch::new(revision, store_changes);
        Some(Self { patch, store_batch })
    }

    pub(crate) fn patch(&self) -> &WorkspacePatch {
        &self.patch
    }

    pub(crate) fn store_batch(&self) -> Option<&StoreBatch> {
        self.store_batch.as_ref()
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
            SnapshotEnvelope::new(snapshot_revision, vec![stale]),
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
            2
        );
        assert_eq!(
            coordinator.conversation("C1").unwrap().latest_message_ts(),
            Some("30.0")
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
}
