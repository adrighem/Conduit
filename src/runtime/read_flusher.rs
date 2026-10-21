use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{oneshot, Notify};

use crate::models::slack_timestamp_is_after;
use crate::slack::{SlackApi, SlackError, SlackErrorCategory};

use super::wait_for_realtime_or_shutdown;

/// Bound on remembered confirmed cursors. Evicting one only costs a
/// redundant mark later, so the oldest-confirmed target goes first.
pub const READ_FLUSHER_MAX_MARKED_TARGETS: usize = 1_024;

/// What a read mark applies to: a conversation, or one thread inside it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReadTarget {
    pub channel_id: String,
    pub thread_ts: Option<String>,
}

impl ReadTarget {
    pub fn conversation(channel_id: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            thread_ts: None,
        }
    }

    pub fn thread(channel_id: impl Into<String>, thread_ts: impl Into<String>) -> Self {
        Self {
            channel_id: channel_id.into(),
            thread_ts: Some(thread_ts.into()),
        }
    }
}

impl std::fmt::Display for ReadTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "channel={}", self.channel_id)?;
        if let Some(thread_ts) = &self.thread_ts {
            write!(formatter, " thread={thread_ts}")?;
        }
        Ok(())
    }
}

/// Intent behind a mark. A read is only queued if it moves past the latest
/// intended cursor; an unread is a deliberate user rewind that is always
/// queued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadMarkKind {
    Read,
    Unread,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingReadMark {
    pub target: ReadTarget,
    pub target_ts: String,
    pub kind: ReadMarkKind,
}

/// Debounced read marks for conversations and threads alike. Each target
/// has at most one pending and one in-flight mark; `last_marked` mirrors
/// Slack's confirmed cursor.
#[derive(Clone, Debug, Default)]
pub struct ReadFlusherQueue {
    last_marked: HashMap<ReadTarget, String>,
    last_marked_order: VecDeque<ReadTarget>,
    pending: HashMap<ReadTarget, PendingReadMark>,
    in_flight: HashMap<ReadTarget, PendingReadMark>,
}

impl ReadFlusherQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds a known cursor; never moves an existing one backwards.
    #[allow(dead_code)]
    pub fn set_last_marked(&mut self, target: &ReadTarget, ts: &str) {
        if let Some(current) = self.last_marked.get(target) {
            if !slack_timestamp_is_after(ts, current) {
                return;
            }
        }
        self.record_marked(target, ts);
    }

    #[allow(dead_code)]
    pub fn last_marked(&self, target: &ReadTarget) -> Option<&str> {
        self.last_marked.get(target).map(String::as_str)
    }

    #[cfg(test)]
    pub fn last_marked_len(&self) -> usize {
        self.last_marked.len()
    }

    #[allow(dead_code)]
    pub fn pending_mark(&self, target: &ReadTarget) -> Option<&str> {
        self.pending.get(target).map(|mark| mark.target_ts.as_str())
    }

    #[cfg(test)]
    pub fn pending_mark_kind(&self, target: &ReadTarget) -> Option<ReadMarkKind> {
        self.pending.get(target).map(|mark| mark.kind)
    }

    /// The cursor the user most recently asked for: a pending mark is newer
    /// than the in-flight one, which is newer than the last confirmed one.
    /// After a forced unread this can sit behind `last_marked`.
    fn latest_intended_ts(&self, target: &ReadTarget) -> Option<&str> {
        self.pending
            .get(target)
            .or_else(|| self.in_flight.get(target))
            .map(|mark| mark.target_ts.as_str())
            .or_else(|| self.last_marked(target))
    }

    /// Queues a read mark if it moves the latest intended cursor forward.
    pub fn enqueue(&mut self, target: &ReadTarget, target_ts: &str) -> bool {
        if let Some(latest_ts) = self.latest_intended_ts(target) {
            if !slack_timestamp_is_after(target_ts, latest_ts) {
                return false;
            }
        }
        self.insert_pending(target, target_ts, ReadMarkKind::Read);
        true
    }

    /// Queues a user-forced unread mark; it replaces any pending mark and is
    /// never filtered against older, higher read cursors.
    pub fn enqueue_force(&mut self, target: &ReadTarget, target_ts: &str) {
        self.insert_pending(target, target_ts, ReadMarkKind::Unread);
    }

    fn insert_pending(&mut self, target: &ReadTarget, target_ts: &str, kind: ReadMarkKind) {
        self.pending.insert(
            target.clone(),
            PendingReadMark {
                target: target.clone(),
                target_ts: target_ts.to_string(),
                kind,
            },
        );
    }

    /// Drains pending marks, conversations before their threads, in a
    /// stable order. `last_marked` only advances once the caller reports
    /// success through [`Self::mark_succeeded`]. Drained marks stay in
    /// flight until settled so the same target is not enqueued again.
    pub fn flush(&mut self) -> Vec<PendingReadMark> {
        let mut marks: Vec<PendingReadMark> = self.pending.drain().map(|(_, mark)| mark).collect();
        marks.sort_by(|left, right| left.target.cmp(&right.target));
        for mark in &marks {
            self.in_flight.insert(mark.target.clone(), mark.clone());
        }
        marks
    }

    #[allow(dead_code)]
    pub fn flush_target(&mut self, target: &ReadTarget) -> Option<PendingReadMark> {
        let mark = self.pending.remove(target)?;
        self.in_flight.insert(mark.target.clone(), mark.clone());
        Some(mark)
    }

    /// Mirrors Slack's cursor after a confirmed mark, for both kinds.
    ///
    /// Slack sets the cursor absolutely, so the last mark that succeeded is
    /// Slack's cursor even when it sits behind `last_marked` (e.g. a read
    /// that superseded an unsent or failed unread). Ordering is already
    /// enforced upstream: `enqueue` gates reads against the latest intent,
    /// and the flusher settles at most one mark per target at a time, so a
    /// success is never older than a previously settled mark.
    pub fn mark_succeeded(&mut self, mark: &PendingReadMark) {
        self.in_flight.remove(&mark.target);
        self.record_marked(&mark.target, &mark.target_ts);
    }

    /// Puts a failed mark back unless the user queued a newer mark for the
    /// target meanwhile. Anything pending was enqueued after this mark was
    /// drained, so it is the most recent intent and wins regardless of kind
    /// or timestamp order.
    pub fn mark_failed(&mut self, mark: &PendingReadMark) {
        self.in_flight.remove(&mark.target);
        self.pending
            .entry(mark.target.clone())
            .or_insert_with(|| mark.clone());
    }

    /// Gives up on a mark that can never succeed; it is not requeued.
    pub fn mark_dropped(&mut self, mark: &PendingReadMark) {
        self.in_flight.remove(&mark.target);
    }

    fn record_marked(&mut self, target: &ReadTarget, ts: &str) {
        self.last_marked.insert(target.clone(), ts.to_string());
        self.last_marked_order.retain(|queued| queued != target);
        self.last_marked_order.push_back(target.clone());
        while self.last_marked.len() > READ_FLUSHER_MAX_MARKED_TARGETS {
            let Some(evicted) = self.last_marked_order.pop_front() else {
                break;
            };
            self.last_marked.remove(&evicted);
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReadFlusherHandle {
    queue: Arc<Mutex<ReadFlusherQueue>>,
    notify: Arc<Notify>,
}

impl ReadFlusherHandle {
    pub fn new(queue: Arc<Mutex<ReadFlusherQueue>>, notify: Arc<Notify>) -> Self {
        Self { queue, notify }
    }

    pub fn enqueue(&self, target: &ReadTarget, target_ts: &str) -> bool {
        let updated = self.lock().enqueue(target, target_ts);
        if updated {
            self.notify.notify_one();
        }
        updated
    }

    pub fn enqueue_force(&self, target: &ReadTarget, target_ts: &str) {
        self.lock().enqueue_force(target, target_ts);
        self.notify.notify_one();
    }

    pub fn flush(&self) -> Vec<PendingReadMark> {
        self.lock().flush()
    }

    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, ReadFlusherQueue> {
        self.queue.lock().expect("read flusher queue lock poisoned")
    }
}

/// Sends one mark to the Slack method matching its target, retrying a few
/// times on rate limits.
pub async fn execute_read_flusher_mark(api: &SlackApi, mark: &PendingReadMark) -> Result<()> {
    let channel_id = mark.target.channel_id.as_str();
    let mut retries = 0usize;
    loop {
        let result = match mark.target.thread_ts.as_deref() {
            None => api.conversations_mark(channel_id, &mark.target_ts).await,
            Some(thread_ts) => {
                api.subscriptions_thread_mark(channel_id, thread_ts, &mark.target_ts)
                    .await
            }
        };
        match result {
            Ok(()) => return Ok(()),
            Err(slack_err) => {
                if slack_err.category() == SlackErrorCategory::RateLimited && retries < 3 {
                    retries += 1;
                    tokio::time::sleep(Duration::from_millis(500 * (1 << retries))).await;
                    continue;
                }
                return Err(anyhow::Error::from(slack_err));
            }
        }
    }
}

/// First retry delay after a failed mark; doubles per failed round.
const READ_FLUSHER_RETRY_INITIAL: Duration = Duration::from_secs(2);
const READ_FLUSHER_RETRY_MAX: Duration = Duration::from_secs(60);

/// Whether retrying a failed mark can never succeed (bad channel, auth,
/// scope, timestamp). Connectivity, rate limits and server errors are transient.
pub(super) fn read_mark_error_is_permanent(error: &anyhow::Error) -> bool {
    let Some(slack_error) = error.downcast_ref::<SlackError>() else {
        return false;
    };
    match slack_error {
        SlackError::Api { code, .. } => !matches!(
            code.as_str(),
            "internal_error"
                | "fatal_error"
                | "service_unavailable"
                | "request_timeout"
                | "ratelimited"
                | "rate_limited"
        ),
        SlackError::Validation { .. } | SlackError::InvalidParameters => true,
        SlackError::RateLimited { .. } => false,
        SlackError::Other(_) => slack_error.category() == SlackErrorCategory::Authentication,
    }
}

fn log_read_mark_error(mark: &PendingReadMark, outcome: &str, error: &anyhow::Error) {
    crate::debug::log(
        "read_flusher",
        &format!("read mark {outcome} {} error={error}", mark.target),
    );
}

/// Sends every pending mark once. Failed marks go back into the queue and
/// `last_marked` only advances on success. Returns whether any mark failed.
pub(super) async fn flush_read_marks(handle: &ReadFlusherHandle, api: &SlackApi) -> bool {
    let mut failed = false;
    for mark in handle.flush() {
        match execute_read_flusher_mark(api, &mark).await {
            Ok(()) => handle.lock().mark_succeeded(&mark),
            Err(error) if read_mark_error_is_permanent(&error) => {
                handle.lock().mark_dropped(&mark);
                log_read_mark_error(&mark, "dropped", &error);
            }
            Err(error) => {
                failed = true;
                handle.lock().mark_failed(&mark);
                log_read_mark_error(&mark, "failed", &error);
            }
        }
    }
    failed
}

pub async fn run_read_flusher_loop(
    handle: ReadFlusherHandle,
    api: SlackApi,
    debounce_delay: Duration,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut retry_delay: Option<Duration> = None;
    loop {
        // Requeued failures retry after a backoff even without new marks.
        let woken = match retry_delay {
            Some(delay) => {
                let mut notified = std::pin::pin!(handle.notify.notified());
                let mut retry = std::pin::pin!(tokio::time::sleep(delay));
                let either = std::future::poll_fn(|context| {
                    if notified.as_mut().poll(context).is_ready()
                        || retry.as_mut().poll(context).is_ready()
                    {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                });
                wait_for_realtime_or_shutdown(&mut shutdown, either).await
            }
            None => wait_for_realtime_or_shutdown(&mut shutdown, handle.notify.notified()).await,
        };
        if woken.is_none() {
            flush_read_marks(&handle, &api).await;
            break;
        }

        if wait_for_realtime_or_shutdown(&mut shutdown, tokio::time::sleep(debounce_delay))
            .await
            .is_none()
        {
            flush_read_marks(&handle, &api).await;
            break;
        }

        retry_delay = if flush_read_marks(&handle, &api).await {
            Some(retry_delay.map_or(READ_FLUSHER_RETRY_INITIAL, |delay| {
                (delay * 2).min(READ_FLUSHER_RETRY_MAX)
            }))
        } else {
            None
        };
    }
}
