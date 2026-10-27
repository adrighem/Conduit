use std::cell::{Cell, RefCell};

use gtk::glib;
use gtk::glib::subclass::prelude::*;

use crate::models::SlackMessage;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TimelineMessageObject {
        pub message: RefCell<SlackMessage>,
        /// True for a day-separator row; `message` is then the first message
        /// of the day it introduces (only its `ts` is used).
        pub day_separator: Cell<bool>,
        pub hide_header: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TimelineMessageObject {
        const NAME: &'static str = "ConduitTimelineMessageObject";
        type Type = super::TimelineMessageObject;
        type ParentType = glib::Object;
    }

    impl ObjectImpl for TimelineMessageObject {}
}

glib::wrapper! {
    pub struct TimelineMessageObject(ObjectSubclass<imp::TimelineMessageObject>);
}

impl TimelineMessageObject {
    pub fn new(message: SlackMessage) -> Self {
        let obj: Self = glib::Object::builder().build();
        *obj.imp().message.borrow_mut() = message;
        obj
    }

    /// Separator row introducing the day of `first_message_of_day`.
    pub fn day_separator(first_message_of_day: SlackMessage) -> Self {
        let obj = Self::new(first_message_of_day);
        obj.imp().day_separator.set(true);
        obj
    }

    pub fn is_day_separator(&self) -> bool {
        self.imp().day_separator.get()
    }

    pub fn hide_header(&self) -> bool {
        self.imp().hide_header.get()
    }

    pub fn set_hide_header(&self, hide: bool) {
        self.imp().hide_header.set(hide);
    }

    pub fn message(&self) -> SlackMessage {
        self.imp().message.borrow().clone()
    }

    /// Runs `f` on the message without cloning it.
    pub(crate) fn with_message<R>(&self, f: impl FnOnce(&SlackMessage) -> R) -> R {
        f(&self.imp().message.borrow())
    }

    /// Whether both objects render the same row (kind, header, message).
    /// A day separator only shows its day, so its message's `ts` is enough.
    pub(crate) fn same_row(&self, other: &Self) -> bool {
        if self.is_day_separator() || other.is_day_separator() {
            return self.is_day_separator() == other.is_day_separator()
                && self.imp().message.borrow().ts == other.imp().message.borrow().ts;
        }
        self.hide_header() == other.hide_header()
            && self
                .imp()
                .message
                .borrow()
                .renders_same(&other.imp().message.borrow())
    }

    /// Store order: chronological, a day separator before its first message.
    fn order_key(&self) -> (String, bool) {
        (
            self.imp().message.borrow().ts.clone(),
            !self.is_day_separator(),
        )
    }

    /// Takes `other`'s message without rebuilding the row, so a kept row
    /// still holds the freshest copy (e.g. wire fields a cached copy lacks).
    pub(crate) fn adopt_message(&self, other: &Self) {
        let message = other.imp().message.borrow();
        if *self.imp().message.borrow() != *message {
            *self.imp().message.borrow_mut() = message.clone();
        }
    }
}

pub(crate) fn is_system_subtype(message: &SlackMessage) -> bool {
    message.subtype.as_deref().is_some_and(|subtype| {
        matches!(
            subtype,
            "channel_join" | "channel_leave" | "channel_topic" | "channel_purpose"
        )
    })
}

pub(crate) fn is_thread_broadcast(message: &SlackMessage) -> bool {
    message.is_thread_broadcast == Some(true)
        || matches!(
            message.subtype.as_deref(),
            Some("thread_broadcast" | "reply_broadcast")
        )
}

pub(crate) fn sender_key(message: &SlackMessage) -> &str {
    message
        .user
        .as_deref()
        .or(message.username.as_deref())
        .or(message.bot_id.as_deref())
        .unwrap_or("Slack")
}

pub(crate) fn can_group_messages(previous: &SlackMessage, current: &SlackMessage) -> bool {
    if is_system_subtype(previous) || is_system_subtype(current) {
        return false;
    }
    if is_thread_broadcast(previous) || is_thread_broadcast(current) {
        return false;
    }
    if sender_key(previous) != sender_key(current) {
        return false;
    }

    let Some(previous_ts) = previous.ts.parse::<f64>().ok() else {
        return false;
    };
    let Some(current_ts) = current.ts.parse::<f64>().ok() else {
        return false;
    };
    let delta = current_ts - previous_ts;

    (0.0..180.0).contains(&delta)
}

/// One store edit: at `position` (an index into the old items), replace
/// `removed` items with `inserted`.
#[derive(Debug)]
pub(crate) struct StoreSplice {
    pub(crate) position: usize,
    pub(crate) removed: usize,
    pub(crate) inserted: Vec<TimelineMessageObject>,
}

/// Edits turning `old` into `new` that keep every old row rendering the
/// same, so its widget (focus, text selection) survives. Rows are matched by
/// store order, and kept rows adopt the fresh message. Should either list
/// not be strictly ordered, everything between the common prefix and
/// suffix is replaced instead.
pub(crate) fn store_splices(
    old: &[TimelineMessageObject],
    new: &[TimelineMessageObject],
) -> Vec<StoreSplice> {
    let old_keys = old
        .iter()
        .map(TimelineMessageObject::order_key)
        .collect::<Vec<_>>();
    let new_keys = new
        .iter()
        .map(TimelineMessageObject::order_key)
        .collect::<Vec<_>>();
    let ordered = |keys: &[(String, bool)]| keys.windows(2).all(|pair| pair[0] < pair[1]);
    if !ordered(&old_keys) || !ordered(&new_keys) {
        return span_splice(old, new);
    }

    let mut splices = Vec::new();
    let mut pending: Option<StoreSplice> = None;
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        let order = match (old_keys.get(i), new_keys.get(j)) {
            (Some(old_key), Some(new_key)) => old_key.cmp(new_key),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, _) => std::cmp::Ordering::Greater,
        };
        if order == std::cmp::Ordering::Equal && old[i].same_row(&new[j]) {
            old[i].adopt_message(&new[j]);
            splices.extend(pending.take());
            i += 1;
            j += 1;
            continue;
        }
        let splice = pending.get_or_insert_with(|| StoreSplice {
            position: i,
            removed: 0,
            inserted: Vec::new(),
        });
        if order != std::cmp::Ordering::Greater {
            splice.removed += 1;
            i += 1;
        }
        if order != std::cmp::Ordering::Less {
            splice.inserted.push(new[j].clone());
            j += 1;
        }
    }
    splices.extend(pending);
    splices
}

fn span_splice(old: &[TimelineMessageObject], new: &[TimelineMessageObject]) -> Vec<StoreSplice> {
    let max_common = old.len().min(new.len());
    let prefix = (0..max_common)
        .take_while(|&i| old[i].same_row(&new[i]))
        .count();
    let suffix = (0..max_common - prefix)
        .take_while(|&i| old[old.len() - 1 - i].same_row(&new[new.len() - 1 - i]))
        .count();
    let kept = (0..prefix).chain(old.len() - suffix..old.len());
    let kept_new = (0..prefix).chain(new.len() - suffix..new.len());
    for (old_index, new_index) in kept.zip(kept_new) {
        old[old_index].adopt_message(&new[new_index]);
    }
    let removed = old.len() - prefix - suffix;
    let inserted = new[prefix..new.len() - suffix].to_vec();
    if removed == 0 && inserted.is_empty() {
        return Vec::new();
    }
    vec![StoreSplice {
        position: prefix,
        removed,
        inserted,
    }]
}

/// Chronological store items for `messages` (newest first on input): a day
/// separator is inserted wherever the local calendar day changes. The store
/// is only ever rebuilt from the message list (`set_messages`, which keeps
/// unchanged rows via `store_splices`), so separators are derived
/// here once and can never drift out of sync with inserts or prepends.
/// Also returns the index of `focus_ts` within the resulting items.
pub(crate) fn build_store_items(
    messages: &[SlackMessage],
    focus_ts: Option<&str>,
    unread_separator_ts: Option<&str>,
) -> (Vec<TimelineMessageObject>, Option<u32>) {
    let days: Vec<Option<i64>> = messages
        .iter()
        .rev()
        .map(|msg| {
            crate::message_timestamp::slack_ts_datetime(&msg.ts)
                .map(|dt| crate::day_label::local_calendar_day(&dt))
        })
        .collect();
    let mut items = Vec::with_capacity(messages.len() + 4);
    let mut focus_index = None;
    let mut prev_msg: Option<&SlackMessage> = None;
    for (i, msg) in messages.iter().rev().enumerate() {
        if crate::day_label::separator_before(&days, i) {
            items.push(TimelineMessageObject::day_separator(msg.clone()));
            prev_msg = None;
        }
        if focus_ts == Some(msg.ts.as_str()) && focus_index.is_none() {
            focus_index = Some(items.len() as u32);
        }
        let is_unread = unread_separator_ts == Some(msg.ts.as_str());
        let can_group = !is_unread && prev_msg.is_some_and(|prev| can_group_messages(prev, msg));
        let obj = TimelineMessageObject::new(msg.clone());
        if can_group {
            obj.set_hide_header(true);
        }
        items.push(obj);
        if is_system_subtype(msg) || is_thread_broadcast(msg) {
            prev_msg = None;
        } else {
            prev_msg = Some(msg);
        }
    }
    (items, focus_index)
}
