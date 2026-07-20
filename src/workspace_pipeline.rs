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

use crate::models::{SlackConversation, SlackMessage, SlackUnreadState, SlackUser};
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

#[derive(Debug, Clone)]
pub(crate) enum WorkspaceMutation {
    Hydrate(WorkspaceBootstrapData),
    MembershipSnapshot(SnapshotEnvelope<Vec<SlackConversation>>),
    ConversationUpsert(SlackConversation),
    ConversationRemove {
        channel_id: String,
    },
    UnreadChanged {
        channel_id: String,
        state: SlackUnreadState,
        server_last_read: Option<String>,
    },
    ReadAdvanced {
        channel_id: String,
        ts: String,
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
    Upsert(SlackMessage),
    Remove { message_ts: String },
}

#[derive(Debug, Clone)]
pub(crate) enum WorkspaceChange {
    BootstrapReset(WorkspaceBootstrapData),
    ConversationsReset(Vec<SlackConversation>),
    ConversationUpsert(SlackConversation),
    ConversationRemoved {
        channel_id: String,
    },
    UnreadChanged {
        channel_id: String,
        state: SlackUnreadState,
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

#[derive(Debug, Clone)]
pub(crate) enum StoreChange {
    BootstrapReplaced(WorkspaceBootstrapData),
    ConversationsReplaced(Vec<SlackConversation>),
    ConversationUpsert(SlackConversation),
    ConversationRemoved {
        channel_id: String,
    },
    UnreadChanged {
        channel_id: String,
        state: SlackUnreadState,
        server_last_read: Option<String>,
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

    #[test]
    fn revision_advances_monotonically() {
        let initial = WorkspaceRevision::INITIAL;
        let first = initial.successor();
        let second = first.successor();

        assert_eq!(initial.value(), 0);
        assert_eq!(first.value(), 1);
        assert_eq!(second.value(), 2);
        assert!(second > first);
    }

    #[test]
    fn snapshot_envelope_retains_and_classifies_its_base_revision() {
        let base_revision = WorkspaceRevision::INITIAL.successor();
        let snapshot = SnapshotEnvelope::new(base_revision, vec!["cached"]);

        assert_eq!(snapshot.base_revision(), base_revision);
        assert_eq!(snapshot.data(), &["cached"]);
        assert!(!snapshot.is_stale_at(base_revision));
        assert!(snapshot.is_stale_at(base_revision.successor()));
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
    fn store_batch_preserves_reducer_change_order() {
        let revision = WorkspaceRevision::INITIAL.successor();
        let batch = StoreBatch::new(
            revision,
            vec![
                StoreChange::ConversationRemoved {
                    channel_id: "C1".to_string(),
                },
                StoreChange::HistoryRemoved {
                    channel_id: "C1".to_string(),
                },
            ],
        )
        .unwrap();

        assert!(matches!(
            batch.changes(),
            [
                StoreChange::ConversationRemoved { .. },
                StoreChange::HistoryRemoved { .. }
            ]
        ));
    }
}
