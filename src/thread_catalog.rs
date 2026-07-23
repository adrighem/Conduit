use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::models::SlackMessage;

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
            unread_reply_ts: HashSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_known_subscribed(&self) -> bool {
        self.subscribed == Some(true)
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
            if record
                .latest_reply
                .as_deref()
                .is_some_and(|latest| last_read < latest)
            {
                return Vec::new();
            }
            let mut cleared_reply_ts = record
                .unread_reply_ts
                .iter()
                .filter(|reply_ts| reply_ts.as_str() <= last_read)
                .cloned()
                .collect::<Vec<_>>();
            if let ThreadUnreadState::Known {
                last_read: Some(previous_last_read),
                ..
            } = &record.unread
            {
                cleared_reply_ts.extend(
                    record
                        .seen_reply_ts
                        .iter()
                        .filter(|reply_ts| {
                            reply_ts.as_str() > previous_last_read.as_str()
                                && reply_ts.as_str() <= last_read
                        })
                        .cloned(),
                );
            }
            cleared_reply_ts.sort();
            cleared_reply_ts.dedup();
            record
                .unread_reply_ts
                .retain(|reply_ts| reply_ts.as_str() > last_read);
            record.unread = ThreadUnreadState::Known {
                count: 0,
                last_read: (!last_read.trim().is_empty()).then(|| last_read.to_string()),
            };
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
                count: 0,
                last_read: Some(known_last_read),
            } if root.last_read.as_deref().is_none_or(|incoming| known_last_read.as_str() >= incoming)
        );
        if !preserves_newer_local_read {
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
    } else if let Some(last_read) = root.last_read.as_ref() {
        if let ThreadUnreadState::Known {
            last_read: known, ..
        } = &mut record.unread
        {
            *known = Some(last_read.clone());
        }
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
    }

    #[test]
    fn mark_read_does_not_clear_a_reply_newer_than_the_marker() {
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
                last_read: None
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
    fn legacy_records_default_missing_thread_participants() {
        let mut catalog = ThreadCatalog::default();
        catalog.observe_history("C1", &[root("1.0", 1)]);
        let record = catalog.into_records().pop().unwrap();
        let mut value = serde_json::to_value(record).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("participant_user_ids");

        let restored: ThreadRecord = serde_json::from_value(value).unwrap();
        assert!(restored.participant_user_ids.is_empty());
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
