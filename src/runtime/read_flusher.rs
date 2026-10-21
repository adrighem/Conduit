use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{oneshot, Notify};

use crate::models::slack_timestamp_is_after;
use crate::slack::{SlackApi, SlackError, SlackErrorCategory};

use super::wait_for_realtime_or_shutdown;

pub const READ_FLUSHER_MAX_MARKED_THREADS: usize = 1_024;

/// Intent behind a channel mark. A read is only queued if it moves past the
/// latest intended cursor; an unread is a deliberate user rewind that is
/// always queued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadMarkKind {
    Read,
    Unread,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingReadMark {
    pub channel_id: String,
    pub target_ts: String,
    pub kind: ReadMarkKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingThreadReadMark {
    pub channel_id: String,
    pub thread_ts: String,
    pub target_ts: String,
}

#[derive(Clone, Debug, Default)]
pub struct ReadFlusherQueue {
    last_marked: HashMap<String, String>,
    pending: HashMap<String, PendingReadMark>,
    last_marked_threads: HashMap<(String, String), String>,
    last_marked_thread_order: VecDeque<(String, String)>,
    pending_threads: HashMap<(String, String), String>,
    in_flight: HashMap<String, PendingReadMark>,
    in_flight_threads: HashMap<(String, String), String>,
}

impl ReadFlusherQueue {
    pub fn new() -> Self {
        Self::default()
    }

    #[allow(dead_code)]
    pub fn set_last_marked(&mut self, channel_id: impl Into<String>, ts: impl Into<String>) {
        let channel_id = channel_id.into();
        let ts = ts.into();
        if let Some(current) = self.last_marked.get(&channel_id) {
            if slack_timestamp_is_after(&ts, current) {
                self.last_marked.insert(channel_id, ts);
            }
        } else {
            self.last_marked.insert(channel_id, ts);
        }
    }

    #[allow(dead_code)]
    pub fn last_marked(&self, channel_id: &str) -> Option<&str> {
        self.last_marked.get(channel_id).map(String::as_str)
    }

    #[allow(dead_code)]
    pub fn pending_mark(&self, channel_id: &str) -> Option<&str> {
        self.pending
            .get(channel_id)
            .map(|mark| mark.target_ts.as_str())
    }

    #[cfg(test)]
    pub fn pending_mark_kind(&self, channel_id: &str) -> Option<ReadMarkKind> {
        self.pending.get(channel_id).map(|mark| mark.kind)
    }

    /// The cursor the user most recently asked for: a pending mark is newer
    /// than the in-flight one, which is newer than the last confirmed one.
    /// After a forced unread this can sit behind `last_marked`.
    fn latest_intended_ts(&self, channel_id: &str) -> Option<&str> {
        self.pending
            .get(channel_id)
            .or_else(|| self.in_flight.get(channel_id))
            .map(|mark| mark.target_ts.as_str())
            .or_else(|| self.last_marked(channel_id))
    }

    /// Queues a read mark if it moves the latest intended cursor forward.
    pub fn enqueue(&mut self, channel_id: &str, target_ts: &str) -> bool {
        if let Some(latest_ts) = self.latest_intended_ts(channel_id) {
            if !slack_timestamp_is_after(target_ts, latest_ts) {
                return false;
            }
        }
        self.insert_pending(channel_id, target_ts, ReadMarkKind::Read);
        true
    }

    /// Queues a user-forced unread mark; it replaces any pending mark and is
    /// never filtered against older, higher read cursors.
    pub fn enqueue_force(&mut self, channel_id: &str, target_ts: &str) {
        self.insert_pending(channel_id, target_ts, ReadMarkKind::Unread);
    }

    fn insert_pending(&mut self, channel_id: &str, target_ts: &str, kind: ReadMarkKind) {
        self.pending.insert(
            channel_id.to_string(),
            PendingReadMark {
                channel_id: channel_id.to_string(),
                target_ts: target_ts.to_string(),
                kind,
            },
        );
    }

    #[allow(dead_code)]
    pub fn set_last_marked_thread(
        &mut self,
        channel_id: impl Into<String>,
        thread_ts: impl Into<String>,
        ts: impl Into<String>,
    ) {
        let key = (channel_id.into(), thread_ts.into());
        let ts = ts.into();
        if let Some(current) = self.last_marked_threads.get(&key) {
            if slack_timestamp_is_after(&ts, current) {
                self.last_marked_threads.insert(key.clone(), ts);
                self.touch_marked_thread(&key);
            }
        } else {
            self.last_marked_threads.insert(key.clone(), ts);
            self.touch_marked_thread(&key);
            while self.last_marked_threads.len() > READ_FLUSHER_MAX_MARKED_THREADS {
                if let Some(evicted) = self.last_marked_thread_order.pop_front() {
                    self.last_marked_threads.remove(&evicted);
                } else {
                    break;
                }
            }
        }
    }

    fn touch_marked_thread(&mut self, key: &(String, String)) {
        self.last_marked_thread_order.retain(|queued| queued != key);
        self.last_marked_thread_order.push_back(key.clone());
    }

    #[allow(dead_code)]
    pub fn last_marked_thread(&self, channel_id: &str, thread_ts: &str) -> Option<&str> {
        let key = (channel_id.to_string(), thread_ts.to_string());
        self.last_marked_threads.get(&key).map(String::as_str)
    }

    #[cfg(test)]
    pub fn last_marked_threads_len(&self) -> usize {
        self.last_marked_threads.len()
    }

    #[allow(dead_code)]
    pub fn pending_thread_mark(&self, channel_id: &str, thread_ts: &str) -> Option<&str> {
        let key = (channel_id.to_string(), thread_ts.to_string());
        self.pending_threads.get(&key).map(String::as_str)
    }

    pub fn enqueue_thread(&mut self, channel_id: &str, thread_ts: &str, target_ts: &str) -> bool {
        let key = (channel_id.to_string(), thread_ts.to_string());
        if let Some(marked_ts) = self.last_marked_threads.get(&key) {
            if !slack_timestamp_is_after(target_ts, marked_ts) {
                return false;
            }
        }
        if let Some(p_ts) = self.pending_threads.get(&key) {
            if !slack_timestamp_is_after(target_ts, p_ts) {
                return false;
            }
        }
        if let Some(f_ts) = self.in_flight_threads.get(&key) {
            if !slack_timestamp_is_after(target_ts, f_ts) {
                return false;
            }
        }
        self.pending_threads.insert(key, target_ts.to_string());
        true
    }

    #[allow(dead_code)]
    pub fn enqueue_thread_force(&mut self, channel_id: &str, thread_ts: &str, target_ts: &str) {
        let key = (channel_id.to_string(), thread_ts.to_string());
        self.pending_threads.insert(key, target_ts.to_string());
    }

    /// Drains pending marks. `last_marked` only advances once the caller
    /// reports success through [`Self::mark_succeeded`]. Drained marks stay
    /// in flight until settled so the same target is not enqueued again.
    pub fn flush(&mut self) -> Vec<PendingReadMark> {
        let marks: Vec<PendingReadMark> = self.pending.drain().map(|(_, mark)| mark).collect();
        for mark in &marks {
            self.begin_in_flight(mark);
        }
        marks
    }

    pub fn flush_channel(&mut self, channel_id: &str) -> Option<PendingReadMark> {
        let mark = self.pending.remove(channel_id)?;
        self.begin_in_flight(&mark);
        Some(mark)
    }

    fn begin_in_flight(&mut self, mark: &PendingReadMark) {
        self.in_flight.insert(mark.channel_id.clone(), mark.clone());
    }

    /// Mirrors Slack's cursor after a confirmed mark, for both kinds.
    ///
    /// `conversations.mark` sets the cursor absolutely, so the last mark that
    /// succeeded is Slack's cursor even when it sits behind `last_marked`
    /// (e.g. a read that superseded an unsent or failed unread). Ordering is
    /// already enforced upstream: `enqueue` gates reads against the latest
    /// intent, and the flusher settles at most one mark per channel at a time,
    /// so a success is never older than a previously settled mark.
    pub fn mark_succeeded(&mut self, mark: &PendingReadMark) {
        self.in_flight.remove(&mark.channel_id);
        self.last_marked
            .insert(mark.channel_id.clone(), mark.target_ts.clone());
    }

    /// Puts a failed mark back unless the user queued a newer mark for the
    /// channel meanwhile. Anything pending was enqueued after this mark was
    /// drained, so it is the most recent intent and wins regardless of kind
    /// or timestamp order.
    pub fn mark_failed(&mut self, mark: &PendingReadMark) {
        self.in_flight.remove(&mark.channel_id);
        if !self.pending.contains_key(&mark.channel_id) {
            self.pending.insert(mark.channel_id.clone(), mark.clone());
        }
    }

    /// Gives up on a mark that can never succeed; it is not requeued.
    pub fn mark_dropped(&mut self, mark: &PendingReadMark) {
        self.in_flight.remove(&mark.channel_id);
    }

    pub fn flush_threads(&mut self) -> Vec<PendingThreadReadMark> {
        let marks: Vec<PendingThreadReadMark> = self
            .pending_threads
            .drain()
            .map(
                |((channel_id, thread_ts), target_ts)| PendingThreadReadMark {
                    channel_id,
                    thread_ts,
                    target_ts,
                },
            )
            .collect();
        for mark in &marks {
            self.in_flight_threads.insert(
                (mark.channel_id.clone(), mark.thread_ts.clone()),
                mark.target_ts.clone(),
            );
        }
        marks
    }

    pub fn thread_mark_succeeded(&mut self, mark: &PendingThreadReadMark) {
        self.in_flight_threads
            .remove(&(mark.channel_id.clone(), mark.thread_ts.clone()));
        self.set_last_marked_thread(
            mark.channel_id.as_str(),
            mark.thread_ts.as_str(),
            mark.target_ts.as_str(),
        );
    }

    pub fn thread_mark_failed(&mut self, mark: &PendingThreadReadMark) {
        self.in_flight_threads
            .remove(&(mark.channel_id.clone(), mark.thread_ts.clone()));
        self.enqueue_thread(&mark.channel_id, &mark.thread_ts, &mark.target_ts);
    }

    pub fn thread_mark_dropped(&mut self, mark: &PendingThreadReadMark) {
        self.in_flight_threads
            .remove(&(mark.channel_id.clone(), mark.thread_ts.clone()));
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

    #[allow(dead_code)]
    pub fn set_last_marked(&self, channel_id: &str, ts: &str) {
        let mut q = self.queue.lock().expect("read flusher queue lock poisoned");
        q.set_last_marked(channel_id, ts);
    }

    pub fn enqueue(&self, channel_id: &str, target_ts: &str) -> bool {
        let updated = {
            let mut q = self.queue.lock().expect("read flusher queue lock poisoned");
            q.enqueue(channel_id, target_ts)
        };
        if updated {
            self.notify.notify_one();
        }
        updated
    }

    #[allow(dead_code)]
    pub fn enqueue_force(&self, channel_id: &str, target_ts: &str) {
        {
            let mut q = self.queue.lock().expect("read flusher queue lock poisoned");
            q.enqueue_force(channel_id, target_ts);
        }
        self.notify.notify_one();
    }

    #[allow(dead_code)]
    pub fn set_last_marked_thread(&self, channel_id: &str, thread_ts: &str, ts: &str) {
        let mut q = self.queue.lock().expect("read flusher queue lock poisoned");
        q.set_last_marked_thread(channel_id, thread_ts, ts);
    }

    pub fn enqueue_thread(&self, channel_id: &str, thread_ts: &str, target_ts: &str) -> bool {
        let updated = {
            let mut q = self.queue.lock().expect("read flusher queue lock poisoned");
            q.enqueue_thread(channel_id, thread_ts, target_ts)
        };
        if updated {
            self.notify.notify_one();
        }
        updated
    }

    #[allow(dead_code)]
    pub fn enqueue_thread_force(&self, channel_id: &str, thread_ts: &str, target_ts: &str) {
        {
            let mut q = self.queue.lock().expect("read flusher queue lock poisoned");
            q.enqueue_thread_force(channel_id, thread_ts, target_ts);
        }
        self.notify.notify_one();
    }

    #[allow(dead_code)]
    pub fn flush_channel(&self, channel_id: &str) -> Option<PendingReadMark> {
        let mut q = self.queue.lock().expect("read flusher queue lock poisoned");
        q.flush_channel(channel_id)
    }

    pub fn flush(&self) -> Vec<PendingReadMark> {
        let mut q = self.queue.lock().expect("read flusher queue lock poisoned");
        q.flush()
    }

    pub fn flush_threads(&self) -> Vec<PendingThreadReadMark> {
        let mut q = self.queue.lock().expect("read flusher queue lock poisoned");
        q.flush_threads()
    }

    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, ReadFlusherQueue> {
        self.queue.lock().expect("read flusher queue lock poisoned")
    }
}

pub async fn execute_read_flusher_mark(api: &SlackApi, mark: &PendingReadMark) -> Result<()> {
    let mut retries = 0usize;
    loop {
        match api
            .conversations_mark(&mark.channel_id, &mark.target_ts)
            .await
        {
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

pub async fn execute_read_flusher_thread_mark(
    api: &SlackApi,
    mark: &PendingThreadReadMark,
) -> Result<()> {
    let mut retries = 0usize;
    loop {
        match api
            .subscriptions_thread_mark(&mark.channel_id, &mark.thread_ts, &mark.target_ts)
            .await
        {
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

/// Sends every pending mark once. Failed marks go back into the queue and
/// `last_marked` only advances on success. Returns whether any mark failed.
pub(super) async fn flush_read_marks(handle: &ReadFlusherHandle, api: &SlackApi) -> bool {
    let mut failed = false;
    for mark in handle.flush() {
        match execute_read_flusher_mark(api, &mark).await {
            Ok(()) => handle.lock().mark_succeeded(&mark),
            Err(error) if read_mark_error_is_permanent(&error) => {
                handle.lock().mark_dropped(&mark);
                crate::debug::log(
                    "read_flusher",
                    &format!(
                        "conversations_mark dropped channel={} error={error}",
                        mark.channel_id
                    ),
                );
            }
            Err(error) => {
                failed = true;
                handle.lock().mark_failed(&mark);
                crate::debug::log(
                    "read_flusher",
                    &format!(
                        "conversations_mark failed channel={} error={error}",
                        mark.channel_id
                    ),
                );
            }
        }
    }
    for thread_mark in handle.flush_threads() {
        match execute_read_flusher_thread_mark(api, &thread_mark).await {
            Ok(()) => handle.lock().thread_mark_succeeded(&thread_mark),
            Err(error) if read_mark_error_is_permanent(&error) => {
                handle.lock().thread_mark_dropped(&thread_mark);
                crate::debug::log(
                    "read_flusher",
                    &format!(
                        "subscriptions_thread_mark dropped channel={} thread={} error={error}",
                        thread_mark.channel_id, thread_mark.thread_ts,
                    ),
                );
            }
            Err(error) => {
                failed = true;
                handle.lock().thread_mark_failed(&thread_mark);
                crate::debug::log(
                    "read_flusher",
                    &format!(
                        "subscriptions_thread_mark failed channel={} thread={} error={error}",
                        thread_mark.channel_id, thread_mark.thread_ts,
                    ),
                );
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
