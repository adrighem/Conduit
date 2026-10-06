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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadRecord {
    pub(crate) key: ThreadKey,
    pub(crate) root: Option<SlackMessage>,
    pub(crate) reply_count: u64,
    pub(crate) latest_reply: Option<String>,
    #[serde(default)]
    pub(crate) last_read: Option<String>,
    /// How many of `reply_count` replies have been marked read. Paired with
    /// `last_read`: always set together via `mark_read()`. Deliberately NOT
    /// derived from `seen_reply_ts` (a realtime-only dedup cache that is
    /// empty for any thread whose replies were never streamed in this
    /// session) so the unread count is correct immediately on cold start.
    #[serde(default)]
    pub(crate) read_reply_count: u64,
    /// `None` means Slack has not supplied subscription metadata yet.
    pub(crate) subscribed: Option<bool>,
    /// Reply authors are append-only: deleting a reply does not erase the
    /// fact that its author previously participated in the thread.
    #[serde(default)]
    pub(crate) participant_user_ids: HashSet<String>,
    #[serde(default)]
    seen_reply_ts: HashSet<String>,
}

impl ThreadRecord {
    pub(crate) fn placeholder(key: ThreadKey) -> Self {
        Self {
            key,
            root: None,
            reply_count: 0,
            latest_reply: None,
            last_read: None,
            read_reply_count: 0,
            subscribed: None,
            participant_user_ids: HashSet::new(),
            seen_reply_ts: HashSet::new(),
        }
    }

    pub fn has_unread_replies(&self) -> bool {
        let Some(latest_reply) = self
            .latest_reply
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return false;
        };
        match self
            .last_read
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(last_read) => slack_timestamp_is_after(latest_reply, last_read),
            None => true,
        }
    }

    /// Replies not yet marked read. Derived from `reply_count`/`read_reply_count`
    /// (not `seen_reply_ts`) so it's accurate even for a thread whose replies
    /// were only ever seen via channel history, never streamed this session.
    pub fn unread_reply_count(&self) -> u64 {
        self.reply_count.saturating_sub(self.read_reply_count)
    }

    /// Marks every reply seen so far as read. Local-only: no Slack API backs
    /// thread read-state. `last_read` and `read_reply_count` are always set
    /// together; they're one "I've seen everything up to here" operation.
    pub(crate) fn mark_read(&mut self) {
        self.last_read = self.latest_reply.clone();
        self.read_reply_count = self.reply_count;
    }

    #[cfg(test)]
    pub(crate) fn is_known_subscribed(&self) -> bool {
        self.subscribed == Some(true)
    }

    #[cfg(test)]
    pub(crate) fn has_seen_reply(&self, reply_ts: &str) -> bool {
        self.seen_reply_ts.contains(reply_ts)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
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

    pub(crate) fn to_records(&self) -> Vec<ThreadRecord> {
        let mut records = self.records.values().cloned().collect::<Vec<_>>();
        records.sort_by(|left, right| {
            left.key
                .channel_id
                .cmp(&right.key.channel_id)
                .then_with(|| left.key.root_ts.cmp(&right.key.root_ts))
        });
        records
    }

    pub(crate) fn unread_summaries_for_user<'a>(
        &'a self,
        user_id: &'a str,
    ) -> impl Iterator<Item = (&'a str, &'a str, u64, u64)> + 'a {
        self.records.values().filter_map(move |record| {
            if record.has_unread_replies() && record.participant_user_ids.contains(user_id) {
                Some((
                    record.key.channel_id.as_str(),
                    record.key.root_ts.as_str(),
                    record.reply_count,
                    record.unread_reply_count(),
                ))
            } else {
                None
            }
        })
    }

    pub(crate) fn upsert_records(&mut self, records: impl IntoIterator<Item = ThreadRecord>) {
        for record in records {
            self.records.insert(record.key.clone(), record);
        }
    }

    pub(crate) fn get(&self, channel_id: &str, root_ts: &str) -> Option<&ThreadRecord> {
        ThreadKey::new(channel_id, root_ts).and_then(|key| self.records.get(&key))
    }

    pub(crate) fn mark_read(&mut self, channel_id: &str, root_ts: &str) -> bool {
        let Some(key) = ThreadKey::new(channel_id, root_ts) else {
            return false;
        };
        if let Some(record) = self.records.get_mut(&key) {
            record.mark_read();
            true
        } else {
            false
        }
    }

    /// Build the thread-inbox projection from locally observed roots and persisted Slack
    /// metadata. Catalog records win because they carry the most complete reply data.
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
    pub(crate) fn observe_history(
        &mut self,
        channel_id: &str,
        messages: &[SlackMessage],
    ) -> Vec<ThreadRecord> {
        let mut changed_keys = HashSet::new();
        for message in messages {
            if let Some((key, changed)) = self.observe_message(channel_id, message, false) {
                if changed {
                    changed_keys.insert(key);
                }
            }
        }
        changed_keys
            .into_iter()
            .filter_map(|key| self.records.get(&key).cloned())
            .collect()
    }

    /// Applies replies from `conversations.replies`. `complete` means every
    /// page was collected, so the observed replies are an exact reply count.
    pub(crate) fn observe_thread(
        &mut self,
        channel_id: &str,
        root_ts: &str,
        messages: &[SlackMessage],
        complete: bool,
    ) -> Option<ThreadRecord> {
        let key = ThreadKey::new(channel_id, root_ts)?;
        let previous = self.records.get(&key).cloned();
        self.records
            .entry(key.clone())
            .or_insert_with(|| ThreadRecord::placeholder(key.clone()));
        for message in messages {
            self.observe_message(channel_id, message, true);
        }

        let record = self.records.get_mut(&key)?;
        if complete {
            record.reply_count = record.reply_count.max(record.seen_reply_ts.len() as u64);
        }
        if previous.as_ref() != self.records.get(&key) {
            self.records.get(&key).cloned()
        } else {
            None
        }
    }

    /// Applies a realtime message, counting each new reply exactly once.
    pub(crate) fn observe_realtime(
        &mut self,
        channel_id: &str,
        message: &SlackMessage,
        current_user_id: Option<&str>,
    ) -> Option<ThreadRecord> {
        let Some(root_ts) = reply_root_ts(message) else {
            let (key, changed) = self.observe_message(channel_id, message, false)?;
            return changed.then(|| self.records.get(&key).cloned()).flatten();
        };
        let key = ThreadKey::new(channel_id, root_ts)?;
        let previous = self.records.get(&key).cloned();
        let (duplicate, previous_reply_count) = previous
            .as_ref()
            .map(|record| {
                (
                    record.seen_reply_ts.contains(&message.ts)
                        || (record.seen_reply_ts.is_empty()
                            && record.latest_reply.as_deref().is_some_and(|latest| {
                                !slack_timestamp_is_after(&message.ts, latest)
                            })),
                    record.reply_count,
                )
            })
            .unwrap_or((false, 0));
        self.observe_message(channel_id, message, true);
        if duplicate || message.user.as_deref() == current_user_id {
            return if previous.as_ref() != self.records.get(&key) {
                self.records.get(&key).cloned()
            } else {
                None
            };
        }
        let record = self.records.get_mut(&key)?;
        record.reply_count = record
            .reply_count
            .max(previous_reply_count.saturating_add(1));
        if previous.as_ref() != self.records.get(&key) {
            self.records.get(&key).cloned()
        } else {
            None
        }
    }

    fn observe_message(
        &mut self,
        channel_id: &str,
        message: &SlackMessage,
        thread_response: bool,
    ) -> Option<(ThreadKey, bool)> {
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
            return None;
        };
        let key = ThreadKey::new(channel_id, root_ts)?;
        let previous = self.records.get(&key).cloned();
        let record = self
            .records
            .entry(key.clone())
            .or_insert_with(|| ThreadRecord::placeholder(key.clone()));
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
            if record
                .latest_reply
                .as_deref()
                .is_none_or(|latest| slack_timestamp_is_after(&message.ts, latest))
            {
                record.latest_reply = Some(message.ts.clone());
            }
        }
        let changed = previous.as_ref() != self.records.get(&key);
        Some((key, changed))
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
    let incoming_latest_is_newer =
        match (record.latest_reply.as_deref(), root.latest_reply.as_deref()) {
            (None, Some(_)) => true,
            (Some(current), Some(incoming)) => slack_timestamp_is_after(incoming, current),
            _ => false,
        };
    if incoming_latest_is_newer {
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
    record.root = Some(root.clone());
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
    fn explicit_metadata_supplies_subscription_and_latest_reply() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 2);
        root.subscribed = Some(true);
        root.latest_reply = Some("3.0".into());
        catalog.observe_thread("C1", "1.0", &[root], false);
        let record = catalog.get("C1", "1.0").unwrap();
        assert!(record.is_known_subscribed());
        assert_eq!(record.latest_reply.as_deref(), Some("3.0"));
        assert_eq!(record.reply_count, 2);
    }

    #[test]
    fn realtime_replies_count_each_reply_once() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 1);
        root.subscribed = Some(true);
        catalog.observe_thread("C1", "1.0", &[root], false);
        let reply = reply("2.0", "1.0", "U2");
        catalog.observe_realtime("C1", &reply, Some("ME"));
        catalog.observe_realtime("C1", &reply, Some("ME"));
        assert_eq!(catalog.get("C1", "1.0").unwrap().reply_count, 2);
    }

    #[test]
    fn realtime_deduplication_does_not_drop_out_of_order_replies() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 0);
        root.subscribed = Some(true);
        catalog.observe_thread("C1", "1.0", &[root], false);

        catalog.observe_realtime("C1", &reply("3.0", "1.0", "U2"), Some("ME"));
        catalog.observe_realtime("C1", &reply("2.0", "1.0", "U3"), Some("ME"));

        let record = catalog.get("C1", "1.0").unwrap();
        assert_eq!(record.reply_count, 2);
        assert!(record.has_seen_reply("2.0"));
        assert!(record.has_seen_reply("3.0"));
    }

    #[test]
    fn complete_pagination_counts_replies_observed_across_pages() {
        let mut catalog = ThreadCatalog::default();
        let mut root = root("1.0", 3);
        root.subscribed = Some(true);
        catalog.observe_thread("C1", "1.0", &[root, reply("3.0", "1.0", "U2")], false);
        catalog.observe_thread(
            "C1",
            "1.0",
            &[reply("2.0", "1.0", "U3"), reply("1.4", "1.0", "U4")],
            true,
        );
        assert_eq!(catalog.get("C1", "1.0").unwrap().reply_count, 3);
    }

    #[test]
    fn unread_reply_count_is_accurate_on_cold_start_even_though_seen_reply_ts_is_empty() {
        // A thread seen only via channel history (never streamed over realtime
        // this session) has an empty `seen_reply_ts`: deriving unread count
        // from that set would wrongly report 0 unread here. reply_count minus
        // read_reply_count must still report all 20 as unread.
        let mut catalog = ThreadCatalog::default();
        catalog.observe_history("C1", &[root("1.0", 20)]);
        let record = catalog.get("C1", "1.0").unwrap();
        assert!(!record.has_seen_reply("x"));
        assert_eq!(record.unread_reply_count(), 20);
    }

    #[test]
    fn mark_read_zeroes_unread_count_and_a_later_reply_shows_one_unread() {
        let mut record = ThreadRecord::placeholder(ThreadKey::new("C1", "1.0").unwrap());
        record.reply_count = 5;
        record.latest_reply = Some("5.0".into());
        assert_eq!(record.unread_reply_count(), 5);

        record.mark_read();
        assert_eq!(record.unread_reply_count(), 0);
        assert_eq!(record.last_read.as_deref(), Some("5.0"));
        assert_eq!(record.read_reply_count, 5);

        // A new reply arrives without another mark_read() call.
        record.reply_count = 6;
        record.latest_reply = Some("6.0".into());
        assert_eq!(record.unread_reply_count(), 1);
    }

    #[test]
    fn unread_summaries_for_user_filters_correctly() {
        let mut catalog = ThreadCatalog::default();
        let mut rec1 = ThreadRecord::placeholder(ThreadKey::new("C1", "1.0").unwrap());
        rec1.reply_count = 2;
        rec1.latest_reply = Some("2.0".into());
        rec1.participant_user_ids.insert("U1".into());

        let mut rec2 = ThreadRecord::placeholder(ThreadKey::new("C1", "2.0").unwrap());
        rec2.reply_count = 3;
        rec2.latest_reply = Some("3.0".into());
        rec2.participant_user_ids.insert("U2".into()); // not U1

        let mut rec3 = ThreadRecord::placeholder(ThreadKey::new("C2", "3.0").unwrap());
        rec3.reply_count = 4;
        rec3.latest_reply = Some("4.0".into());
        rec3.participant_user_ids.insert("U1".into());
        rec3.mark_read(); // read

        catalog.upsert_records([rec1, rec2, rec3]);

        let summaries: Vec<_> = catalog.unread_summaries_for_user("U1").collect();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0], ("C1", "1.0", 2, 2));
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
        catalog.observe_thread("C1", "1.0", &[catalog_root], false);

        let mut observed_root = root("1.0", 1);
        observed_root.latest_reply = Some("2.0".into());
        let projection = catalog.inbox_projection(vec![("C1".into(), observed_root)]);

        assert_eq!(projection.len(), 1);
        assert_eq!(projection[0].1.reply_count, Some(3));
        assert_eq!(projection[0].1.latest_reply.as_deref(), Some("4.0"));
    }

    #[test]
    fn canonical_timestamp_comparison_realtime_duplicate() {
        let mut catalog = ThreadCatalog::default();
        let mut root_msg = root("1.0", 1);
        root_msg.subscribed = Some(true);
        root_msg.latest_reply = Some("10.000000".into());
        catalog.observe_thread("C1", "1.0", &[root_msg], false);

        catalog.observe_realtime("C1", &reply("9.999999", "1.0", "U2"), Some("ME"));
        assert_eq!(catalog.get("C1", "1.0").unwrap().reply_count, 1);
    }

    #[test]
    fn canonical_timestamp_comparison_observe_message_latest() {
        let mut catalog = ThreadCatalog::default();
        let mut root_msg = root("1.0", 1);
        root_msg.latest_reply = Some("10.000000".into());
        catalog.observe_thread("C1", "1.0", &[root_msg], false);

        catalog.observe_message("C1", &reply("9.999999", "1.0", "U2"), true);

        assert_eq!(
            catalog.get("C1", "1.0").unwrap().latest_reply.as_deref(),
            Some("10.000000")
        );
    }

    #[test]
    fn canonical_timestamp_comparison_merge_root_metadata_latest() {
        let mut catalog = ThreadCatalog::default();
        let mut root_msg = root("1.0", 1);
        root_msg.latest_reply = Some("10.000000".into());
        catalog.observe_thread("C1", "1.0", &[root_msg], false);

        let mut root_msg_new = root("1.0", 1);
        root_msg_new.latest_reply = Some("9.999999".into());
        catalog.observe_thread("C1", "1.0", &[root_msg_new], false);

        assert_eq!(
            catalog.get("C1", "1.0").unwrap().latest_reply.as_deref(),
            Some("10.000000")
        );
    }

    #[test]
    fn thread_record_unread_replies_check() {
        let mut record = ThreadRecord::placeholder(ThreadKey::new("C1", "1.0").unwrap());
        assert!(!record.has_unread_replies());

        record.latest_reply = Some("10.000000".to_string());
        assert!(record.has_unread_replies());

        record.last_read = Some("9.000000".to_string());
        assert!(record.has_unread_replies());

        record.last_read = Some("10.000000".to_string());
        assert!(!record.has_unread_replies());

        record.last_read = Some("11.000000".to_string());
        assert!(!record.has_unread_replies());
    }
}
