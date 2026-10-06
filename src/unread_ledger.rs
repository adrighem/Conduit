/* unread_ledger.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Per-conversation read state behind sidebar bold titles and badges.
//!
//! Counts use a two-tier watermark instead of naked integer deltas:
//! - a `client.counts` snapshot is the baseline for the interval
//!   `(last_read, server_latest]`;
//! - live messages newer than that baseline are tracked by timestamp in
//!   `live_mentions`, so edits, deletes and redelivery cannot drift the count.
//!
//! History or backfill inside the baseline never adds to the counts. A reconnect
//! re-snapshot replaces the baseline and prunes the live tier it now covers.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::attention::contains_direct_mention;
use crate::models::{slack_timestamp_is_after, SlackMessage};

/// Subtypes that never make a conversation unread (membership noise).
const SILENT_SUBTYPES: [&str; 6] = [
    "channel_join",
    "channel_leave",
    "group_join",
    "group_leave",
    "message_deleted",
    "message_changed",
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ConversationReadState {
    pub(crate) last_read: Option<String>,
    pub(crate) server_latest: Option<String>,
    pub(crate) server_mention_count: u64,
    pub(crate) server_unread_count: u64,
    pub(crate) server_has_unreads: bool,
    /// Badge-worthy messages newer than the server baseline.
    pub(crate) live_mentions: BTreeSet<String>,
    /// Newest non-badge unread message newer than the server baseline.
    pub(crate) live_latest_unread: Option<String>,
}

/// One conversation entry from Slack's `client.counts`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ServerReadCounts {
    pub(crate) channel_id: String,
    pub(crate) last_read: Option<String>,
    pub(crate) latest: Option<String>,
    pub(crate) mention_count: u64,
    pub(crate) unread_count: u64,
    pub(crate) has_unreads: bool,
}

/// What one message means for its conversation's read state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageSignal {
    /// Does not affect read state; also clears a stale badge for its `ts`.
    Ignore,
    /// Makes the conversation unread (bold) without a badge.
    Unread,
    /// Makes the conversation unread and counts toward the badge.
    Badge,
}

/// Badge rule: DMs and group DMs badge every message from someone else;
/// channels badge only direct @-mentions. Thread replies that are not
/// broadcast to the channel never touch channel read state.
pub(crate) fn message_signal(
    message: &SlackMessage,
    is_direct_message: bool,
    current_user_id: Option<&str>,
) -> MessageSignal {
    let self_authored = message
        .user
        .as_deref()
        .zip(current_user_id)
        .is_some_and(|(author, current)| author == current);
    let silent = message
        .subtype
        .as_deref()
        .is_some_and(|subtype| SILENT_SUBTYPES.contains(&subtype));
    if self_authored || silent || !message.belongs_in_channel_timeline() {
        MessageSignal::Ignore
    } else if is_direct_message || contains_direct_mention(&message.visible_text(), current_user_id)
    {
        MessageSignal::Badge
    } else {
        MessageSignal::Unread
    }
}

fn is_after_watermark(ts: &str, watermark: Option<&str>) -> bool {
    watermark.is_none_or(|watermark| slack_timestamp_is_after(ts, watermark))
}

fn is_at_or_after(ts: &str, watermark: Option<&str>) -> bool {
    watermark.is_none_or(|watermark| !slack_timestamp_is_after(watermark, ts))
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

impl ConversationReadState {
    pub(crate) fn badge_count(&self) -> u64 {
        self.server_mention_count
            .saturating_add(self.live_mentions.len() as u64)
    }

    pub(crate) fn has_unreads(&self) -> bool {
        self.server_has_unreads
            || self.server_mention_count > 0
            || self.server_unread_count > 0
            || !self.live_mentions.is_empty()
            || self.live_latest_unread.is_some()
    }

    /// Live badge-worthy messages may seed state without a baseline (a DM
    /// that appeared after the last snapshot); history may not, because
    /// unsnapshotted backlog would otherwise badge everything.
    fn accepts_new(&self, ts: &str, signal: MessageSignal, live: bool) -> bool {
        let has_baseline = self.last_read.is_some() || self.server_latest.is_some();
        (has_baseline || (live && signal == MessageSignal::Badge))
            && is_after_watermark(ts, self.last_read.as_deref())
            && is_after_watermark(ts, self.server_latest.as_deref())
    }

    /// Records one message observation. Returns whether state changed.
    pub(crate) fn observe(&mut self, ts: &str, signal: MessageSignal, live: bool) -> bool {
        let Some(ts) = non_empty(ts) else {
            return false;
        };
        let mut changed = false;
        if signal != MessageSignal::Badge {
            changed |= self.live_mentions.remove(ts);
        }
        if signal == MessageSignal::Ignore || !self.accepts_new(ts, signal, live) {
            return changed;
        }
        if signal == MessageSignal::Badge {
            changed |= self.live_mentions.insert(ts.to_string());
        } else if is_after_watermark(ts, self.live_latest_unread.as_deref()) {
            self.live_latest_unread = Some(ts.to_string());
            changed = true;
        }
        changed
    }

    /// Advances the read watermark. Stale (older or equal) marks are ignored.
    pub(crate) fn mark_read(&mut self, ts: &str) -> bool {
        let Some(ts) = non_empty(ts) else {
            return false;
        };
        if !is_after_watermark(ts, self.last_read.as_deref()) {
            return false;
        }
        self.last_read = Some(ts.to_string());
        if is_at_or_after(ts, self.server_latest.as_deref()) {
            self.server_mention_count = 0;
            self.server_unread_count = 0;
            self.server_has_unreads = false;
        }
        self.prune_live_through(ts);
        true
    }

    /// Moves the watermark backwards on an explicit "mark unread". The
    /// caller supplies badge-worthy timestamps from loaded history, which
    /// become the live tier on top of an empty server baseline.
    pub(crate) fn mark_unread(
        &mut self,
        ts: &str,
        badge_ts: impl IntoIterator<Item = String>,
    ) -> bool {
        let Some(ts) = non_empty(ts) else {
            return false;
        };
        let before = self.clone();
        self.last_read = Some(ts.to_string());
        self.server_latest = Some(ts.to_string());
        self.server_mention_count = 0;
        self.server_unread_count = 0;
        self.server_has_unreads = true;
        self.live_mentions = badge_ts
            .into_iter()
            .filter(|candidate| slack_timestamp_is_after(candidate, ts))
            .collect();
        self.live_latest_unread = None;
        *self != before
    }

    /// Replaces the server baseline with a fresh `client.counts` entry.
    pub(crate) fn apply_server_counts(&mut self, counts: &ServerReadCounts) -> bool {
        let before = self.clone();
        if let Some(last_read) = counts.last_read.as_deref().and_then(non_empty) {
            if is_after_watermark(last_read, self.last_read.as_deref()) {
                self.last_read = Some(last_read.to_string());
            }
        }
        self.server_latest = counts
            .latest
            .as_deref()
            .and_then(non_empty)
            .map(str::to_string);
        let read_ahead = self.last_read.as_deref().is_some_and(|last_read| {
            self.server_latest
                .as_deref()
                .is_some_and(|latest| !slack_timestamp_is_after(latest, last_read))
        });
        if read_ahead {
            // A local mark newer than the snapshot wins over its counts.
            self.server_mention_count = 0;
            self.server_unread_count = 0;
            self.server_has_unreads = false;
        } else {
            self.server_mention_count = counts.mention_count;
            self.server_unread_count = counts.unread_count;
            self.server_has_unreads = counts.has_unreads;
        }
        if let Some(watermark) = self.server_latest.clone() {
            self.prune_live_through(&watermark);
        }
        if let Some(watermark) = self.last_read.clone() {
            self.prune_live_through(&watermark);
        }
        *self != before
    }

    fn prune_live_through(&mut self, ts: &str) {
        self.live_mentions
            .retain(|candidate| slack_timestamp_is_after(candidate, ts));
        if self
            .live_latest_unread
            .as_deref()
            .is_some_and(|latest| !slack_timestamp_is_after(latest, ts))
        {
            self.live_latest_unread = None;
        }
    }
}

/// Read state for every conversation in the (single) workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UnreadLedger {
    states: HashMap<String, ConversationReadState>,
}

impl UnreadLedger {
    pub(crate) fn from_entries(
        entries: impl IntoIterator<Item = (String, ConversationReadState)>,
    ) -> Self {
        Self {
            states: entries
                .into_iter()
                .filter(|(channel_id, _)| non_empty(channel_id).is_some())
                .collect(),
        }
    }

    /// Entries sorted by channel id, for deterministic patches and storage.
    pub(crate) fn entries(&self) -> Vec<(String, ConversationReadState)> {
        let mut entries = self
            .states
            .iter()
            .map(|(channel_id, state)| (channel_id.clone(), state.clone()))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    pub(crate) fn get(&self, channel_id: &str) -> Option<&ConversationReadState> {
        self.states.get(channel_id)
    }

    pub(crate) fn upsert(&mut self, channel_id: &str, state: ConversationReadState) {
        if non_empty(channel_id).is_some() {
            self.states.insert(channel_id.to_string(), state);
        }
    }

    /// Applies `update` to one conversation's state and returns the new state
    /// only when it changed.
    pub(crate) fn update(
        &mut self,
        channel_id: &str,
        update: impl FnOnce(&mut ConversationReadState) -> bool,
    ) -> Option<ConversationReadState> {
        let channel_id = non_empty(channel_id)?;
        let mut state = self.states.get(channel_id).cloned().unwrap_or_default();
        if !update(&mut state) {
            return None;
        }
        self.states.insert(channel_id.to_string(), state.clone());
        Some(state)
    }

    /// Applies a full `client.counts` snapshot and returns the changed ids.
    pub(crate) fn apply_server_snapshot(&mut self, counts: &[ServerReadCounts]) -> Vec<String> {
        let mut changed = counts
            .iter()
            .filter_map(|entry| {
                self.update(&entry.channel_id, |state| state.apply_server_counts(entry))
                    .map(|_| entry.channel_id.trim().to_string())
            })
            .collect::<Vec<_>>();
        changed.sort();
        changed.dedup();
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(ts: &str, user: &str, text: &str) -> SlackMessage {
        let mut message = SlackMessage {
            ts: ts.to_string(),
            user: Some(user.to_string()),
            text: Some(text.to_string()),
            ..Default::default()
        };
        message.refresh_canonical_content();
        message
    }

    fn baseline(last_read: &str, latest: &str, mentions: u64) -> ConversationReadState {
        let mut state = ConversationReadState::default();
        state.apply_server_counts(&ServerReadCounts {
            channel_id: "C1".to_string(),
            last_read: Some(last_read.to_string()),
            latest: Some(latest.to_string()),
            mention_count: mentions,
            unread_count: 0,
            has_unreads: mentions > 0,
        });
        state
    }

    #[test]
    fn direct_message_from_someone_else_badges_but_self_authored_does_not() {
        let other = message("2.0", "U2", "hi");
        let own = message("3.0", "U1", "hi");
        assert_eq!(
            message_signal(&other, true, Some("U1")),
            MessageSignal::Badge
        );
        assert_eq!(
            message_signal(&own, true, Some("U1")),
            MessageSignal::Ignore
        );

        let mut state = baseline("1.0", "1.0", 0);
        assert!(state.observe(&other.ts, MessageSignal::Badge, true));
        assert!(!state.observe(&own.ts, MessageSignal::Ignore, true));
        assert_eq!(state.badge_count(), 1);
    }

    #[test]
    fn channel_message_without_mention_is_unread_but_not_badged() {
        let plain = message("2.0", "U2", "hello all");
        let mention = message("3.0", "U2", "hey <@U1>");
        assert_eq!(
            message_signal(&plain, false, Some("U1")),
            MessageSignal::Unread
        );
        assert_eq!(
            message_signal(&mention, false, Some("U1")),
            MessageSignal::Badge
        );

        let mut state = baseline("1.0", "1.0", 0);
        assert!(state.observe(&plain.ts, MessageSignal::Unread, true));
        assert!(state.has_unreads());
        assert_eq!(state.badge_count(), 0);
        assert!(state.observe(&mention.ts, MessageSignal::Badge, true));
        assert_eq!(state.badge_count(), 1);
    }

    #[test]
    fn unbroadcast_thread_replies_and_joins_are_silent() {
        let mut reply = message("3.0", "U2", "<@U1>");
        reply.thread_ts = Some("2.0".to_string());
        assert_eq!(
            message_signal(&reply, true, Some("U1")),
            MessageSignal::Ignore
        );
        reply.subtype = Some("thread_broadcast".to_string());
        assert_eq!(
            message_signal(&reply, false, Some("U1")),
            MessageSignal::Badge
        );

        let mut join = message("4.0", "U2", "joined");
        join.subtype = Some("channel_join".to_string());
        assert_eq!(
            message_signal(&join, true, Some("U1")),
            MessageSignal::Ignore
        );
    }

    #[test]
    fn backfill_inside_the_baseline_never_double_counts() {
        let mut state = baseline("1.0", "5.0", 2);
        assert!(!state.observe("3.0", MessageSignal::Badge, false));
        assert!(!state.observe("4.0", MessageSignal::Badge, true));
        assert_eq!(state.badge_count(), 2);

        assert!(state.observe("6.0", MessageSignal::Badge, true));
        assert!(!state.observe("6.0", MessageSignal::Badge, true));
        assert_eq!(state.badge_count(), 3);
    }

    #[test]
    fn history_without_a_baseline_is_ignored_but_live_badges_count() {
        let mut state = ConversationReadState::default();
        assert!(!state.observe("2.0", MessageSignal::Badge, false));
        assert!(!state.observe("2.0", MessageSignal::Unread, true));
        assert!(state.observe("2.0", MessageSignal::Badge, true));
        assert_eq!(state.badge_count(), 1);
    }

    #[test]
    fn edits_removing_a_mention_and_deletes_clear_the_live_badge() {
        let mut state = baseline("1.0", "1.0", 0);
        state.observe("2.0", MessageSignal::Badge, true);
        state.observe("3.0", MessageSignal::Badge, true);

        assert!(state.observe("2.0", MessageSignal::Unread, true));
        assert!(state.observe("3.0", MessageSignal::Ignore, true));
        assert_eq!(state.badge_count(), 0);
        assert!(state.has_unreads());
    }

    #[test]
    fn marks_clear_counts_and_stale_marks_are_ignored() {
        let mut state = baseline("1.0", "5.0", 2);
        state.observe("6.0", MessageSignal::Badge, true);
        state.observe("7.0", MessageSignal::Badge, true);

        assert!(state.mark_read("6.0"));
        assert_eq!(state.badge_count(), 1);
        assert!(!state.mark_read("4.0"));
        assert_eq!(state.last_read.as_deref(), Some("6.0"));

        assert!(state.mark_read("7.0"));
        assert_eq!(state.badge_count(), 0);
        assert!(!state.has_unreads());
    }

    #[test]
    fn partial_mark_inside_the_baseline_keeps_server_counts() {
        let mut state = baseline("1.0", "5.0", 2);
        assert!(state.mark_read("3.0"));
        assert_eq!(state.badge_count(), 2);
    }

    #[test]
    fn mark_unread_moves_the_watermark_back_and_seeds_badges() {
        let mut state = baseline("9.0", "9.0", 0);
        assert!(state.mark_unread("4.0", ["3.0".to_string(), "5.0".to_string()]));
        assert_eq!(state.last_read.as_deref(), Some("4.0"));
        assert!(state.has_unreads());
        assert_eq!(state.badge_count(), 1);
    }

    #[test]
    fn snapshot_prunes_live_entries_it_now_covers() {
        let mut state = baseline("1.0", "1.0", 0);
        state.observe("2.0", MessageSignal::Badge, true);
        state.observe("3.0", MessageSignal::Badge, true);
        state.apply_server_counts(&ServerReadCounts {
            channel_id: "C1".to_string(),
            last_read: Some("1.0".to_string()),
            latest: Some("2.0".to_string()),
            mention_count: 1,
            unread_count: 0,
            has_unreads: true,
        });
        assert_eq!(state.badge_count(), 2);
        assert_eq!(state.live_mentions.iter().collect::<Vec<_>>(), vec!["3.0"]);
    }

    #[test]
    fn snapshot_older_than_a_local_mark_does_not_resurrect_counts() {
        let mut state = baseline("1.0", "5.0", 2);
        state.mark_read("5.0");
        state.apply_server_counts(&ServerReadCounts {
            channel_id: "C1".to_string(),
            last_read: Some("1.0".to_string()),
            latest: Some("5.0".to_string()),
            mention_count: 2,
            unread_count: 0,
            has_unreads: true,
        });
        assert_eq!(state.last_read.as_deref(), Some("5.0"));
        assert_eq!(state.badge_count(), 0);
        assert!(!state.has_unreads());
    }

    #[test]
    fn ledger_snapshot_reports_only_changed_conversations() {
        let mut ledger = UnreadLedger::default();
        let counts = vec![ServerReadCounts {
            channel_id: "D1".to_string(),
            last_read: Some("1.0".to_string()),
            latest: Some("2.0".to_string()),
            mention_count: 1,
            unread_count: 1,
            has_unreads: true,
        }];
        assert_eq!(
            ledger.apply_server_snapshot(&counts),
            vec!["D1".to_string()]
        );
        assert!(ledger.apply_server_snapshot(&counts).is_empty());
        assert_eq!(
            ledger.get("D1").map(ConversationReadState::badge_count),
            Some(1)
        );
    }
}
