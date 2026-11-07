//! Activity feed: mentions of the user and reactions to their messages, plus
//! followed threads derived from the thread catalog at render time.
//!
//! Everything here is GTK-free. Only mentions and reactions are stored (and
//! persisted); thread rows and every unread flag are derived per render from
//! the thread catalog and the unread ledger, never stored.

use serde::{Deserialize, Serialize};

use crate::models::slack_timestamp_is_after;
use crate::thread_catalog::{ThreadCatalog, ThreadRecord};
use crate::unread_ledger::UnreadLedger;

mod observe;

pub(crate) use observe::{backfill_is_due, items_from_search, mention_item, observe_socket_event};
pub use observe::{ActivityObservation, ReactionChange};

/// Stored mention and reaction items; the oldest fall off first.
pub(crate) const MAX_STORED_ITEMS: usize = 200;
/// Followed threads shown at once, newest activity first.
const MAX_THREAD_ITEMS: usize = 100;
/// Snippet characters kept per item; the row clamps it to two lines anyway.
const MAX_TEXT_CHARS: usize = 280;
/// The `search.messages` backfill is skipped while the last one is younger.
pub(crate) const BACKFILL_MAX_AGE_MS: i64 = 15 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivityKind {
    Mention {
        /// `@channel`, `@here` or `@everyone` rather than a direct mention.
        #[serde(default)]
        broadcast: bool,
        /// The user group (`<!subteam^ID>`) that reached the user.
        #[serde(default)]
        group: Option<String>,
    },
    /// Everyone who reacted to one of the user's messages with one emoji.
    Reaction { name: String, user_ids: Vec<String> },
    /// Derived from the thread catalog; never stored.
    ThreadReply,
}

/// The part of an item's key that tells its kinds apart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum KindTag {
    Mention,
    Reaction(String),
    ThreadReply,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActivityKey {
    pub(crate) channel_id: String,
    pub(crate) ts: String,
    pub(crate) kind: KindTag,
}

impl ActivityKey {
    /// Stable key of the persisted row.
    pub(crate) fn storage_key(&self) -> String {
        let kind = match &self.kind {
            KindTag::Mention => "m".to_string(),
            KindTag::Reaction(name) => format!("r:{name}"),
            KindTag::ThreadReply => "t".to_string(),
        };
        format!("{}:{}:{kind}", self.channel_id, self.ts)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityItem {
    pub(crate) channel_id: String,
    /// The mentioning or reacted-to message (a thread's root for threads).
    pub(crate) ts: String,
    pub(crate) kind: ActivityKind,
    pub(crate) author_id: Option<String>,
    /// Raw Slack text; mentions resolve when the row is rendered.
    pub(crate) text: String,
    pub(crate) thread_ts: Option<String>,
    /// When it happened: the message time, or the latest reaction or reply.
    pub(crate) timestamp: String,
    /// Other thread participants, for the "+N" of derived thread items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) participants: Vec<String>,
    /// `timestamp` parsed once, so sorting never reparses it.
    #[serde(skip)]
    order: (u64, u64),
}

impl ActivityItem {
    pub(crate) fn new(channel_id: &str, ts: &str, kind: ActivityKind, timestamp: &str) -> Self {
        Self {
            channel_id: channel_id.to_string(),
            ts: ts.to_string(),
            kind,
            author_id: None,
            text: String::new(),
            thread_ts: None,
            timestamp: timestamp.to_string(),
            participants: Vec::new(),
            order: sort_key(timestamp),
        }
    }

    pub(crate) fn with_text(mut self, text: &str) -> Self {
        self.text = text.chars().take(MAX_TEXT_CHARS).collect();
        self
    }

    pub(crate) fn key(&self) -> ActivityKey {
        ActivityKey {
            channel_id: self.channel_id.clone(),
            ts: self.ts.clone(),
            kind: match &self.kind {
                ActivityKind::Mention { .. } => KindTag::Mention,
                ActivityKind::Reaction { name, .. } => KindTag::Reaction(name.clone()),
                ActivityKind::ThreadReply => KindTag::ThreadReply,
            },
        }
    }
}

/// `1700000000.000200` as comparable (seconds, microseconds).
pub(crate) fn sort_key(ts: &str) -> (u64, u64) {
    let (seconds, fraction) = ts.split_once('.').unwrap_or((ts, ""));
    let fraction = format!("{fraction:0<6}");
    (
        seconds.parse().unwrap_or_default(),
        fraction[..6].parse().unwrap_or_default(),
    )
}

/// Stored items, newest first, at most [`MAX_STORED_ITEMS`], one per key.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ActivityFeed {
    items: Vec<ActivityItem>,
}

impl ActivityFeed {
    pub(crate) fn items(&self) -> &[ActivityItem] {
        &self.items
    }

    /// Merges `items` (persisted or backfilled); returns whether the feed changed.
    pub(crate) fn merge(&mut self, items: impl IntoIterator<Item = ActivityItem>) -> bool {
        items
            .into_iter()
            .filter(|item| item.kind != ActivityKind::ThreadReply)
            .map(|mut item| {
                item.order = sort_key(&item.timestamp);
                item
            })
            .fold(false, |changed, item| self.upsert(item) | changed)
    }

    /// Records `user_id` adding or removing `name` on the message `ts`. An
    /// item whose last reactor left is dropped.
    pub(crate) fn reaction_changed(
        &mut self,
        reaction: &ReactionChange,
        text: &str,
        now_ts: &str,
    ) -> bool {
        let kind = ActivityKind::Reaction {
            name: reaction.name.clone(),
            user_ids: vec![reaction.user_id.clone()],
        };
        let mut item =
            ActivityItem::new(&reaction.channel_id, &reaction.ts, kind, now_ts).with_text(text);
        if reaction.added {
            item.author_id = Some(reaction.user_id.clone());
            return self.upsert(item);
        }
        let key = item.key();
        let Some(position) = self.items.iter().position(|item| item.key() == key) else {
            return false;
        };
        let ActivityKind::Reaction { user_ids, .. } = &mut self.items[position].kind else {
            return false;
        };
        let before = user_ids.len();
        user_ids.retain(|user_id| *user_id != reaction.user_id);
        let emptied = user_ids.is_empty();
        let changed = user_ids.len() != before;
        if emptied {
            self.items.remove(position);
        }
        changed
    }

    fn upsert(&mut self, item: ActivityItem) -> bool {
        let key = item.key();
        let Some(position) = self.items.iter().position(|item| item.key() == key) else {
            let at = self.items.partition_point(|kept| kept.order >= item.order);
            self.items.insert(at, item);
            self.items.truncate(MAX_STORED_ITEMS);
            return self.items.iter().any(|kept| kept.key() == key);
        };
        let mut merged = self.items[position].clone();
        if let (
            ActivityKind::Reaction { user_ids, .. },
            ActivityKind::Reaction {
                user_ids: incoming, ..
            },
        ) = (&mut merged.kind, &item.kind)
        {
            let new = incoming
                .iter()
                .filter(|user_id| !user_ids.contains(user_id))
                .cloned()
                .collect::<Vec<_>>();
            if !new.is_empty() {
                user_ids.extend(new);
                merged.author_id = item.author_id.clone().or(merged.author_id);
                if item.order > merged.order {
                    merged.timestamp = item.timestamp.clone();
                    merged.order = item.order;
                }
            }
        }
        if merged.text.is_empty() {
            merged.text = item.text;
        }
        if merged == self.items[position] {
            return false;
        }
        self.items.remove(position);
        let at = self
            .items
            .partition_point(|kept| kept.order >= merged.order);
        self.items.insert(at, merged);
        true
    }
}

/// Which items the header toggles show; neither toggle active shows all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ActivityFilter {
    #[default]
    All,
    Mentions,
    Threads,
}

/// An item plus its derived read state; `unread` is 0 when read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActivityEntry {
    pub(crate) item: ActivityItem,
    pub(crate) unread: u64,
}

impl ActivityEntry {
    fn admitted(&self, filter: ActivityFilter, unread_only: bool) -> bool {
        let kind_ok = match filter {
            ActivityFilter::All => true,
            ActivityFilter::Mentions => matches!(self.item.kind, ActivityKind::Mention { .. }),
            ActivityFilter::Threads => self.item.kind == ActivityKind::ThreadReply,
        };
        kind_ok && (!unread_only || self.unread > 0)
    }
}

/// Mentions and reactions are unread when they came after the conversation's
/// read marker; a conversation without read state counts as read.
fn is_unread(item: &ActivityItem, ledger: &UnreadLedger) -> bool {
    ledger.get(&item.channel_id).is_some_and(|state| {
        state
            .last_read
            .as_deref()
            .is_none_or(|last_read| slack_timestamp_is_after(&item.timestamp, last_read))
    })
}

/// Unread mention and reaction items: the Activity rail badge.
pub(crate) fn badge_count(feed: &ActivityFeed, ledger: &UnreadLedger) -> usize {
    feed.items()
        .iter()
        .filter(|item| is_unread(item, ledger))
        .count()
}

fn thread_entry(record: &ThreadRecord, me: &str) -> Option<ActivityEntry> {
    let root = record.root.as_ref()?;
    let latest = record.latest_reply.as_deref()?;
    let involved = record.subscribed == Some(true)
        || record.participant_user_ids.contains(me)
        || root.user.as_deref() == Some(me);
    if record.subscribed == Some(false) || !involved {
        return None;
    }
    let mut others = record
        .participant_user_ids
        .iter()
        .filter(|id| id.as_str() != me)
        .cloned()
        .collect::<Vec<_>>();
    others.sort();
    let author = root
        .user
        .clone()
        .filter(|id| id != me)
        .or_else(|| others.first().cloned());
    others.retain(|id| Some(id) != author.as_ref());
    let mut item = ActivityItem::new(
        &record.key.channel_id,
        &record.key.root_ts,
        ActivityKind::ThreadReply,
        latest,
    )
    .with_text(&root.visible_text());
    item.author_id = author;
    item.thread_ts = Some(record.key.root_ts.clone());
    item.participants = others;
    let unread = if record.has_unread_replies() {
        record.unread_reply_count().max(1)
    } else {
        0
    };
    Some(ActivityEntry { item, unread })
}

/// Every Activity entry, newest first: stored items with their derived read
/// state plus the user's threads from the catalog.
pub(crate) fn entries(
    feed: &ActivityFeed,
    ledger: &UnreadLedger,
    threads: &ThreadCatalog,
    me: Option<&str>,
) -> Vec<ActivityEntry> {
    let mut thread_entries = me
        .map(|me| {
            threads
                .records()
                .filter_map(|record| thread_entry(record, me))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    thread_entries.sort_by_key(|entry| std::cmp::Reverse(entry.item.order));
    thread_entries.truncate(MAX_THREAD_ITEMS);
    let mut all = feed
        .items()
        .iter()
        .map(|item| ActivityEntry {
            unread: u64::from(is_unread(item, ledger)),
            item: item.clone(),
        })
        .chain(thread_entries)
        .collect::<Vec<_>>();
    all.sort_by_key(|entry| std::cmp::Reverse(entry.item.order));
    all
}

/// Unread mention items and unread threads: the header toggle counts.
pub(crate) fn header_counts(entries: &[ActivityEntry]) -> (usize, usize) {
    let unread = |kind: fn(&ActivityKind) -> bool| {
        entries
            .iter()
            .filter(|entry| entry.unread > 0 && kind(&entry.item.kind))
            .count()
    };
    (
        unread(|kind| matches!(kind, ActivityKind::Mention { .. })),
        unread(|kind| *kind == ActivityKind::ThreadReply),
    )
}

pub(crate) fn filter_entries(
    entries: Vec<ActivityEntry>,
    filter: ActivityFilter,
    unread_only: bool,
) -> Vec<ActivityEntry> {
    entries
        .into_iter()
        .filter(|entry| entry.admitted(filter, unread_only))
        .collect()
}

#[cfg(test)]
mod tests;
