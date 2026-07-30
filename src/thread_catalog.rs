use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::models::{slack_timestamp_is_after, SlackMessage};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ThreadKey {
    pub(crate) channel_id: String,
    pub(crate) root_ts: String,
}

impl ThreadKey {
    pub(crate) fn new(channel_id: &str, root_ts: &str) -> Option<Self> {
        let channel_id = channel_id.trim();
        let root_ts = root_ts.trim();
        (!channel_id.is_empty() && !root_ts.is_empty()).then(|| Self {
            channel_id: channel_id.to_string(),
            root_ts: root_ts.to_string(),
        })
    }
}

/// A partial history response must never be mistaken for proof that a thread
/// has no unread replies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ThreadUnreadState {
    #[default]
    Unknown,
    Known {
        count: u64,
        last_read: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadCatalogMessageKind {
    Posted,
    Changed,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadRecord {
    pub(crate) key: ThreadKey,
    pub(crate) root: Option<SlackMessage>,
    pub(crate) reply_count: u64,
    pub(crate) latest_reply: Option<String>,
    /// `None` means Slack has not supplied subscription metadata yet.
    pub(crate) subscribed: Option<bool>,
    pub(crate) unread: ThreadUnreadState,
    /// Reply authors are append-only: deleting a reply does not erase the
    /// fact that its author previously participated in the thread.
    #[serde(default)]
    pub(crate) participant_user_ids: HashSet<String>,
    #[serde(default)]
    seen_reply_ts: HashSet<String>,
    /// Reply identities removed by realtime deletion or an authoritative
    /// complete thread response. These remain durable across restarts and are
    /// cleared only when a complete canonical thread explicitly contains the
    /// same identity again.
    #[serde(default)]
    deleted_reply_ts: HashSet<String>,
    /// Exact locally observed reply identities that contributed to the
    /// aggregate unread count. Older records deserialize safely without them.
    #[serde(default)]
    unread_reply_ts: HashSet<String>,
}

impl ThreadRecord {
    fn placeholder(key: ThreadKey) -> Self {
        Self {
            key,
            root: None,
            reply_count: 0,
            latest_reply: None,
            subscribed: None,
            unread: ThreadUnreadState::Unknown,
            participant_user_ids: HashSet::new(),
            seen_reply_ts: HashSet::new(),
            deleted_reply_ts: HashSet::new(),
            unread_reply_ts: HashSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_known_subscribed(&self) -> bool {
        self.subscribed == Some(true)
    }

    pub(crate) fn reply_timestamp_for_identity(
        &self,
        identity_timestamps: &HashSet<String>,
    ) -> Option<&str> {
        self.seen_reply_ts
            .iter()
            .chain(self.unread_reply_ts.iter())
            .find(|reply_ts| identity_timestamps.contains(*reply_ts))
            .or_else(|| {
                self.latest_reply
                    .as_ref()
                    .filter(|reply_ts| identity_timestamps.contains(*reply_ts))
            })
            .map(String::as_str)
    }

    pub(crate) fn has_deleted_reply_identity(&self, identity_timestamps: &HashSet<String>) -> bool {
        self.deleted_reply_ts
            .iter()
            .any(|reply_ts| identity_timestamps.contains(reply_ts))
    }

    pub(crate) fn has_deleted_replies(&self) -> bool {
        !self.deleted_reply_ts.is_empty()
    }

    pub(crate) fn latest_reply_excluding(
        &self,
        identity_timestamps: &HashSet<String>,
    ) -> Option<&str> {
        self.seen_reply_ts
            .iter()
            .filter(|reply_ts| !identity_timestamps.contains(*reply_ts))
            .max()
            .map(String::as_str)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ThreadCatalog {
    records: HashMap<ThreadKey, ThreadRecord>,
}

impl ThreadCatalog {
    pub(crate) fn from_records(records: Vec<ThreadRecord>) -> Self {
        let mut catalog = Self::default();
        for record in records {
            if ThreadKey::new(&record.key.channel_id, &record.key.root_ts).is_some() {
                catalog.records.insert(record.key.clone(), record);
            }
        }
        catalog
    }

    pub(crate) fn into_records(self) -> Vec<ThreadRecord> {
        let mut records = self.records.into_values().collect::<Vec<_>>();
        records.sort_by(|left, right| {
            left.key
                .channel_id
                .cmp(&right.key.channel_id)
                .then_with(|| left.key.root_ts.cmp(&right.key.root_ts))
        });
        records
    }

    pub(crate) fn get(&self, channel_id: &str, root_ts: &str) -> Option<&ThreadRecord> {
        ThreadKey::new(channel_id, root_ts).and_then(|key| self.records.get(&key))
    }

    /// Build the thread-inbox projection from locally observed roots and persisted Slack
    /// metadata. Catalog records win because they carry the most complete reply and unread data.
    pub(crate) fn inbox_projection(
        &self,
        observed: impl IntoIterator<Item = (String, SlackMessage)>,
    ) -> Vec<(String, SlackMessage)> {
        let mut roots = observed
            .into_iter()
            .map(|(channel_id, root)| ((channel_id, root.ts.clone()), root))
            .collect::<HashMap<_, _>>();

        for record in self.records.values() {
            if record.subscribed == Some(false) {
                continue;
            }
            let Some(root) = record.root.as_ref() else {
                continue;
            };
            let mut root = root.clone();
            root.reply_count = Some(record.reply_count);
            if let ThreadUnreadState::Known { count, .. } = &record.unread {
                root.unread_count = Some(*count);
            }
            roots.insert(
                (record.key.channel_id.clone(), record.key.root_ts.clone()),
                root,
            );
        }

        let mut roots = roots
            .into_iter()
            .map(|((channel_id, _), root)| (channel_id, root))
            .collect::<Vec<_>>();
        roots.sort_by(|(left_channel, left), (right_channel, right)| {
            right
                .latest_reply
                .as_deref()
                .unwrap_or(&right.ts)
                .cmp(left.latest_reply.as_deref().unwrap_or(&left.ts))
                .then_with(|| left_channel.cmp(right_channel))
                .then_with(|| left.ts.cmp(&right.ts))
        });
        roots
    }

    /// Additively discovers roots and orphan replies in any history page.
    pub(crate) fn observe_history(&mut self, channel_id: &str, messages: &[SlackMessage]) {
        for message in messages {
            self.observe_message(channel_id, message, false);
        }
    }

    /// Applies replies from `conversations.replies`. `complete` means every
    /// page was collected, so a last-read marker can safely yield an exact
    /// unread count when Slack omitted one.
    pub(crate) fn observe_thread(
        &mut self,
        channel_id: &str,
        root_ts: &str,
        messages: &[SlackMessage],
        complete: bool,
    ) {
        let Some(key) = ThreadKey::new(channel_id, root_ts) else {
            return;
        };
        self.records
            .entry(key.clone())
            .or_insert_with(|| ThreadRecord::placeholder(key.clone()));
        for message in messages {
            self.observe_message(channel_id, message, true);
        }

        let Some(record) = self.records.get_mut(&key) else {
            return;
        };
        if complete {
            record.reply_count = record.reply_count.max(record.seen_reply_ts.len() as u64);
            let last_read = record
                .root
                .as_ref()
                .and_then(|root| root.last_read.clone())
                .or_else(|| match &record.unread {
                    ThreadUnreadState::Known { last_read, .. } => last_read.clone(),
                    ThreadUnreadState::Unknown => None,
                });
            if let Some(last_read) = last_read {
                let count = record
                    .seen_reply_ts
                    .iter()
                    .filter(|reply_ts| reply_ts.as_str() > last_read.as_str())
                    .count() as u64;
                record.unread = ThreadUnreadState::Known {
                    count,
                    last_read: Some(last_read.clone()),
                };
                record.unread_reply_ts = record
                    .seen_reply_ts
                    .iter()
                    .filter(|reply_ts| reply_ts.as_str() > last_read.as_str())
                    .cloned()
                    .collect();
            }
        }
    }

    /// Rebuilds one complete thread from the coordinator's canonical timeline.
    ///
    /// Reply identities and derived aggregates are exact for the supplied
    /// complete timeline. Participation remains append-only, and newer local
    /// read metadata retained by `merge_root_metadata` still wins.
    pub(crate) fn reconcile_complete_thread(
        &mut self,
        channel_id: &str,
        root_ts: &str,
        messages: &[SlackMessage],
    ) {
        let Some(key) = ThreadKey::new(channel_id, root_ts) else {
            return;
        };
        let record = self
            .records
            .entry(key.clone())
            .or_insert_with(|| ThreadRecord::placeholder(key));
        if let Some(root) = messages
            .iter()
            .find(|message| message.ts == root_ts && message.thread_root_ts().is_none())
        {
            merge_root_metadata(record, root);
        }

        let replies = messages
            .iter()
            .filter(|message| message.thread_root_ts() == Some(root_ts))
            .collect::<Vec<_>>();
        for reply in &replies {
            record_participant(record, reply);
        }
        let canonical_reply_ts = replies
            .iter()
            .map(|message| message.ts.clone())
            .collect::<HashSet<_>>();
        record.deleted_reply_ts.extend(
            record
                .seen_reply_ts
                .difference(&canonical_reply_ts)
                .cloned(),
        );
        record
            .deleted_reply_ts
            .retain(|reply_ts| !canonical_reply_ts.contains(reply_ts));
        record.seen_reply_ts = canonical_reply_ts;
        record.reply_count = record.seen_reply_ts.len() as u64;
        record.latest_reply = record.seen_reply_ts.iter().max().cloned();
        if let Some(root) = record.root.as_mut() {
            if replies.is_empty() {
                root.reply_users = Some(Vec::new());
            } else if replies.iter().all(|reply| {
                reply
                    .user
                    .as_deref()
                    .is_some_and(|user_id| !user_id.trim().is_empty())
            }) {
                let mut active_reply_users = Vec::new();
                for user_id in replies.iter().filter_map(|reply| reply.user.as_ref()) {
                    if !active_reply_users.iter().any(|known| known == user_id) {
                        active_reply_users.push(user_id.clone());
                    }
                }
                root.reply_users = Some(active_reply_users);
            }
        }

        let known_last_read = match &record.unread {
            ThreadUnreadState::Known { last_read, .. } => last_read.clone(),
            ThreadUnreadState::Unknown => None,
        };
        if let Some(last_read) = known_last_read {
            record.unread_reply_ts = record
                .seen_reply_ts
                .iter()
                .filter(|reply_ts| reply_ts.as_str() > last_read.as_str())
                .cloned()
                .collect();
            record.unread = ThreadUnreadState::Known {
                count: record.unread_reply_ts.len() as u64,
                last_read: Some(last_read),
            };
        } else if let ThreadUnreadState::Known { count, .. } = &mut record.unread {
            *count = (*count).min(record.reply_count);
            let unread_count = usize::try_from(*count).unwrap_or(usize::MAX);
            let mut newest = record.seen_reply_ts.iter().cloned().collect::<Vec<_>>();
            newest.sort_by(|left, right| right.cmp(left));
            record.unread_reply_ts = newest.into_iter().take(unread_count).collect();
        } else {
            record.unread_reply_ts.clear();
        }
        sync_record_root_aggregates(record);
    }

    /// Applies a realtime message and increments known subscribed unread state
    /// once. Unknown state remains unknown rather than becoming a false count.
    pub(crate) fn observe_realtime(
        &mut self,
        channel_id: &str,
        message: &SlackMessage,
        current_user_id: Option<&str>,
    ) {
        let Some(root_ts) = reply_root_ts(message) else {
            self.observe_message(channel_id, message, false);
            return;
        };
        let (duplicate, previous_reply_count) = self
            .get(channel_id, root_ts)
            .map(|record| {
                (
                    record.seen_reply_ts.contains(&message.ts)
                        || (record.seen_reply_ts.is_empty()
                            && record
                                .latest_reply
                                .as_deref()
                                .is_some_and(|latest| message.ts.as_str() <= latest)),
                    record.reply_count,
                )
            })
            .unwrap_or((false, 0));
        self.observe_message(channel_id, message, true);
        if duplicate || message.user.as_deref() == current_user_id {
            return;
        }
        let Some(key) = ThreadKey::new(channel_id, root_ts) else {
            return;
        };
        let Some(record) = self.records.get_mut(&key) else {
            return;
        };
        record.reply_count = record
            .reply_count
            .max(previous_reply_count.saturating_add(1));
        if record.subscribed == Some(true) {
            if let ThreadUnreadState::Known { count, .. } = &mut record.unread {
                *count = count.saturating_add(1);
                record.unread_reply_ts.insert(message.ts.clone());
            }
        }
    }

    /// Reconciles one canonical message mutation across thread records.
    ///
    /// `previous` contains any coordinator-owned copies of the same logical
    /// message before the mutation. Persisting the resulting full projection
    /// in the message StoreBatch keeps reply timelines and thread metadata in
    /// one authority transaction.
    pub(crate) fn reconcile_message(
        &mut self,
        channel_id: &str,
        message: &SlackMessage,
        previous: &[SlackMessage],
        kind: ThreadCatalogMessageKind,
        current_user_id: Option<&str>,
    ) {
        let Some(channel_key) = non_empty(channel_id) else {
            return;
        };
        let mut identity_ts = previous
            .iter()
            .map(|previous| previous.ts.clone())
            .chain(std::iter::once(message.ts.clone()))
            .filter(|ts| !ts.trim().is_empty())
            .collect::<HashSet<_>>();
        if identity_ts.is_empty() {
            return;
        }
        identity_ts.insert(message.ts.clone());

        let identity_is_known = self.records.values().any(|record| {
            record.key.channel_id == channel_key
                && (record
                    .seen_reply_ts
                    .iter()
                    .any(|ts| identity_ts.contains(ts))
                    || record
                        .unread_reply_ts
                        .iter()
                        .any(|ts| identity_ts.contains(ts))
                    || record
                        .latest_reply
                        .as_ref()
                        .is_some_and(|ts| identity_ts.contains(ts)))
        });
        let identity_was_deleted = self.records.values().any(|record| {
            record.key.channel_id == channel_key && record.has_deleted_reply_identity(&identity_ts)
        });
        if message.thread_root_ts().is_some() && !identity_is_known && identity_was_deleted {
            return;
        }

        let previous_roots = previous
            .iter()
            .filter_map(|previous| previous.thread_root_ts().map(str::to_string))
            .collect::<HashSet<_>>();
        let current_root = (kind != ThreadCatalogMessageKind::Deleted)
            .then(|| message.thread_root_ts().map(str::to_string))
            .flatten();

        let mut old_keys = self
            .records
            .iter()
            .filter_map(|(key, record)| {
                (key.channel_id == channel_key
                    && ((!identity_is_known && previous_roots.contains(&key.root_ts))
                        || record
                            .seen_reply_ts
                            .iter()
                            .any(|ts| identity_ts.contains(ts))
                        || record
                            .unread_reply_ts
                            .iter()
                            .any(|ts| identity_ts.contains(ts))
                        || record
                            .latest_reply
                            .as_ref()
                            .is_some_and(|ts| identity_ts.contains(ts))))
                .then_some(key.clone())
            })
            .collect::<Vec<_>>();
        old_keys.sort_by(|left, right| left.root_ts.cmp(&right.root_ts));
        old_keys.dedup();

        let mut retained_in_current_root = false;
        let mut removed_from_other_root = false;
        for key in old_keys {
            let Some(record) = self.records.get_mut(&key) else {
                continue;
            };
            if current_root.as_deref() == Some(key.root_ts.as_str()) {
                reconcile_reply_in_place(record, message, &identity_ts);
                retained_in_current_root = true;
            } else {
                remove_reply_identity(
                    record,
                    message,
                    &identity_ts,
                    &previous_roots,
                    current_user_id,
                    kind == ThreadCatalogMessageKind::Deleted,
                );
                removed_from_other_root = true;
            }
        }

        if let Some(root_ts) = current_root.as_deref() {
            let Some(key) = ThreadKey::new(channel_key, root_ts) else {
                return;
            };
            let record = self
                .records
                .entry(key.clone())
                .or_insert_with(|| ThreadRecord::placeholder(key));
            if !retained_in_current_root {
                let known_changed_addition = removed_from_other_root
                    || previous
                        .iter()
                        .any(|previous| previous.thread_root_ts() != Some(root_ts));
                add_reply_identity(
                    record,
                    message,
                    kind,
                    current_user_id,
                    previous
                        .iter()
                        .any(|previous| previous.thread_root_ts() == Some(root_ts)),
                    known_changed_addition,
                );
            }
        } else if kind == ThreadCatalogMessageKind::Deleted {
            if let Some(key) = ThreadKey::new(channel_key, &message.ts) {
                if let Some(record) = self.records.get_mut(&key) {
                    record.root = None;
                }
            }
        } else {
            self.replace_root_projection(channel_key, message);
        }
    }

    /// Replaces root metadata from the coordinator's canonical timeline while
    /// preserving locally tracked subscription and unread identities.
    pub(crate) fn replace_root_projection(&mut self, channel_id: &str, root: &SlackMessage) {
        if root.thread_root_ts().is_some() || root.ts.trim().is_empty() {
            return;
        }
        let Some(key) = ThreadKey::new(channel_id, &root.ts) else {
            return;
        };
        let has_thread_metadata = root.reply_count.is_some()
            || root.latest_reply.is_some()
            || root.reply_users.is_some()
            || root.subscribed.is_some()
            || root.unread_count.is_some();
        if !has_thread_metadata && !self.records.contains_key(&key) {
            return;
        }
        let record = self
            .records
            .entry(key.clone())
            .or_insert_with(|| ThreadRecord::placeholder(key));
        merge_root_metadata(record, root);
        if let Some(reply_count) = root.reply_count {
            record.reply_count = reply_count;
        }
        if record.reply_count == 0 {
            record.latest_reply = None;
        } else if root.latest_reply.is_some() {
            record.latest_reply.clone_from(&root.latest_reply);
        }
        sync_record_root_aggregates(record);
    }

    /// Updates a root after a reply mutation without restoring stale unread
    /// metadata carried by an older root copy.
    pub(crate) fn replace_root_projection_after_reply(
        &mut self,
        channel_id: &str,
        root: &SlackMessage,
    ) {
        let Some(key) = ThreadKey::new(channel_id, &root.ts) else {
            return;
        };
        let record = self
            .records
            .entry(key.clone())
            .or_insert_with(|| ThreadRecord::placeholder(key));
        let known_unread = matches!(record.unread, ThreadUnreadState::Known { .. })
            .then(|| (record.unread.clone(), record.unread_reply_ts.clone()));
        merge_root_metadata(record, root);
        if let Some((unread, unread_reply_ts)) = known_unread {
            record.unread = unread;
            record.unread_reply_ts = unread_reply_ts;
        }
        if let Some(reply_count) = root.reply_count {
            record.reply_count = reply_count;
        }
        if record.reply_count == 0 {
            record.latest_reply = None;
        } else if root.latest_reply.is_some() {
            record.latest_reply.clone_from(&root.latest_reply);
        }
        sync_record_root_aggregates(record);
    }

    #[allow(dead_code)]
    pub(crate) fn mark_read(
        &mut self,
        channel_id: &str,
        root_ts: &str,
        last_read: &str,
    ) -> Vec<String> {
        let Some(key) = ThreadKey::new(channel_id, root_ts) else {
            return Vec::new();
        };
        if let Some(record) = self.records.get_mut(&key) {
            let (previous_count, previous_last_read) = match &record.unread {
                ThreadUnreadState::Known { count, last_read } => (*count, last_read.clone()),
                ThreadUnreadState::Unknown => (0, None),
            };
            if previous_last_read
                .as_deref()
                .is_some_and(|previous| !slack_timestamp_is_after(last_read, previous))
            {
                return Vec::new();
            }
            let mut cleared_reply_ts = record
                .unread_reply_ts
                .iter()
                .filter(|reply_ts| !slack_timestamp_is_after(reply_ts, last_read))
                .cloned()
                .collect::<Vec<_>>();
            if let Some(previous_last_read) = previous_last_read.as_deref() {
                cleared_reply_ts.extend(
                    record
                        .seen_reply_ts
                        .iter()
                        .filter(|reply_ts| {
                            slack_timestamp_is_after(reply_ts, previous_last_read)
                                && !slack_timestamp_is_after(reply_ts, last_read)
                        })
                        .cloned(),
                );
            }
            cleared_reply_ts.sort();
            cleared_reply_ts.dedup();
            record
                .unread_reply_ts
                .retain(|reply_ts| slack_timestamp_is_after(reply_ts, last_read));
            let marker_reaches_latest = record
                .latest_reply
                .as_deref()
                .is_none_or(|latest| !slack_timestamp_is_after(latest, last_read));
            let remaining_count = if marker_reaches_latest {
                0
            } else {
                previous_count
                    .saturating_sub(u64::try_from(cleared_reply_ts.len()).unwrap_or(u64::MAX))
            };
            record.unread = ThreadUnreadState::Known {
                count: remaining_count,
                last_read: (!last_read.trim().is_empty()).then(|| last_read.to_string()),
            };
            sync_record_root_aggregates(record);
            return cleared_reply_ts;
        }
        Vec::new()
    }

    fn observe_message(&mut self, channel_id: &str, message: &SlackMessage, thread_response: bool) {
        let root_ts = if thread_response {
            message
                .thread_ts
                .as_deref()
                .filter(|ts| !ts.is_empty())
                .unwrap_or(message.ts.as_str())
        } else if let Some(root_ts) = reply_root_ts(message) {
            root_ts
        } else if message.has_thread() {
            message.ts.as_str()
        } else {
            return;
        };
        let Some(key) = ThreadKey::new(channel_id, root_ts) else {
            return;
        };
        let record = self
            .records
            .entry(key.clone())
            .or_insert_with(|| ThreadRecord::placeholder(key));
        if message.ts == root_ts {
            merge_root_metadata(record, message);
        } else {
            if record.deleted_reply_ts.contains(&message.ts) {
                return;
            }
            if let Some(user_id) = message
                .user
                .as_deref()
                .map(str::trim)
                .filter(|user_id| !user_id.is_empty())
            {
                record.participant_user_ids.insert(user_id.to_string());
            }
            record.seen_reply_ts.insert(message.ts.clone());
            record.reply_count = record.reply_count.max(record.seen_reply_ts.len() as u64);
            if record.latest_reply.as_deref() < Some(message.ts.as_str()) {
                record.latest_reply = Some(message.ts.clone());
            }
        }
    }
}

fn reply_root_ts(message: &SlackMessage) -> Option<&str> {
    message
        .thread_ts
        .as_deref()
        .filter(|thread_ts| !thread_ts.is_empty() && *thread_ts != message.ts)
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn reconcile_reply_in_place(
    record: &mut ThreadRecord,
    message: &SlackMessage,
    identity_ts: &HashSet<String>,
) {
    let was_unread = record
        .unread_reply_ts
        .iter()
        .any(|ts| identity_ts.contains(ts));
    let replaced_latest = record
        .latest_reply
        .as_ref()
        .is_some_and(|latest| identity_ts.contains(latest));
    record.seen_reply_ts.retain(|ts| !identity_ts.contains(ts));
    record
        .unread_reply_ts
        .retain(|ts| !identity_ts.contains(ts));
    record.seen_reply_ts.insert(message.ts.clone());
    if was_unread {
        record.unread_reply_ts.insert(message.ts.clone());
    }
    if replaced_latest {
        record.latest_reply = record.seen_reply_ts.iter().max().cloned();
    }
    if record.latest_reply.as_deref() < Some(message.ts.as_str()) {
        record.latest_reply = Some(message.ts.clone());
    }
    record_participant(record, message);
    sync_record_root_aggregates(record);
}

fn remove_reply_identity(
    record: &mut ThreadRecord,
    message: &SlackMessage,
    identity_ts: &HashSet<String>,
    previous_roots: &HashSet<String>,
    current_user_id: Option<&str>,
    retain_deletion_authority: bool,
) {
    record_participant(record, message);
    if retain_deletion_authority {
        record.deleted_reply_ts.extend(identity_ts.iter().cloned());
    }
    let explicitly_known = previous_roots.contains(&record.key.root_ts);
    let seen_before = record
        .seen_reply_ts
        .iter()
        .any(|ts| identity_ts.contains(ts));
    let tracked_unread = record
        .unread_reply_ts
        .iter()
        .any(|ts| identity_ts.contains(ts));
    let author_is_known_other = current_user_id.is_some_and(|current_user_id| {
        message
            .user
            .as_deref()
            .is_some_and(|author_id| !author_id.trim().is_empty() && author_id != current_user_id)
    });
    let inferred_unread = author_is_known_other
        && matches!(
        &record.unread,
        ThreadUnreadState::Known { count, last_read }
            if *count > 0
                && identity_ts.iter().any(|ts| {
                    last_read
                        .as_deref()
                        .is_none_or(|last_read| ts.as_str() > last_read)
                })
        );
    let removed_latest = record
        .latest_reply
        .as_ref()
        .is_some_and(|latest| identity_ts.contains(latest));
    record.seen_reply_ts.retain(|ts| !identity_ts.contains(ts));
    record
        .unread_reply_ts
        .retain(|ts| !identity_ts.contains(ts));
    if seen_before || explicitly_known || removed_latest {
        record.reply_count = record.reply_count.saturating_sub(1);
        if tracked_unread || inferred_unread {
            if let ThreadUnreadState::Known { count, .. } = &mut record.unread {
                *count = count.saturating_sub(1);
            }
        }
    }
    if removed_latest {
        record.latest_reply = record.seen_reply_ts.iter().max().cloned();
    }
    if record.reply_count == 0 {
        record.latest_reply = None;
    }
    sync_record_root_aggregates(record);
}

fn add_reply_identity(
    record: &mut ThreadRecord,
    message: &SlackMessage,
    kind: ThreadCatalogMessageKind,
    current_user_id: Option<&str>,
    previously_in_root: bool,
    known_changed_addition: bool,
) {
    let duplicate = previously_in_root
        || record.seen_reply_ts.contains(&message.ts)
        || (kind == ThreadCatalogMessageKind::Posted
            && record.seen_reply_ts.is_empty()
            && record
                .latest_reply
                .as_deref()
                .is_some_and(|latest| message.ts.as_str() <= latest));
    record.seen_reply_ts.insert(message.ts.clone());
    record_participant(record, message);
    let aggregate_addition_is_known =
        kind == ThreadCatalogMessageKind::Posted || known_changed_addition;
    if !duplicate && aggregate_addition_is_known {
        record.reply_count = record.reply_count.saturating_add(1);
        let should_record_unread = message.user.as_deref() != current_user_id
            && record.subscribed == Some(true)
            && matches!(
                &record.unread,
                ThreadUnreadState::Known { last_read, .. }
                    if last_read
                        .as_deref()
                        .is_none_or(|last_read| message.ts.as_str() > last_read)
            );
        if should_record_unread {
            if let ThreadUnreadState::Known { count, .. } = &mut record.unread {
                *count = count.saturating_add(1);
            }
            record.unread_reply_ts.insert(message.ts.clone());
        }
    }
    if kind != ThreadCatalogMessageKind::Changed || known_changed_addition || previously_in_root {
        record.reply_count = record.reply_count.max(record.seen_reply_ts.len() as u64);
    }
    if record.latest_reply.as_deref() < Some(message.ts.as_str()) {
        record.latest_reply = Some(message.ts.clone());
    }
    sync_record_root_aggregates(record);
}

fn record_participant(record: &mut ThreadRecord, message: &SlackMessage) {
    if let Some(user_id) = message
        .user
        .as_deref()
        .map(str::trim)
        .filter(|user_id| !user_id.is_empty())
    {
        record.participant_user_ids.insert(user_id.to_string());
    }
}

fn sync_record_root_aggregates(record: &mut ThreadRecord) {
    let Some(root) = record.root.as_mut() else {
        return;
    };
    root.reply_count = Some(record.reply_count);
    root.latest_reply.clone_from(&record.latest_reply);
    if record.subscribed.is_some() {
        root.subscribed = record.subscribed;
    }
    if let ThreadUnreadState::Known { count, last_read } = &record.unread {
        root.unread_count = Some(*count);
        root.last_read.clone_from(last_read);
    }
}

fn merge_root_metadata(record: &mut ThreadRecord, root: &SlackMessage) {
    record.reply_count = record.reply_count.max(root.reply_count.unwrap_or_default());
    if record.latest_reply.as_deref() < root.latest_reply.as_deref() {
        record.latest_reply = root.latest_reply.clone();
    }
    if root.subscribed.is_some() {
        record.subscribed = root.subscribed;
    }
    record.participant_user_ids.extend(
        root.reply_users
            .iter()
            .flatten()
            .filter(|user_id| !user_id.trim().is_empty())
            .cloned(),
    );
    if let Some(unread_count) = root.unread_count {
        let preserves_newer_local_read = matches!(
            &record.unread,
            ThreadUnreadState::Known {
                count,
                last_read: Some(known_last_read),
            } if root.last_read.as_deref().is_none_or(|incoming| {
                known_last_read.as_str() > incoming
                    || (known_last_read.as_str() == incoming && *count == 0)
            })
        );
        if preserves_newer_local_read {
            if let ThreadUnreadState::Known {
                count,
                last_read: Some(last_read),
            } = &mut record.unread
            {
                record.unread_reply_ts = record
                    .seen_reply_ts
                    .iter()
                    .filter(|reply_ts| reply_ts.as_str() > last_read.as_str())
                    .cloned()
                    .collect();
                *count = (*count)
                    .max(record.unread_reply_ts.len() as u64)
                    .min(record.reply_count);
            }
        } else {
            record.unread = ThreadUnreadState::Known {
                count: unread_count,
                last_read: root.last_read.clone(),
            };
            if let Some(last_read) = root.last_read.as_deref() {
                record.unread_reply_ts = record
                    .seen_reply_ts
                    .iter()
                    .filter(|reply_ts| reply_ts.as_str() > last_read)
                    .cloned()
                    .collect();
            } else if unread_count == 0 {
                record.unread_reply_ts.clear();
            }
        }
    } else if let Some(incoming_last_read) = root.last_read.as_ref() {
        let known = match &record.unread {
            ThreadUnreadState::Known { count, last_read } => Some((*count, last_read.clone())),
            ThreadUnreadState::Unknown => None,
        };
        let known_last_read = known.as_ref().and_then(|(_, last_read)| last_read.clone());
        let marker_advanced = known_last_read
            .as_deref()
            .is_none_or(|known| incoming_last_read.as_str() > known);
        let retained_last_read = known_last_read
            .filter(|known| known.as_str() >= incoming_last_read.as_str())
            .unwrap_or_else(|| incoming_last_read.clone());
        let previous_unread_reply_ts = record.unread_reply_ts.clone();
        record.unread_reply_ts = record
            .seen_reply_ts
            .iter()
            .filter(|reply_ts| reply_ts.as_str() > retained_last_read.as_str())
            .cloned()
            .collect();
        let observed_count = record.unread_reply_ts.len() as u64;
        let count = match known {
            Some((known_count, known_last_read)) if marker_advanced => {
                let provably_cleared = previous_unread_reply_ts
                    .iter()
                    .filter(|reply_ts| {
                        known_last_read
                            .as_deref()
                            .is_none_or(|last_read| reply_ts.as_str() > last_read)
                            && reply_ts.as_str() <= incoming_last_read.as_str()
                    })
                    .count() as u64;
                known_count.saturating_sub(provably_cleared)
            }
            Some((known_count, _)) => known_count,
            None => observed_count,
        }
        .max(observed_count)
        .min(record.reply_count);
        record.unread = ThreadUnreadState::Known {
            count,
            last_read: Some(retained_last_read),
        };
    }
    let mut merged_root = root.clone();
    if let ThreadUnreadState::Known { count, last_read } = &record.unread {
        merged_root.unread_count = Some(*count);
        merged_root.last_read = last_read.clone();
    }
    record.root = Some(merged_root);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(ts: &str, reply_count: u64) -> SlackMessage {
        SlackMessage {
            ts: ts.into(),
            reply_count: Some(reply_count),
            ..Default::default()
        }
    }

    fn reply(ts: &str, root_ts: &str, user: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.into(),
            thread_ts: Some(root_ts.into()),
            user: Some(user.into()),
            ..Default::default()
        }
    }

    #[test]
    fn history_additively_discovers_roots_and_orphan_replies() {
        let mut catalog = ThreadCatalog::default();
        catalog.observe_history("C1", &[root("1.0", 2), reply("3.0", "2.0", "U2")]);
        assert_eq!(catalog.get("C1", "1.0").unwrap().reply_count, 2);
        assert_eq!(
            catalog.get("C1", "2.0").unwrap().latest_reply.as_deref(),
            Some("3.0")
        );
        catalog.observe_history("C1", &[]);
        assert!(catalog.get("C1", "1.0").is_some());
    }

    #[test]
    fn explicit_metadata_supplies_subscription_and_unreads() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 2);
        root.subscribed = Some(true);
        root.last_read = Some("2.0".into());
        root.unread_count = Some(1);
        root.latest_reply = Some("3.0".into());
        catalog.observe_thread("C1", "1.0", &[root], false);
        let record = catalog.get("C1", "1.0").unwrap();
        assert!(record.is_known_subscribed());
        assert_eq!(record.latest_reply.as_deref(), Some("3.0"));
        assert_eq!(
            record.unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("2.0".into())
            }
        );
    }

    #[test]
    fn complete_replies_derive_unreads_from_last_read() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 2);
        root.last_read = Some("1.5".into());
        catalog.observe_thread(
            "C1",
            "1.0",
            &[root, reply("2.0", "1.0", "U2"), reply("3.0", "1.0", "U3")],
            true,
        );
        assert_eq!(
            catalog.get("C1", "1.0").unwrap().unread,
            ThreadUnreadState::Known {
                count: 2,
                last_read: Some("1.5".into())
            }
        );
    }

    #[test]
    fn partial_replies_preserve_unknown_unread_state() {
        let mut catalog = ThreadCatalog::default();
        catalog.observe_thread("C1", "1.0", &[root("1.0", 3)], false);
        assert_eq!(
            catalog.get("C1", "1.0").unwrap().unread,
            ThreadUnreadState::Unknown
        );
    }

    #[test]
    fn local_thread_read_marker_beats_older_server_metadata() {
        let mut catalog = ThreadCatalog::default();
        let mut initial = root("1.0", 1);
        initial.unread_count = Some(1);
        initial.last_read = Some("1.0".into());
        catalog.observe_thread("C1", "1.0", &[initial], false);
        catalog.mark_read("C1", "1.0", "2.0");

        let mut stale = root("1.0", 1);
        stale.unread_count = Some(1);
        stale.last_read = Some("1.0".into());
        catalog.observe_thread("C1", "1.0", &[stale], false);

        let record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(
            record.unread,
            ThreadUnreadState::Known {
                count: 0,
                last_read: Some("2.0".into())
            }
        );
        assert_eq!(
            record.root.as_ref().and_then(|root| root.unread_count),
            Some(0)
        );
    }

    #[test]
    fn partial_thread_read_marker_beats_older_server_metadata() {
        let mut catalog = ThreadCatalog::default();
        let mut current = root("1.0", 3);
        current.unread_count = Some(1);
        current.last_read = Some("3.0".into());
        catalog.observe_thread("C1", "1.0", &[current], false);

        let mut stale = root("1.0", 3);
        stale.unread_count = Some(2);
        stale.last_read = Some("1.0".into());
        catalog.observe_thread("C1", "1.0", &[stale], false);

        let record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(
            record.unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("3.0".into())
            }
        );
        let projected_root = record.root.as_ref().unwrap();
        assert_eq!(projected_root.unread_count, Some(1));
        assert_eq!(projected_root.last_read.as_deref(), Some("3.0"));
    }

    #[test]
    fn last_read_only_metadata_initializes_and_merges_monotonically() {
        let mut catalog = ThreadCatalog::default();
        let mut initial_root = root("1.0", 2);
        initial_root.last_read = Some("1.0".into());
        initial_root.latest_reply = Some("3.0".into());
        let replies = [reply("2.0", "1.0", "U2"), reply("3.0", "1.0", "U3")];
        catalog.reconcile_complete_thread(
            "C1",
            "1.0",
            &[initial_root.clone(), replies[0].clone(), replies[1].clone()],
        );
        assert_eq!(
            catalog.get("C1", "1.0").unwrap().unread,
            ThreadUnreadState::Known {
                count: 2,
                last_read: Some("1.0".into()),
            }
        );

        let mut current = initial_root;
        current.unread_count = Some(1);
        current.last_read = Some("2.0".into());
        catalog.observe_thread("C1", "1.0", &[current], false);
        let mut stale_marker_only = root("1.0", 2);
        stale_marker_only.last_read = Some("1.0".into());
        catalog.observe_thread("C1", "1.0", &[stale_marker_only], false);
        let record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(
            record.unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("2.0".into()),
            }
        );
        assert_eq!(
            record
                .root
                .as_ref()
                .and_then(|root| root.last_read.as_deref()),
            Some("2.0")
        );
    }

    #[test]
    fn realtime_replies_increment_known_subscribed_threads_once() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 1);
        root.subscribed = Some(true);
        root.unread_count = Some(0);
        catalog.observe_thread("C1", "1.0", &[root], false);
        let reply = reply("2.0", "1.0", "U2");
        catalog.observe_realtime("C1", &reply, Some("ME"));
        catalog.observe_realtime("C1", &reply, Some("ME"));
        assert_eq!(
            catalog.get("C1", "1.0").unwrap().unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: None
            }
        );
        assert_eq!(catalog.get("C1", "1.0").unwrap().reply_count, 2);
    }

    #[test]
    fn realtime_delete_reconciles_reply_unread_latest_and_keeps_participants() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 2);
        root.subscribed = Some(true);
        root.unread_count = Some(2);
        root.last_read = Some("1.0".into());
        root.latest_reply = Some("3.0".into());
        let reply_two = reply("2.0", "1.0", "U2");
        let reply_three = reply("3.0", "1.0", "U3");
        catalog.observe_thread("C1", "1.0", &[root, reply_two, reply_three.clone()], true);

        catalog.reconcile_message(
            "C1",
            &reply_three,
            std::slice::from_ref(&reply_three),
            ThreadCatalogMessageKind::Deleted,
            Some("ME"),
        );

        let record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(record.reply_count, 1);
        assert_eq!(record.latest_reply.as_deref(), Some("2.0"));
        assert_eq!(
            record.unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("1.0".into()),
            }
        );
        assert!(record.participant_user_ids.contains("U2"));
        assert!(record.participant_user_ids.contains("U3"));
    }

    #[test]
    fn realtime_move_transfers_reply_metadata_once_between_threads() {
        let mut catalog = ThreadCatalog::default();
        let mut first_root = root("1.0", 1);
        first_root.subscribed = Some(true);
        first_root.unread_count = Some(1);
        first_root.last_read = Some("1.0".into());
        first_root.latest_reply = Some("2.0".into());
        let previous = reply("2.0", "1.0", "U2");
        catalog.observe_thread("C1", "1.0", &[first_root, previous.clone()], true);

        let mut second_root = root("10.0", 0);
        second_root.subscribed = Some(true);
        second_root.unread_count = Some(0);
        second_root.last_read = Some("1.0".into());
        catalog.observe_thread("C1", "10.0", &[second_root], true);

        let moved = SlackMessage {
            thread_ts: Some("10.0".into()),
            ..previous.clone()
        };
        catalog.reconcile_message(
            "C1",
            &moved,
            std::slice::from_ref(&previous),
            ThreadCatalogMessageKind::Changed,
            Some("ME"),
        );
        catalog.reconcile_message(
            "C1",
            &moved,
            std::slice::from_ref(&moved),
            ThreadCatalogMessageKind::Changed,
            Some("ME"),
        );

        let first = catalog.get("C1", "1.0").unwrap();
        assert_eq!(first.reply_count, 0);
        assert_eq!(first.latest_reply, None);
        assert_eq!(
            first.unread,
            ThreadUnreadState::Known {
                count: 0,
                last_read: Some("1.0".into()),
            }
        );
        assert!(first.participant_user_ids.contains("U2"));

        let second = catalog.get("C1", "10.0").unwrap();
        assert_eq!(second.reply_count, 1);
        assert_eq!(second.latest_reply.as_deref(), Some("2.0"));
        assert_eq!(
            second.unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("1.0".into()),
            }
        );
        assert!(second.participant_user_ids.contains("U2"));
    }

    #[test]
    fn delete_with_stale_root_metadata_removes_only_the_known_reply_location() {
        let mut catalog = ThreadCatalog::default();
        let mut first_root = root("1.0", 2);
        first_root.latest_reply = Some("3.0".into());
        let moved_from_first = reply("2.0", "1.0", "U2");
        let retained_in_first = reply("3.0", "1.0", "U3");
        catalog.observe_thread(
            "C1",
            "1.0",
            &[
                first_root,
                moved_from_first.clone(),
                retained_in_first.clone(),
            ],
            true,
        );
        catalog.observe_thread("C1", "10.0", &[root("10.0", 0)], true);

        let moved = SlackMessage {
            thread_ts: Some("10.0".into()),
            ..moved_from_first.clone()
        };
        catalog.reconcile_message(
            "C1",
            &moved,
            std::slice::from_ref(&moved_from_first),
            ThreadCatalogMessageKind::Changed,
            Some("ME"),
        );
        catalog.reconcile_message(
            "C1",
            &moved_from_first,
            std::slice::from_ref(&moved_from_first),
            ThreadCatalogMessageKind::Deleted,
            Some("ME"),
        );

        let first = catalog.get("C1", "1.0").unwrap();
        assert_eq!(first.reply_count, 1);
        assert_eq!(first.latest_reply.as_deref(), Some("3.0"));
        let second = catalog.get("C1", "10.0").unwrap();
        assert_eq!(second.reply_count, 0);
        assert_eq!(second.latest_reply, None);
    }

    #[test]
    fn realtime_broadcast_transition_keeps_thread_aggregates_stable() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 1);
        root.subscribed = Some(true);
        root.unread_count = Some(1);
        root.last_read = Some("1.0".into());
        root.latest_reply = Some("2.0".into());
        let previous = reply("2.0", "1.0", "U2");
        catalog.observe_thread("C1", "1.0", &[root, previous.clone()], true);
        let broadcast = SlackMessage {
            subtype: Some("thread_broadcast".into()),
            ..previous.clone()
        };

        catalog.reconcile_message(
            "C1",
            &broadcast,
            std::slice::from_ref(&previous),
            ThreadCatalogMessageKind::Changed,
            Some("ME"),
        );

        let record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(record.reply_count, 1);
        assert_eq!(record.latest_reply.as_deref(), Some("2.0"));
        assert_eq!(
            record.unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("1.0".into()),
            }
        );
    }

    #[test]
    fn complete_thread_reconciliation_removes_absent_replies_exactly() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 3);
        root.subscribed = Some(true);
        root.unread_count = Some(3);
        root.last_read = Some("1.0".into());
        root.latest_reply = Some("4.0".into());
        let reply_two = reply("2.0", "1.0", "U2");
        catalog.observe_thread(
            "C1",
            "1.0",
            &[
                root.clone(),
                reply_two.clone(),
                reply("3.0", "1.0", "U3"),
                reply("4.0", "1.0", "U4"),
            ],
            true,
        );

        catalog.reconcile_complete_thread("C1", "1.0", &[root, reply_two]);

        let record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(record.reply_count, 1);
        assert_eq!(record.latest_reply.as_deref(), Some("2.0"));
        assert_eq!(
            record.unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("1.0".into()),
            }
        );
        assert!(["U2", "U3", "U4"]
            .iter()
            .all(|user_id| record.participant_user_ids.contains(*user_id)));
        let projected_root = record.root.as_ref().unwrap();
        assert_eq!(projected_root.reply_count, Some(1));
        assert_eq!(projected_root.latest_reply.as_deref(), Some("2.0"));
        assert_eq!(projected_root.unread_count, Some(1));
    }

    #[test]
    fn complete_root_only_thread_clears_metadata_only_reply_aggregates() {
        let mut catalog = ThreadCatalog::default();
        let mut stale_root = root("1.0", 2);
        stale_root.latest_reply = Some("3.0".into());
        stale_root.reply_users = Some(vec!["U2".into(), "U3".into()]);
        catalog.observe_history("C1", std::slice::from_ref(&stale_root));

        catalog.reconcile_complete_thread("C1", "1.0", std::slice::from_ref(&stale_root));

        let record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(record.reply_count, 0);
        assert_eq!(record.latest_reply, None);
        assert!(record.seen_reply_ts.is_empty());
        let projected_root = record.root.as_ref().unwrap();
        assert_eq!(projected_root.reply_count, Some(0));
        assert_eq!(projected_root.latest_reply, None);
        assert_eq!(projected_root.reply_users.as_deref(), Some(&[][..]));
    }

    #[test]
    fn complete_thread_explicitly_restores_and_clears_deleted_reply_identity() {
        let mut catalog = ThreadCatalog::default();
        let root = root("1.0", 1);
        let reply = reply("2.0", "1.0", "U2");
        catalog.observe_thread("C1", "1.0", &[root.clone(), reply.clone()], true);
        catalog.reconcile_message(
            "C1",
            &reply,
            std::slice::from_ref(&reply),
            ThreadCatalogMessageKind::Deleted,
            Some("ME"),
        );
        let identity = HashSet::from([reply.ts.clone()]);
        assert!(catalog
            .get("C1", "1.0")
            .unwrap()
            .has_deleted_reply_identity(&identity));

        catalog.observe_history("C1", std::slice::from_ref(&reply));
        assert_eq!(catalog.get("C1", "1.0").unwrap().reply_count, 0);
        catalog.reconcile_complete_thread("C1", "1.0", &[root, reply]);

        let record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(record.reply_count, 1);
        assert!(!record.has_deleted_reply_identity(&identity));
    }

    #[test]
    fn last_read_only_advance_subtracts_only_provably_cleared_unread_replies() {
        let mut catalog = ThreadCatalog::default();
        let mut initial_root = root("1.0", 3);
        initial_root.subscribed = Some(true);
        initial_root.unread_count = Some(2);
        initial_root.last_read = Some("1.0".into());
        catalog.observe_history("C1", std::slice::from_ref(&initial_root));
        catalog.observe_realtime("C1", &reply("2.0", "1.0", "U2"), Some("ME"));
        assert_eq!(
            catalog.get("C1", "1.0").unwrap().unread,
            ThreadUnreadState::Known {
                count: 3,
                last_read: Some("1.0".into()),
            }
        );

        let mut advanced = root("1.0", 3);
        advanced.last_read = Some("2.0".into());
        catalog.observe_history("C1", &[advanced]);

        assert_eq!(
            catalog.get("C1", "1.0").unwrap().unread,
            ThreadUnreadState::Known {
                count: 2,
                last_read: Some("2.0".into()),
            }
        );
    }

    #[test]
    fn realtime_root_edit_and_delete_refresh_then_clear_inbox_projection() {
        let mut catalog = ThreadCatalog::default();
        let mut original = root("1.0", 1);
        original.text = Some("original".into());
        let reply = reply("2.0", "1.0", "U2");
        catalog.observe_thread("C1", "1.0", &[original.clone(), reply.clone()], true);
        let edited = SlackMessage {
            text: Some("edited".into()),
            ..original.clone()
        };

        catalog.reconcile_message(
            "C1",
            &edited,
            std::slice::from_ref(&original),
            ThreadCatalogMessageKind::Changed,
            Some("ME"),
        );
        let edited_record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(
            edited_record
                .root
                .as_ref()
                .and_then(|root| root.text.as_deref()),
            Some("edited")
        );
        assert_eq!(edited_record.reply_count, 1);

        catalog.reconcile_message(
            "C1",
            &edited,
            std::slice::from_ref(&edited),
            ThreadCatalogMessageKind::Deleted,
            Some("ME"),
        );
        let deleted_record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(deleted_record.root, None);
        assert_eq!(deleted_record.reply_count, 1);
        assert_eq!(deleted_record.latest_reply.as_deref(), Some("2.0"));
        assert!(deleted_record.participant_user_ids.contains("U2"));
        assert!(catalog.inbox_projection(Vec::new()).is_empty());
    }

    #[test]
    fn mark_read_returns_exact_realtime_reply_timestamps_without_a_prior_marker() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 0);
        root.subscribed = Some(true);
        root.unread_count = Some(0);
        catalog.observe_thread("C1", "1.0", &[root], false);
        catalog.observe_realtime("C1", &reply("2.0", "1.0", "U2"), Some("ME"));
        catalog.observe_realtime("C1", &reply("3.0", "1.0", "U3"), Some("ME"));

        assert_eq!(
            catalog.mark_read("C1", "1.0", "3.0"),
            vec!["2.0".to_string(), "3.0".to_string()]
        );
    }

    #[test]
    fn realtime_deduplication_does_not_drop_out_of_order_replies() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 0);
        root.subscribed = Some(true);
        root.unread_count = Some(0);
        catalog.observe_thread("C1", "1.0", &[root], false);

        catalog.observe_realtime("C1", &reply("3.0", "1.0", "U2"), Some("ME"));
        catalog.observe_realtime("C1", &reply("2.0", "1.0", "U3"), Some("ME"));

        let record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(record.reply_count, 2);
        assert_eq!(
            record.unread,
            ThreadUnreadState::Known {
                count: 2,
                last_read: None
            }
        );
    }

    #[test]
    fn mark_read_records_the_marker() {
        let mut catalog = ThreadCatalog::default();
        catalog.observe_history("C1", &[root("1.0", 1)]);
        catalog.mark_read("C1", "1.0", "2.0");
        assert_eq!(
            catalog.get("C1", "1.0").unwrap().unread,
            ThreadUnreadState::Known {
                count: 0,
                last_read: Some("2.0".into())
            }
        );
        let projected_root = catalog.get("C1", "1.0").unwrap().root.as_ref().unwrap();
        assert_eq!(projected_root.unread_count, Some(0));
        assert_eq!(projected_root.last_read.as_deref(), Some("2.0"));
    }

    #[test]
    fn mark_read_preserves_a_reply_newer_than_the_marker() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 1);
        root.subscribed = Some(true);
        root.unread_count = Some(1);
        root.latest_reply = Some("3.0".into());
        catalog.observe_thread("C1", "1.0", &[root], false);
        catalog.mark_read("C1", "1.0", "2.0");
        assert_eq!(
            catalog.get("C1", "1.0").unwrap().unread,
            ThreadUnreadState::Known {
                count: 1,
                last_read: Some("2.0".into())
            }
        );
    }

    #[test]
    fn complete_pagination_counts_replies_observed_across_pages() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 3);
        root.subscribed = Some(true);
        root.last_read = Some("1.5".into());
        catalog.observe_thread("C1", "1.0", &[root, reply("3.0", "1.0", "U2")], false);
        catalog.observe_thread(
            "C1",
            "1.0",
            &[reply("2.0", "1.0", "U3"), reply("1.4", "1.0", "U4")],
            true,
        );
        assert_eq!(
            catalog.get("C1", "1.0").unwrap().unread,
            ThreadUnreadState::Known {
                count: 2,
                last_read: Some("1.5".into())
            }
        );
    }

    #[test]
    fn records_round_trip_with_stable_composite_keys() {
        let mut catalog = ThreadCatalog::default();
        catalog.observe_history("C2", &[root("2.0", 1)]);
        catalog.observe_history("C1", &[root("1.0", 1)]);
        catalog.observe_realtime("C1", &reply("2.0", "1.0", "U_SELF"), Some("U_SELF"));
        let records = catalog.into_records();
        assert_eq!(records[0].key, ThreadKey::new("C1", "1.0").unwrap());
        assert!(records[0].participant_user_ids.contains("U_SELF"));
        assert!(ThreadCatalog::from_records(records)
            .get("C2", "2.0")
            .is_some());
    }

    #[test]
    fn legacy_records_default_missing_participants_and_delete_authority() {
        let mut catalog = ThreadCatalog::default();
        catalog.observe_history("C1", &[root("1.0", 1)]);
        let record = catalog.into_records().pop().unwrap();
        let mut value = serde_json::to_value(record).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("participant_user_ids");
        object.remove("deleted_reply_ts");

        let restored: ThreadRecord = serde_json::from_value(value).unwrap();
        assert!(restored.participant_user_ids.is_empty());
        assert!(!restored.has_deleted_replies());
    }

    #[test]
    fn inbox_projection_merges_observed_roots_with_authoritative_catalog_metadata() {
        let mut catalog = ThreadCatalog::default();
        let mut catalog_root = root("1.0", 3);
        catalog_root.latest_reply = Some("4.0".into());
        catalog_root.unread_count = Some(2);
        catalog.observe_thread("C1", "1.0", &[catalog_root], false);

        let mut observed_root = root("1.0", 1);
        observed_root.latest_reply = Some("2.0".into());
        let projection = catalog.inbox_projection(vec![("C1".into(), observed_root)]);

        assert_eq!(projection.len(), 1);
        assert_eq!(projection[0].1.reply_count, Some(3));
        assert_eq!(projection[0].1.unread_count, Some(2));
        assert_eq!(projection[0].1.latest_reply.as_deref(), Some("4.0"));
    }
}
