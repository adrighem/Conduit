/* workspace_state.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Pure workspace navigation and message state.
//!
//! This module deliberately has no dependency on GTK, WebKit, or the runtime. Callers apply
//! the returned outcomes to their views and translate request decisions into runtime commands.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use crate::conversation_catalog::ConversationCatalog;
use crate::models::{
    SavedItem, SearchMatch, SearchMessageLocation, SlackFile, SlackMessage, SlackReaction,
};
use crate::thread_catalog::ThreadCatalog;

/// Authoritative connection lifecycle for one workspace session.
///
/// This is intentionally separate from navigation and contains no presentation strings. Runtime
/// events drive transitions; GTK renders the resulting state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WorkspaceLifecycle {
    #[default]
    Disconnected,
    Connecting,
    Syncing,
    Ready,
    Degraded,
    AuthenticationRequired,
    StartupFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceLifecycleEvent {
    ConnectRequested,
    Authenticated,
    SyncCompleted,
    RetryableFailure,
    RecoveryStarted,
    AuthenticationFailed,
    StartupFailed,
    SignedOut,
}

impl WorkspaceLifecycle {
    pub(crate) fn transition(self, event: WorkspaceLifecycleEvent) -> Self {
        use WorkspaceLifecycleEvent as Event;

        if event == Event::SignedOut {
            return Self::Disconnected;
        }
        if self == Self::StartupFailed {
            return self;
        }

        match (self, event) {
            (Self::Disconnected | Self::AuthenticationRequired, Event::ConnectRequested) => {
                Self::Connecting
            }
            (Self::Disconnected, Event::StartupFailed) => Self::StartupFailed,
            (Self::Connecting, Event::Authenticated) => Self::Syncing,
            (
                Self::Connecting | Self::Syncing | Self::Ready | Self::Degraded,
                Event::AuthenticationFailed,
            ) => Self::AuthenticationRequired,
            (Self::Connecting | Self::Syncing | Self::Ready, Event::RetryableFailure) => {
                Self::Degraded
            }
            (Self::Degraded, Event::RecoveryStarted) => Self::Syncing,
            (Self::Syncing | Self::Degraded, Event::SyncCompleted) => Self::Ready,
            _ => self,
        }
    }
}

/// Canonical workspace-domain state owned by the window controller.
///
/// Keeping the catalogs and navigation state behind one owner makes session reset explicit and
/// prevents the GTK layer from maintaining parallel conversation collections.
#[derive(Debug, Default)]
pub(crate) struct WorkspaceSessionState {
    lifecycle: Cell<WorkspaceLifecycle>,
    pub(crate) conversations: RefCell<ConversationCatalog>,
    pub(crate) view: RefCell<WorkspaceViewState>,
    pub(crate) threads: RefCell<ThreadCatalog>,
}

impl WorkspaceSessionState {
    pub(crate) fn lifecycle(&self) -> WorkspaceLifecycle {
        self.lifecycle.get()
    }

    pub(crate) fn transition_lifecycle(
        &self,
        event: WorkspaceLifecycleEvent,
    ) -> WorkspaceLifecycle {
        let lifecycle = self.lifecycle.get().transition(event);
        self.lifecycle.set(lifecycle);
        lifecycle
    }

    pub(crate) fn reset(&self) {
        *self.conversations.borrow_mut() = ConversationCatalog::default();
        self.view.borrow_mut().reset();
        *self.threads.borrow_mut() = ThreadCatalog::default();
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum MainMessageView {
    #[default]
    Placeholder,
    Conversation,
    Unreads,
    Threads,
    Search,
    Files,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceScrollBehavior {
    Preserve,
    PreservePrepend,
    StickToBottom,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConversationSelectionDecision {
    RenderCurrent,
    RenderCached,
    RenderCachedAndRefresh,
    RequestFresh,
    AwaitFresh,
}

impl ConversationSelectionDecision {
    pub(crate) fn requests_history(self) -> bool {
        matches!(self, Self::RenderCachedAndRefresh | Self::RequestFresh)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConversationSelectionOutcome {
    pub(crate) decision: ConversationSelectionDecision,
    pub(crate) scroll: Option<WorkspaceScrollBehavior>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HistoryApplyOutcome {
    pub(crate) visible: bool,
    pub(crate) render: bool,
    pub(crate) notify_new_messages: bool,
    pub(crate) scroll: Option<WorkspaceScrollBehavior>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceFailureOutcome {
    pub(crate) active: bool,
    pub(crate) has_content: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadOpenOutcome {
    Ignored,
    RenderCurrent,
    RequestFresh,
    AwaitFresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadApplyOutcome {
    Ignored,
    Applied {
        scroll: WorkspaceScrollBehavior,
        render: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RealtimeMessageKind {
    Posted,
    Changed,
    Deleted,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RealtimeMessageOutcome {
    pub(crate) channel_changed: bool,
    pub(crate) render_channel: bool,
    pub(crate) render_thread: bool,
    pub(crate) refresh_unreads: bool,
    pub(crate) channel_scroll: Option<WorkspaceScrollBehavior>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReactionUpdate {
    pub(crate) channel_id: String,
    pub(crate) ts: String,
    pub(crate) name: String,
    pub(crate) user_id: String,
    pub(crate) added: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ReactionUpdateOutcome {
    pub(crate) changed: bool,
    pub(crate) render_channel: bool,
    pub(crate) render_thread: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceSnapshot {
    pub(crate) channel_id: Option<String>,
    pub(crate) thread_ts: Option<String>,
    pub(crate) channel_messages: Vec<SlackMessage>,
    pub(crate) thread_messages: Vec<SlackMessage>,
    pub(crate) search_results: Vec<SearchMatch>,
    pub(crate) files: Vec<SlackFile>,
    pub(crate) saved_items: Vec<SavedItem>,
    pub(crate) main_view: MainMessageView,
}

#[derive(Debug, Clone, Default)]
struct ChannelHistoryState {
    messages: Vec<SlackMessage>,
    context_messages: Option<Vec<SlackMessage>>,
    next_cursor: Option<String>,
    loading: bool,
    loaded: bool,
    force_bottom: bool,
    focus_ts: Option<String>,
}

#[derive(Debug, Clone)]
struct ThreadViewState {
    channel_id: String,
    ts: String,
    messages: Vec<SlackMessage>,
    context_messages: Option<Vec<SlackMessage>>,
    next_cursor: Option<String>,
    status: ThreadLoadStatus,
    focus_ts: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadLoadStatus {
    Loading,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceViewState {
    main_view: MainMessageView,
    last_channel_id: Option<String>,
    channels: HashMap<String, ChannelHistoryState>,
    thread: Option<ThreadViewState>,
    search_results: Vec<SearchMatch>,
    files: Vec<SlackFile>,
    saved_items: Vec<SavedItem>,
    search_loading: bool,
    files_loading: bool,
    saved_loading: bool,
}

impl WorkspaceViewState {
    pub(crate) fn main_view(&self) -> MainMessageView {
        self.main_view
    }

    pub(crate) fn last_channel_id(&self) -> Option<&str> {
        self.last_channel_id.as_deref()
    }

    pub(crate) fn visible_channel_id(&self) -> Option<&str> {
        (self.main_view == MainMessageView::Conversation)
            .then_some(self.last_channel_id.as_deref())
            .flatten()
    }

    pub(crate) fn selected_thread_ts(&self) -> Option<&str> {
        self.thread.as_ref().map(|thread| thread.ts.as_str())
    }

    pub(crate) fn channel_messages(&self, channel_id: &str) -> &[SlackMessage] {
        self.channels
            .get(channel_id)
            .map(|history| {
                history
                    .context_messages
                    .as_deref()
                    .unwrap_or(&history.messages)
            })
            .unwrap_or_default()
    }

    pub(crate) fn current_thread_messages(&self) -> &[SlackMessage] {
        self.thread
            .as_ref()
            .map(|thread| {
                thread
                    .context_messages
                    .as_deref()
                    .unwrap_or(&thread.messages)
            })
            .unwrap_or_default()
    }

    pub(crate) fn has_channel_context(&self, channel_id: &str) -> bool {
        self.channels
            .get(channel_id)
            .is_some_and(|history| history.context_messages.is_some())
    }

    pub(crate) fn has_thread_context(&self, channel_id: &str, thread_ts: &str) -> bool {
        self.thread.as_ref().is_some_and(|thread| {
            thread.channel_id == channel_id
                && thread.ts == thread_ts
                && thread.context_messages.is_some()
        })
    }

    pub(crate) fn search_results(&self) -> &[SearchMatch] {
        &self.search_results
    }

    pub(crate) fn files(&self) -> &[SlackFile] {
        &self.files
    }

    pub(crate) fn saved_items(&self) -> &[SavedItem] {
        &self.saved_items
    }

    #[cfg(test)]
    pub(crate) fn search_loading(&self) -> bool {
        self.search_loading
    }

    #[cfg(test)]
    pub(crate) fn files_loading(&self) -> bool {
        self.files_loading
    }

    #[cfg(test)]
    pub(crate) fn saved_loading(&self) -> bool {
        self.saved_loading
    }

    pub(crate) fn snapshot(&self) -> WorkspaceSnapshot {
        let channel_id = self.last_channel_id.clone();
        let channel_messages = channel_id
            .as_deref()
            .map(|channel_id| self.channel_messages(channel_id).to_vec())
            .unwrap_or_default();
        let (thread_ts, thread_messages) = self
            .thread
            .as_ref()
            .map(|thread| (Some(thread.ts.clone()), thread.messages.clone()))
            .unwrap_or_default();
        WorkspaceSnapshot {
            channel_id,
            thread_ts,
            channel_messages,
            thread_messages,
            search_results: self.search_results.clone(),
            files: self.files.clone(),
            saved_items: self.saved_items.clone(),
            main_view: self.main_view,
        }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn show_placeholder(&mut self) {
        self.navigate_to(MainMessageView::Placeholder);
    }

    pub(crate) fn remove_conversation(&mut self, channel_id: &str) {
        self.channels.remove(channel_id);
        if self.last_channel_id.as_deref() == Some(channel_id) {
            self.last_channel_id = None;
            if self.main_view == MainMessageView::Conversation {
                self.main_view = MainMessageView::Placeholder;
            }
        }
        if self
            .thread
            .as_ref()
            .is_some_and(|thread| thread.channel_id == channel_id)
        {
            self.thread = None;
        }
    }

    pub(crate) fn show_unreads(&mut self) {
        self.navigate_to(MainMessageView::Unreads);
    }

    pub(crate) fn show_threads(&mut self) {
        self.navigate_to(MainMessageView::Threads);
    }

    pub(crate) fn observed_threads(&self) -> Vec<(String, SlackMessage)> {
        let mut threads = self
            .channels
            .iter()
            .flat_map(|(channel_id, history)| {
                history.messages.iter().filter_map(move |message| {
                    (message.thread_ts.is_none() && message.has_thread())
                        .then_some((channel_id.clone(), message.clone()))
                })
            })
            .collect::<Vec<_>>();
        threads.sort_by(|left, right| right.1.ts.cmp(&left.1.ts));
        threads
    }

    pub(crate) fn show_search(&mut self) {
        self.navigate_to(MainMessageView::Search);
    }

    pub(crate) fn start_search(&mut self) {
        self.show_search();
        self.search_results.clear();
        self.search_loading = true;
    }

    pub(crate) fn apply_search_results(&mut self, results: Vec<SearchMatch>) -> bool {
        self.search_results = results;
        self.search_loading = false;
        self.main_view == MainMessageView::Search
    }

    pub(crate) fn show_files(&mut self) {
        self.navigate_to(MainMessageView::Files);
    }

    pub(crate) fn start_files(&mut self) {
        self.show_files();
        self.files.clear();
        self.files_loading = true;
    }

    pub(crate) fn apply_files(&mut self, files: Vec<SlackFile>) -> bool {
        self.files = files;
        self.files_loading = false;
        self.main_view == MainMessageView::Files
    }

    pub(crate) fn show_saved(&mut self) {
        self.navigate_to(MainMessageView::Saved);
    }

    pub(crate) fn start_saved(&mut self) {
        self.show_saved();
        self.saved_items.clear();
        self.saved_loading = true;
    }

    pub(crate) fn apply_saved(&mut self, items: Vec<SavedItem>) -> bool {
        self.saved_items = items;
        self.saved_loading = false;
        self.main_view == MainMessageView::Saved
    }

    pub(crate) fn select_conversation(&mut self, channel_id: &str) -> ConversationSelectionOutcome {
        let was_visible = self.visible_channel_id() == Some(channel_id);
        let changing_channel = self.last_channel_id.as_deref() != Some(channel_id);
        if let Some(previous_channel_id) = self.last_channel_id.as_deref() {
            if let Some(history) = self.channels.get_mut(previous_channel_id) {
                history.focus_ts = None;
                history.context_messages = None;
            }
        }
        self.thread = None;

        if !was_visible {
            self.clear_current_view_loading();
        }

        if changing_channel {
            self.channels
                .entry(channel_id.to_string())
                .or_default()
                .force_bottom = true;
        }
        self.last_channel_id = Some(channel_id.to_string());
        self.main_view = MainMessageView::Conversation;

        let history = self.channels.entry(channel_id.to_string()).or_default();
        history.focus_ts = None;
        history.context_messages = None;
        let decision = if was_visible && history.loaded {
            ConversationSelectionDecision::RenderCurrent
        } else if history.loaded && history.loading {
            ConversationSelectionDecision::RenderCached
        } else if history.loaded {
            history.loading = true;
            ConversationSelectionDecision::RenderCachedAndRefresh
        } else if history.loading {
            ConversationSelectionDecision::AwaitFresh
        } else {
            history.loading = true;
            ConversationSelectionDecision::RequestFresh
        };
        let scroll = matches!(
            decision,
            ConversationSelectionDecision::RenderCurrent
                | ConversationSelectionDecision::RenderCached
                | ConversationSelectionDecision::RenderCachedAndRefresh
        )
        .then(|| self.take_channel_scroll(channel_id, false));

        ConversationSelectionOutcome { decision, scroll }
    }

    pub(crate) fn begin_history_request(&mut self, channel_id: &str) -> bool {
        let history = self.channels.entry(channel_id.to_string()).or_default();
        if history.loading {
            false
        } else {
            history.loading = true;
            true
        }
    }

    pub(crate) fn fail_history(&mut self, channel_id: &str) -> WorkspaceFailureOutcome {
        let active = self.visible_channel_id() == Some(channel_id);
        let Some(history) = self.channels.get_mut(channel_id) else {
            return WorkspaceFailureOutcome::default();
        };
        history.loading = false;
        if history.messages.is_empty() {
            history.loaded = false;
        }
        WorkspaceFailureOutcome {
            active,
            has_content: !history.messages.is_empty(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_history(
        &mut self,
        channel_id: &str,
        messages: Vec<SlackMessage>,
        has_more: bool,
        next_cursor: Option<String>,
        append_older: bool,
        cached: bool,
    ) -> HistoryApplyOutcome {
        let visible = self.visible_channel_id() == Some(channel_id);
        let history = self.channels.entry(channel_id.to_string()).or_default();
        let was_loaded = history.loaded;
        let previous_messages = history.messages.clone();
        let had_context = history.context_messages.is_some();
        history.messages = if append_older {
            merge_channel_message_pages(&history.messages, &messages)
        } else {
            merge_channel_message_refresh(&history.messages, &messages)
        };
        history.loaded = true;
        if !cached {
            history.next_cursor = usable_cursor(has_more, next_cursor);
            history.loading = false;
            history.context_messages = None;
        }

        let render = visible
            && (!was_loaded
                || history.messages != previous_messages
                || (had_context && !cached && !append_older));
        let notify_new_messages = visible && !cached && !append_older;
        let scroll = visible.then(|| self.take_channel_scroll(channel_id, append_older));
        HistoryApplyOutcome {
            visible,
            render,
            notify_new_messages,
            scroll,
        }
    }

    pub(crate) fn channel_cursor(&self, channel_id: &str) -> Option<&str> {
        self.channels
            .get(channel_id)
            .and_then(|history| history.next_cursor.as_deref())
    }

    pub(crate) fn open_thread(&mut self, channel_id: &str, ts: &str) -> ThreadOpenOutcome {
        if self.visible_channel_id() != Some(channel_id) || ts.trim().is_empty() {
            return ThreadOpenOutcome::Ignored;
        }

        if let Some(thread) = &mut self.thread {
            if thread.channel_id == channel_id && thread.ts == ts {
                thread.focus_ts = None;
                thread.context_messages = None;
                return match thread.status {
                    ThreadLoadStatus::Ready => ThreadOpenOutcome::RenderCurrent,
                    ThreadLoadStatus::Loading => ThreadOpenOutcome::AwaitFresh,
                    ThreadLoadStatus::Failed => {
                        thread.status = ThreadLoadStatus::Loading;
                        ThreadOpenOutcome::RequestFresh
                    }
                };
            }
        }

        self.thread = Some(ThreadViewState {
            channel_id: channel_id.to_string(),
            ts: ts.to_string(),
            messages: Vec::new(),
            context_messages: None,
            next_cursor: None,
            status: ThreadLoadStatus::Loading,
            focus_ts: None,
        });
        ThreadOpenOutcome::RequestFresh
    }

    pub(crate) fn begin_thread_history_request(&mut self) -> bool {
        let Some(thread) = &mut self.thread else {
            return false;
        };
        if thread.status == ThreadLoadStatus::Loading {
            false
        } else {
            thread.status = ThreadLoadStatus::Loading;
            true
        }
    }

    pub(crate) fn fail_thread(&mut self, channel_id: &str, ts: &str) -> WorkspaceFailureOutcome {
        let Some(thread) = &mut self.thread else {
            return WorkspaceFailureOutcome::default();
        };
        if thread.channel_id != channel_id || thread.ts != ts {
            return WorkspaceFailureOutcome::default();
        }
        thread.status = if thread.messages.is_empty() {
            ThreadLoadStatus::Failed
        } else {
            ThreadLoadStatus::Ready
        };
        WorkspaceFailureOutcome {
            active: true,
            has_content: !thread.messages.is_empty(),
        }
    }

    pub(crate) fn close_thread(&mut self) -> bool {
        self.thread.take().is_some()
    }

    pub(crate) fn focus_message(&mut self, location: &SearchMessageLocation) -> bool {
        if self.visible_channel_id() != Some(location.channel_id()) {
            return false;
        }

        if let Some(thread_ts) = location.thread_ts() {
            let Some(thread) = &mut self.thread else {
                return false;
            };
            if thread.channel_id != location.channel_id() || thread.ts != thread_ts {
                return false;
            }
            thread.focus_ts = Some(location.message_ts().to_string());
        } else {
            let Some(history) = self.channels.get_mut(location.channel_id()) else {
                return false;
            };
            history.focus_ts = Some(location.message_ts().to_string());
        }
        true
    }

    pub(crate) fn apply_message_context(
        &mut self,
        location: &SearchMessageLocation,
        messages: Vec<SlackMessage>,
    ) -> bool {
        if !messages
            .iter()
            .any(|message| message.ts == location.message_ts())
        {
            return false;
        }

        if let Some(thread_ts) = location.thread_ts() {
            let Some(thread) = &mut self.thread else {
                return false;
            };
            if thread.channel_id != location.channel_id()
                || thread.ts != thread_ts
                || thread.focus_ts.as_deref() != Some(location.message_ts())
            {
                return false;
            }
            thread.context_messages = Some(normalize_messages(messages));
            thread.status = ThreadLoadStatus::Ready;
            return true;
        }

        if self.visible_channel_id() != Some(location.channel_id()) {
            return false;
        }
        let Some(history) = self.channels.get_mut(location.channel_id()) else {
            return false;
        };
        if history.focus_ts.as_deref() != Some(location.message_ts()) {
            return false;
        }
        history.context_messages = Some(normalize_messages(messages));
        history.loading = false;
        true
    }

    pub(crate) fn take_channel_focus_for_render(
        &mut self,
        channel_id: &str,
        messages: &[SlackMessage],
    ) -> Option<String> {
        if self.visible_channel_id() != Some(channel_id) {
            return None;
        }
        let history = self.channels.get_mut(channel_id)?;
        let focus_ts = history.focus_ts.as_deref()?;
        messages
            .iter()
            .any(|message| message.ts == focus_ts)
            .then(|| history.focus_ts.take())
            .flatten()
    }

    pub(crate) fn take_thread_focus_for_render(
        &mut self,
        channel_id: &str,
        thread_ts: &str,
        messages: &[SlackMessage],
    ) -> Option<String> {
        let thread = self.thread.as_mut()?;
        if thread.channel_id != channel_id || thread.ts != thread_ts {
            return None;
        }
        let focus_ts = thread.focus_ts.as_deref()?;
        messages
            .iter()
            .any(|message| message.ts == focus_ts)
            .then(|| thread.focus_ts.take())
            .flatten()
    }

    #[cfg(test)]
    fn channel_focus_ts(&self, channel_id: &str) -> Option<&str> {
        self.channels
            .get(channel_id)
            .and_then(|history| history.focus_ts.as_deref())
    }

    #[cfg(test)]
    fn thread_focus_ts(&self) -> Option<&str> {
        self.thread
            .as_ref()
            .and_then(|thread| thread.focus_ts.as_deref())
    }

    pub(crate) fn fail_search(&mut self) -> WorkspaceFailureOutcome {
        self.search_loading = false;
        WorkspaceFailureOutcome {
            active: self.main_view == MainMessageView::Search,
            has_content: !self.search_results.is_empty(),
        }
    }

    pub(crate) fn fail_files(&mut self) -> WorkspaceFailureOutcome {
        self.files_loading = false;
        WorkspaceFailureOutcome {
            active: self.main_view == MainMessageView::Files,
            has_content: !self.files.is_empty(),
        }
    }

    pub(crate) fn fail_saved(&mut self) -> WorkspaceFailureOutcome {
        self.saved_loading = false;
        WorkspaceFailureOutcome {
            active: self.main_view == MainMessageView::Saved,
            has_content: !self.saved_items.is_empty(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_thread(
        &mut self,
        channel_id: &str,
        ts: &str,
        messages: Vec<SlackMessage>,
        has_more: bool,
        next_cursor: Option<String>,
        append_older: bool,
    ) -> ThreadApplyOutcome {
        let Some(thread) = &mut self.thread else {
            return ThreadApplyOutcome::Ignored;
        };
        if thread.channel_id != channel_id || thread.ts != ts {
            return ThreadApplyOutcome::Ignored;
        }

        let was_ready = thread.status == ThreadLoadStatus::Ready;
        let previous_messages = thread.messages.clone();
        let had_context = thread.context_messages.is_some();
        thread.messages = if append_older {
            merge_message_pages(&thread.messages, &messages)
        } else {
            merge_message_refresh(&thread.messages, &messages)
        };
        thread.status = ThreadLoadStatus::Ready;
        thread.context_messages = None;
        thread.next_cursor = usable_cursor(has_more, next_cursor);
        ThreadApplyOutcome::Applied {
            scroll: if append_older {
                WorkspaceScrollBehavior::PreservePrepend
            } else {
                WorkspaceScrollBehavior::StickToBottom
            },
            render: !was_ready || had_context || thread.messages != previous_messages,
        }
    }

    pub(crate) fn thread_cursor(&self) -> Option<&str> {
        self.thread
            .as_ref()
            .and_then(|thread| thread.next_cursor.as_deref())
    }

    pub(crate) fn apply_realtime_message(
        &mut self,
        channel_id: &str,
        message: SlackMessage,
        kind: RealtimeMessageKind,
    ) -> RealtimeMessageOutcome {
        let visible = self.visible_channel_id() == Some(channel_id);
        let history = self.channels.entry(channel_id.to_string()).or_default();
        let channel_changed = {
            let existing = std::mem::take(&mut history.messages);
            let normalized = normalize_channel_messages(existing.clone());
            let removed_misrouted_replies = normalized != existing;
            history.messages = normalized;
            let already_in_channel = history.messages.iter().any(|item| item.ts == message.ts);
            let affects_channel = message.belongs_in_channel_timeline()
                || (kind != RealtimeMessageKind::Posted && already_in_channel);
            let base_changed = if affects_channel && history.loaded {
                apply_realtime_message_to(&mut history.messages, &message, kind)
                    || removed_misrouted_replies
            } else if affects_channel && kind == RealtimeMessageKind::Posted {
                let changed = apply_realtime_message_to(&mut history.messages, &message, kind);
                history.loaded = true;
                history.loading = false;
                changed || removed_misrouted_replies
            } else {
                removed_misrouted_replies
            };
            let context_changed = history
                .context_messages
                .as_mut()
                .filter(|messages| messages.iter().any(|item| item.ts == message.ts))
                .is_some_and(|messages| apply_realtime_message_to(messages, &message, kind));
            base_changed || context_changed
        };
        let render_channel = visible && channel_changed;

        let render_thread = self
            .thread
            .as_mut()
            .filter(|thread| {
                thread.channel_id == channel_id && message.belongs_to_thread(&thread.ts)
            })
            .is_some_and(|thread| {
                let base_changed = if thread.status == ThreadLoadStatus::Ready
                    || kind == RealtimeMessageKind::Posted
                {
                    apply_realtime_message_to(&mut thread.messages, &message, kind)
                } else {
                    false
                };
                let context_changed = thread
                    .context_messages
                    .as_mut()
                    .filter(|messages| messages.iter().any(|item| item.ts == message.ts))
                    .is_some_and(|messages| apply_realtime_message_to(messages, &message, kind));
                base_changed || context_changed
            });

        RealtimeMessageOutcome {
            channel_changed,
            render_channel,
            render_thread,
            refresh_unreads: self.main_view == MainMessageView::Unreads,
            channel_scroll: render_channel.then_some(
                if kind == RealtimeMessageKind::Posted && message.belongs_in_channel_timeline() {
                    WorkspaceScrollBehavior::StickToBottom
                } else {
                    WorkspaceScrollBehavior::Preserve
                },
            ),
        }
    }

    pub(crate) fn apply_reaction(&mut self, update: &ReactionUpdate) -> ReactionUpdateOutcome {
        let channel_changed = self
            .channels
            .get_mut(&update.channel_id)
            .is_some_and(|history| {
                let messages_changed = apply_reaction_to_messages(&mut history.messages, update);
                let context_changed = history
                    .context_messages
                    .as_mut()
                    .is_some_and(|messages| apply_reaction_to_messages(messages, update));
                messages_changed || context_changed
            });
        let thread_changed = self
            .thread
            .as_mut()
            .filter(|thread| thread.channel_id == update.channel_id)
            .is_some_and(|thread| {
                let messages_changed = apply_reaction_to_messages(&mut thread.messages, update);
                let context_changed = thread
                    .context_messages
                    .as_mut()
                    .is_some_and(|messages| apply_reaction_to_messages(messages, update));
                messages_changed || context_changed
            });
        let visible = self.visible_channel_id() == Some(update.channel_id.as_str());

        ReactionUpdateOutcome {
            changed: channel_changed || thread_changed,
            render_channel: visible && channel_changed,
            render_thread: thread_changed,
        }
    }

    pub(crate) fn find_message(&self, channel_id: &str, ts: &str) -> Option<SlackMessage> {
        self.channels
            .get(channel_id)
            .and_then(|history| {
                history
                    .context_messages
                    .as_deref()
                    .unwrap_or(&history.messages)
                    .iter()
                    .find(|message| message.ts == ts)
            })
            .or_else(|| {
                self.thread
                    .as_ref()
                    .filter(|thread| thread.channel_id == channel_id)
                    .and_then(|thread| {
                        thread
                            .context_messages
                            .as_deref()
                            .unwrap_or(&thread.messages)
                            .iter()
                            .find(|message| message.ts == ts)
                    })
            })
            .or_else(|| {
                self.saved_items
                    .iter()
                    .filter(|item| item.channel.as_deref() == Some(channel_id))
                    .filter_map(|item| item.message.as_ref())
                    .find(|message| message.ts == ts)
            })
            .cloned()
    }

    fn navigate_to(&mut self, view: MainMessageView) {
        self.clear_current_view_loading();
        if let Some(channel_id) = self.visible_channel_id().map(ToString::to_string) {
            if let Some(history) = self.channels.get_mut(&channel_id) {
                history.focus_ts = None;
                history.context_messages = None;
            }
        }
        self.main_view = view;
        self.thread = None;
    }

    fn clear_current_view_loading(&mut self) {
        match self.main_view {
            MainMessageView::Conversation => {
                if let Some(channel_id) = self.last_channel_id.as_deref() {
                    if let Some(history) = self.channels.get_mut(channel_id) {
                        history.loading = false;
                    }
                }
            }
            MainMessageView::Search => self.search_loading = false,
            MainMessageView::Files => self.files_loading = false,
            MainMessageView::Saved => self.saved_loading = false,
            MainMessageView::Placeholder | MainMessageView::Unreads | MainMessageView::Threads => {}
        }
    }

    fn take_channel_scroll(
        &mut self,
        channel_id: &str,
        append_older: bool,
    ) -> WorkspaceScrollBehavior {
        let force_bottom = self
            .channels
            .get_mut(channel_id)
            .is_some_and(|history| std::mem::take(&mut history.force_bottom));
        if append_older {
            WorkspaceScrollBehavior::PreservePrepend
        } else if force_bottom {
            WorkspaceScrollBehavior::Bottom
        } else {
            WorkspaceScrollBehavior::StickToBottom
        }
    }
}

fn usable_cursor(has_more: bool, cursor: Option<String>) -> Option<String> {
    cursor.filter(|cursor| has_more && !cursor.trim().is_empty())
}

fn normalize_messages(mut messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    messages.sort_by(|left, right| right.ts.cmp(&left.ts));
    messages.dedup_by(|left, right| !left.ts.is_empty() && left.ts == right.ts);
    messages
}

fn normalize_channel_messages(messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    normalize_messages(
        messages
            .into_iter()
            .filter(SlackMessage::belongs_in_channel_timeline)
            .collect(),
    )
}

fn merge_message_pages(existing: &[SlackMessage], page: &[SlackMessage]) -> Vec<SlackMessage> {
    let mut messages = existing.to_vec();
    messages.extend(page.iter().cloned());
    normalize_messages(messages)
}

fn merge_message_refresh(
    existing: &[SlackMessage],
    snapshot: &[SlackMessage],
) -> Vec<SlackMessage> {
    // A send response or realtime event can arrive while the request that
    // produced this snapshot is still in flight. Snapshot entries are
    // authoritative for duplicates, while newer locally observed entries must
    // not disappear until a later response includes them.
    let mut messages = snapshot.to_vec();
    messages.extend(existing.iter().cloned());
    normalize_messages(messages)
}

fn merge_channel_message_pages(
    existing: &[SlackMessage],
    page: &[SlackMessage],
) -> Vec<SlackMessage> {
    normalize_channel_messages(existing.iter().chain(page).cloned().collect::<Vec<_>>())
}

fn merge_channel_message_refresh(
    existing: &[SlackMessage],
    snapshot: &[SlackMessage],
) -> Vec<SlackMessage> {
    normalize_channel_messages(snapshot.iter().chain(existing).cloned().collect::<Vec<_>>())
}

fn apply_realtime_message_to(
    existing: &mut Vec<SlackMessage>,
    message: &SlackMessage,
    kind: RealtimeMessageKind,
) -> bool {
    let current = existing.iter().find(|item| item.ts == message.ts);
    if current == Some(message) {
        return false;
    }
    if current.is_none() && kind != RealtimeMessageKind::Posted {
        return false;
    }

    let mut messages = existing
        .iter()
        .filter(|existing_message| existing_message.ts != message.ts)
        .cloned()
        .collect::<Vec<_>>();
    messages.push(message.clone());
    *existing = normalize_messages(messages);
    true
}

fn apply_reaction_to_messages(messages: &mut [SlackMessage], update: &ReactionUpdate) -> bool {
    messages
        .iter_mut()
        .find(|message| message.ts == update.ts)
        .is_some_and(|message| apply_reaction_to_message(message, update))
}

fn apply_reaction_to_message(message: &mut SlackMessage, update: &ReactionUpdate) -> bool {
    if update.added {
        let reactions = message.reactions.get_or_insert_with(Vec::new);
        if let Some(reaction) = reactions
            .iter_mut()
            .find(|reaction| reaction.name.as_deref() == Some(update.name.as_str()))
        {
            let users = reaction.users.get_or_insert_with(Vec::new);
            if users.iter().any(|user| user == &update.user_id) {
                return false;
            }
            users.push(update.user_id.clone());
            reaction.count = Some(reaction.count.unwrap_or_default().saturating_add(1));
        } else {
            reactions.push(SlackReaction {
                name: Some(update.name.clone()),
                count: Some(1),
                users: Some(vec![update.user_id.clone()]),
            });
        }
        true
    } else {
        let Some(reactions) = message.reactions.as_mut() else {
            return false;
        };
        let Some(index) = reactions
            .iter()
            .position(|reaction| reaction.name.as_deref() == Some(update.name.as_str()))
        else {
            return false;
        };
        let reaction = &mut reactions[index];
        if let Some(users) = reaction.users.as_mut() {
            let original_len = users.len();
            users.retain(|user| user != &update.user_id);
            if users.len() == original_len {
                return false;
            }
        }
        let count = reaction.count.unwrap_or_default().saturating_sub(1);
        reaction.count = Some(count);
        if count == 0 {
            reactions.remove(index);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(ts: &str, text: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.to_string(),
            text: Some(text.to_string()),
            ..SlackMessage::default()
        }
    }

    fn thread_message(ts: &str, thread_ts: &str, text: &str) -> SlackMessage {
        SlackMessage {
            thread_ts: Some(thread_ts.to_string()),
            ..message(ts, text)
        }
    }

    fn apply_fresh(
        state: &mut WorkspaceViewState,
        channel_id: &str,
        messages: Vec<SlackMessage>,
    ) -> HistoryApplyOutcome {
        state.apply_history(channel_id, messages, false, None, false, false)
    }

    #[test]
    fn workspace_lifecycle_connects_syncs_and_becomes_ready() {
        let connecting =
            WorkspaceLifecycle::default().transition(WorkspaceLifecycleEvent::ConnectRequested);
        assert_eq!(connecting, WorkspaceLifecycle::Connecting);

        let syncing = connecting.transition(WorkspaceLifecycleEvent::Authenticated);
        assert_eq!(syncing, WorkspaceLifecycle::Syncing);

        assert_eq!(
            syncing.transition(WorkspaceLifecycleEvent::SyncCompleted),
            WorkspaceLifecycle::Ready
        );
    }

    #[test]
    fn workspace_session_owns_and_applies_lifecycle_transitions() {
        let session = WorkspaceSessionState::default();
        assert_eq!(session.lifecycle(), WorkspaceLifecycle::Disconnected);

        assert_eq!(
            session.transition_lifecycle(WorkspaceLifecycleEvent::ConnectRequested),
            WorkspaceLifecycle::Connecting
        );
        assert_eq!(session.lifecycle(), WorkspaceLifecycle::Connecting);
    }

    #[test]
    fn workspace_lifecycle_recovers_from_degraded_through_sync() {
        let degraded =
            WorkspaceLifecycle::Ready.transition(WorkspaceLifecycleEvent::RetryableFailure);
        assert_eq!(degraded, WorkspaceLifecycle::Degraded);

        let syncing = degraded.transition(WorkspaceLifecycleEvent::RecoveryStarted);
        assert_eq!(syncing, WorkspaceLifecycle::Syncing);
        assert_eq!(
            syncing.transition(WorkspaceLifecycleEvent::SyncCompleted),
            WorkspaceLifecycle::Ready
        );
    }

    #[test]
    fn workspace_lifecycle_handles_authentication_failure_and_reconnect() {
        let authentication_required = WorkspaceLifecycle::Connecting
            .transition(WorkspaceLifecycleEvent::AuthenticationFailed);
        assert_eq!(
            authentication_required,
            WorkspaceLifecycle::AuthenticationRequired
        );
        assert_eq!(
            authentication_required.transition(WorkspaceLifecycleEvent::ConnectRequested),
            WorkspaceLifecycle::Connecting
        );
    }

    #[test]
    fn workspace_lifecycle_sign_out_resets_every_nonterminal_state() {
        for lifecycle in [
            WorkspaceLifecycle::Connecting,
            WorkspaceLifecycle::Syncing,
            WorkspaceLifecycle::Ready,
            WorkspaceLifecycle::Degraded,
            WorkspaceLifecycle::AuthenticationRequired,
        ] {
            assert_eq!(
                lifecycle.transition(WorkspaceLifecycleEvent::SignedOut),
                WorkspaceLifecycle::Disconnected
            );
        }
    }

    #[test]
    fn workspace_lifecycle_startup_failure_is_terminal_until_reset() {
        let failed =
            WorkspaceLifecycle::Disconnected.transition(WorkspaceLifecycleEvent::StartupFailed);
        assert_eq!(failed, WorkspaceLifecycle::StartupFailed);
        assert_eq!(
            failed.transition(WorkspaceLifecycleEvent::ConnectRequested),
            WorkspaceLifecycle::StartupFailed
        );
        assert_eq!(
            failed.transition(WorkspaceLifecycleEvent::SignedOut),
            WorkspaceLifecycle::Disconnected
        );
    }

    #[test]
    fn observed_threads_collect_roots_across_loaded_channels_newest_first() {
        let mut state = WorkspaceViewState::default();
        let mut older = message("1", "older thread");
        older.reply_count = Some(2);
        let mut newer = message("3", "newer thread");
        newer.reply_count = Some(1);
        apply_fresh(&mut state, "C1", vec![older, message("2", "plain")]);
        apply_fresh(&mut state, "C2", vec![newer]);

        let threads = state.observed_threads();

        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].0, "C2");
        assert_eq!(threads[0].1.ts, "3");
        assert_eq!(threads[1].0, "C1");
    }

    #[test]
    fn reset_clears_navigation_payloads_cursors_and_loading() {
        let mut state = WorkspaceViewState::default();
        assert_eq!(
            state.select_conversation("C1").decision,
            ConversationSelectionDecision::RequestFresh
        );
        state.apply_history(
            "C1",
            vec![message("2", "new")],
            true,
            Some("next".into()),
            false,
            false,
        );
        assert_eq!(
            state.open_thread("C1", "2"),
            ThreadOpenOutcome::RequestFresh
        );
        state.apply_thread(
            "C1",
            "2",
            vec![message("2", "parent")],
            true,
            Some("thread-next".into()),
            false,
        );
        state.start_search();
        state.apply_search_results(vec![SearchMatch {
            text: Some("match".into()),
            ..SearchMatch::default()
        }]);
        state.start_files();
        state.apply_files(vec![SlackFile {
            id: Some("F1".into()),
            ..SlackFile::default()
        }]);
        state.start_saved();
        state.apply_saved(vec![SavedItem {
            channel: Some("C1".into()),
            message: Some(message("2", "saved")),
            ..SavedItem::default()
        }]);
        state.start_search();

        state.reset();

        assert_eq!(state.main_view(), MainMessageView::Placeholder);
        assert_eq!(state.last_channel_id(), None);
        assert_eq!(state.visible_channel_id(), None);
        assert_eq!(state.selected_thread_ts(), None);
        assert!(state.channels.is_empty());
        assert!(state.search_results().is_empty());
        assert!(state.files().is_empty());
        assert!(state.saved_items().is_empty());
        assert!(!state.search_loading());
        assert!(!state.files_loading());
        assert!(!state.saved_loading());
    }

    #[test]
    fn workspace_session_reset_clears_its_canonical_domain_state() {
        let session = WorkspaceSessionState::default();
        *session.conversations.borrow_mut() =
            ConversationCatalog::from_cached([crate::models::SlackConversation {
                id: "C1".to_string(),
                ..Default::default()
            }]);
        session.view.borrow_mut().show_unreads();
        let mut thread_root = message("1", "thread root");
        thread_root.reply_count = Some(1);
        session
            .threads
            .borrow_mut()
            .observe_history("C1", &[thread_root]);
        assert!(session.threads.borrow().get("C1", "1").is_some());

        session.reset();

        assert!(session.conversations.borrow().is_empty());
        assert_eq!(
            session.view.borrow().main_view(),
            MainMessageView::Placeholder
        );
        assert!(session.threads.borrow().get("C1", "1").is_none());
    }

    #[test]
    fn conversation_selection_covers_fresh_await_and_current() {
        let mut state = WorkspaceViewState::default();

        let fresh = state.select_conversation("C1");
        assert_eq!(fresh.decision, ConversationSelectionDecision::RequestFresh);
        assert!(fresh.decision.requests_history());
        assert_eq!(fresh.scroll, None);

        let awaiting = state.select_conversation("C1");
        assert_eq!(awaiting.decision, ConversationSelectionDecision::AwaitFresh);
        assert!(!awaiting.decision.requests_history());

        let applied = apply_fresh(&mut state, "C1", vec![message("1", "hello")]);
        assert!(applied.visible);
        assert!(applied.notify_new_messages);
        assert_eq!(applied.scroll, Some(WorkspaceScrollBehavior::Bottom));

        let current = state.select_conversation("C1");
        assert_eq!(
            current.decision,
            ConversationSelectionDecision::RenderCurrent
        );
        assert_eq!(current.scroll, Some(WorkspaceScrollBehavior::StickToBottom));
    }

    #[test]
    fn removing_selected_conversation_clears_navigation_and_cached_history() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        assert_eq!(state.visible_channel_id(), Some("C1"));

        state.remove_conversation("C1");

        assert_eq!(state.visible_channel_id(), None);
        assert_eq!(state.last_channel_id(), None);
        assert_eq!(state.main_view(), MainMessageView::Placeholder);
        assert!(!state.channels.contains_key("C1"));
    }

    #[test]
    fn removing_last_conversation_does_not_interrupt_another_main_view() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        state.show_unreads();

        state.remove_conversation("C1");

        assert_eq!(state.last_channel_id(), None);
        assert_eq!(state.main_view(), MainMessageView::Unreads);
    }

    #[test]
    fn conversation_selection_covers_cached_refresh_and_cached_loading() {
        let mut state = WorkspaceViewState::default();
        let inactive = apply_fresh(&mut state, "C1", vec![message("1", "cached")]);
        assert!(!inactive.visible);

        let cached_refresh = state.select_conversation("C1");
        assert_eq!(
            cached_refresh.decision,
            ConversationSelectionDecision::RenderCachedAndRefresh
        );
        assert!(cached_refresh.decision.requests_history());
        assert_eq!(cached_refresh.scroll, Some(WorkspaceScrollBehavior::Bottom));

        state.show_unreads();
        let cached_again = state.select_conversation("C1");
        assert_eq!(
            cached_again.decision,
            ConversationSelectionDecision::RenderCachedAndRefresh
        );

        apply_fresh(&mut state, "C2", vec![message("2", "other cached")]);
        assert!(state.begin_history_request("C2"));
        let cached_loading = state.select_conversation("C2");
        assert_eq!(
            cached_loading.decision,
            ConversationSelectionDecision::RenderCached
        );
        assert!(!cached_loading.decision.requests_history());
        assert!(!state.begin_history_request("C2"));
    }

    #[test]
    fn loaded_empty_history_is_distinct_from_never_loaded_history() {
        let mut state = WorkspaceViewState::default();
        assert_eq!(
            state.select_conversation("C1").decision,
            ConversationSelectionDecision::RequestFresh
        );
        let loaded_empty = apply_fresh(&mut state, "C1", Vec::new());
        assert!(loaded_empty.visible);

        assert_eq!(
            state.select_conversation("C1").decision,
            ConversationSelectionDecision::RenderCurrent
        );
        state.show_unreads();
        assert_eq!(
            state.select_conversation("C1").decision,
            ConversationSelectionDecision::RenderCachedAndRefresh
        );
    }

    #[test]
    fn leaving_a_loading_view_allows_it_to_be_requested_again() {
        let mut state = WorkspaceViewState::default();
        assert_eq!(
            state.select_conversation("C1").decision,
            ConversationSelectionDecision::RequestFresh
        );
        assert_eq!(
            state.select_conversation("C2").decision,
            ConversationSelectionDecision::RequestFresh
        );
        assert_eq!(
            state.select_conversation("C1").decision,
            ConversationSelectionDecision::RequestFresh
        );

        state.show_unreads();
        assert_eq!(
            state.select_conversation("C1").decision,
            ConversationSelectionDecision::RequestFresh
        );
    }

    #[test]
    fn explicit_history_requests_are_deduplicated_and_errors_clear_loading() {
        let mut state = WorkspaceViewState::default();
        apply_fresh(&mut state, "C1", vec![message("1", "cached one")]);
        apply_fresh(&mut state, "C2", vec![message("2", "cached two")]);
        state.select_conversation("C2");
        apply_fresh(&mut state, "C2", vec![message("2", "cached two")]);
        assert!(state.begin_history_request("C1"));
        assert!(state.begin_history_request("C2"));

        let hidden = state.fail_history("C1");

        assert_eq!(
            hidden,
            WorkspaceFailureOutcome {
                active: false,
                has_content: true,
            }
        );
        assert!(state.begin_history_request("C1"));
        assert!(!state.begin_history_request("C2"));
        assert_eq!(state.visible_channel_id(), Some("C2"));
        assert_eq!(state.channel_messages("C1")[0].body_text(), "cached one");

        let visible = state.fail_history("C2");
        assert_eq!(
            visible,
            WorkspaceFailureOutcome {
                active: true,
                has_content: true,
            }
        );
        assert!(state.begin_history_request("C2"));
    }

    #[test]
    fn thread_failure_clears_only_the_matching_load_and_preserves_messages() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("1", "parent")]);
        state.open_thread("C1", "1");
        state.apply_thread(
            "C1",
            "1",
            vec![message("1", "parent"), message("2", "reply")],
            false,
            None,
            false,
        );
        assert!(state.begin_thread_history_request());

        assert_eq!(
            state.fail_thread("C1", "other"),
            WorkspaceFailureOutcome::default()
        );
        assert!(!state.begin_thread_history_request());

        assert_eq!(
            state.fail_thread("C1", "1"),
            WorkspaceFailureOutcome {
                active: true,
                has_content: true,
            }
        );
        assert!(state.begin_thread_history_request());
        assert_eq!(state.current_thread_messages().len(), 2);
        assert_eq!(state.selected_thread_ts(), Some("1"));
    }

    #[test]
    fn empty_history_and_thread_failures_make_direct_retry_available() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", Vec::new());
        assert_eq!(
            state.select_conversation("C1").decision,
            ConversationSelectionDecision::RenderCurrent
        );
        assert!(state.begin_history_request("C1"));

        assert_eq!(
            state.fail_history("C1"),
            WorkspaceFailureOutcome {
                active: true,
                has_content: false,
            }
        );
        assert_eq!(
            state.select_conversation("C1").decision,
            ConversationSelectionDecision::RequestFresh
        );
        apply_fresh(&mut state, "C1", vec![message("1", "parent")]);
        state.open_thread("C1", "1");
        state.apply_thread("C1", "1", Vec::new(), false, None, false);
        assert_eq!(
            state.open_thread("C1", "1"),
            ThreadOpenOutcome::RenderCurrent
        );
        assert!(state.begin_thread_history_request());

        assert_eq!(
            state.fail_thread("C1", "1"),
            WorkspaceFailureOutcome {
                active: true,
                has_content: false,
            }
        );
        assert_eq!(
            state.open_thread("C1", "1"),
            ThreadOpenOutcome::RequestFresh
        );
    }

    #[test]
    fn surface_failures_clear_only_their_loading_state_and_preserve_content() {
        let mut state = WorkspaceViewState::default();
        state.show_search();
        state.search_loading = true;
        state.files_loading = true;
        state.saved_loading = true;
        state.search_results.push(SearchMatch {
            text: Some("preserved".into()),
            ..SearchMatch::default()
        });
        state.files.push(SlackFile {
            id: Some("F1".into()),
            ..SlackFile::default()
        });
        state.saved_items.push(SavedItem {
            channel: Some("C1".into()),
            ..SavedItem::default()
        });

        assert_eq!(
            state.fail_search(),
            WorkspaceFailureOutcome {
                active: true,
                has_content: true,
            }
        );
        assert!(!state.search_loading());
        assert!(state.files_loading());
        assert!(state.saved_loading());
        assert_eq!(state.search_results()[0].text.as_deref(), Some("preserved"));

        state.show_files();
        state.search_loading = true;
        state.files_loading = true;
        state.saved_loading = true;
        assert_eq!(
            state.fail_files(),
            WorkspaceFailureOutcome {
                active: true,
                has_content: true,
            }
        );
        assert!(state.search_loading());
        assert!(!state.files_loading());
        assert!(state.saved_loading());
        assert_eq!(state.files()[0].id.as_deref(), Some("F1"));

        state.show_saved();
        state.search_loading = true;
        state.files_loading = true;
        state.saved_loading = true;
        assert_eq!(
            state.fail_saved(),
            WorkspaceFailureOutcome {
                active: true,
                has_content: true,
            }
        );
        assert!(state.search_loading());
        assert!(state.files_loading());
        assert!(!state.saved_loading());
        assert_eq!(state.saved_items()[0].channel.as_deref(), Some("C1"));
    }

    #[test]
    fn inactive_surface_failure_does_not_change_the_current_view() {
        let mut state = WorkspaceViewState::default();
        state.start_search();
        state.show_unreads();

        assert_eq!(
            state.fail_search(),
            WorkspaceFailureOutcome {
                active: false,
                has_content: false,
            }
        );
        assert_eq!(state.main_view(), MainMessageView::Unreads);
    }

    #[test]
    fn late_history_updates_only_its_cache_without_navigation_or_read() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("A");
        state.select_conversation("B");

        let outcome = apply_fresh(&mut state, "A", vec![message("1", "late")]);

        assert!(!outcome.visible);
        assert!(!outcome.notify_new_messages);
        assert_eq!(outcome.scroll, None);
        assert_eq!(state.main_view(), MainMessageView::Conversation);
        assert_eq!(state.visible_channel_id(), Some("B"));
        assert_eq!(state.channel_messages("A")[0].body_text(), "late");
    }

    #[test]
    fn late_search_files_and_saved_results_do_not_switch_views() {
        let mut state = WorkspaceViewState::default();
        state.start_search();
        state.show_unreads();
        assert!(!state.apply_search_results(vec![SearchMatch {
            text: Some("late search".into()),
            ..SearchMatch::default()
        }]));
        assert_eq!(state.main_view(), MainMessageView::Unreads);
        assert_eq!(
            state.search_results()[0].text.as_deref(),
            Some("late search")
        );

        state.start_files();
        state.show_placeholder();
        assert!(!state.apply_files(vec![SlackFile {
            id: Some("F1".into()),
            ..SlackFile::default()
        }]));
        assert_eq!(state.main_view(), MainMessageView::Placeholder);
        assert_eq!(state.files()[0].id.as_deref(), Some("F1"));

        state.start_saved();
        state.show_unreads();
        assert!(!state.apply_saved(vec![SavedItem {
            channel: Some("C1".into()),
            ..SavedItem::default()
        }]));
        assert_eq!(state.main_view(), MainMessageView::Unreads);
        assert_eq!(state.saved_items()[0].channel.as_deref(), Some("C1"));
    }

    #[test]
    fn pagination_merges_deduplicates_sorts_and_updates_cursor() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        state.apply_history(
            "C1",
            vec![message("2", "two"), message("4", "four")],
            true,
            Some("page-2".into()),
            false,
            false,
        );
        assert_eq!(state.channel_cursor("C1"), Some("page-2"));
        assert!(state.begin_history_request("C1"));

        let outcome = state.apply_history(
            "C1",
            vec![message("3", "three"), message("2", "duplicate")],
            false,
            Some("ignored".into()),
            true,
            false,
        );

        assert_eq!(
            state
                .channel_messages("C1")
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["4", "3", "2"]
        );
        assert_eq!(state.channel_cursor("C1"), None);
        assert!(state.begin_history_request("C1"));
        assert_eq!(
            outcome.scroll,
            Some(WorkspaceScrollBehavior::PreservePrepend)
        );
        assert!(!outcome.notify_new_messages);
    }

    #[test]
    fn channel_selection_forces_bottom_only_once() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        let first = apply_fresh(&mut state, "C1", vec![message("3", "three")]);
        assert_eq!(first.scroll, Some(WorkspaceScrollBehavior::Bottom));
        let second = apply_fresh(&mut state, "C1", vec![message("3", "three")]);
        assert_eq!(second.scroll, Some(WorkspaceScrollBehavior::StickToBottom));
    }

    #[test]
    fn navigation_closes_thread_but_preserves_last_channel() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("1", "parent")]);
        assert_eq!(
            state.open_thread("C1", "1"),
            ThreadOpenOutcome::RequestFresh
        );

        state.show_unreads();

        assert_eq!(state.last_channel_id(), Some("C1"));
        assert_eq!(state.visible_channel_id(), None);
        assert_eq!(state.selected_thread_ts(), None);
        assert_eq!(state.open_thread("C1", "1"), ThreadOpenOutcome::Ignored);

        state.select_conversation("C1");
        assert_eq!(state.visible_channel_id(), Some("C1"));
    }

    #[test]
    fn stale_thread_result_cannot_replace_active_thread() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(
            &mut state,
            "C1",
            vec![message("2", "parent two"), message("1", "parent one")],
        );
        state.open_thread("C1", "1");
        state.open_thread("C1", "2");

        let stale = state.apply_thread("C1", "1", vec![message("1", "stale")], false, None, false);
        assert_eq!(stale, ThreadApplyOutcome::Ignored);
        assert_eq!(state.selected_thread_ts(), Some("2"));
        assert!(state.current_thread_messages().is_empty());

        let current = state.apply_thread(
            "C1",
            "2",
            vec![message("2.1", "reply")],
            true,
            Some("older".into()),
            false,
        );
        assert_eq!(
            current,
            ThreadApplyOutcome::Applied {
                scroll: WorkspaceScrollBehavior::StickToBottom,
                render: true,
            }
        );
        assert_eq!(state.thread_cursor(), Some("older"));
    }

    #[test]
    fn thread_pagination_is_deduplicated_and_preserves_prepend() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("3", "parent")]);
        state.open_thread("C1", "3");
        state.apply_thread(
            "C1",
            "3",
            vec![message("3", "parent"), message("2", "reply")],
            true,
            Some("older".into()),
            false,
        );
        assert!(state.begin_thread_history_request());

        let outcome = state.apply_thread(
            "C1",
            "3",
            vec![message("2", "duplicate"), message("1", "old")],
            false,
            None,
            true,
        );

        assert_eq!(
            outcome,
            ThreadApplyOutcome::Applied {
                scroll: WorkspaceScrollBehavior::PreservePrepend,
                render: true,
            }
        );
        assert_eq!(
            state
                .current_thread_messages()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["3", "2", "1"]
        );
        assert!(state.begin_thread_history_request());
    }

    #[test]
    fn realtime_messages_update_loaded_channel_and_matching_thread() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("3", "old")]);
        state.open_thread("C1", "3");
        state.apply_thread("C1", "3", vec![message("3", "parent")], false, None, false);

        let changed = state.apply_realtime_message(
            "C1",
            message("3", "edited"),
            RealtimeMessageKind::Changed,
        );
        assert!(changed.channel_changed);
        assert!(changed.render_channel);
        assert!(changed.render_thread);
        assert_eq!(
            changed.channel_scroll,
            Some(WorkspaceScrollBehavior::Preserve)
        );
        assert_eq!(state.channel_messages("C1")[0].body_text(), "edited");
        assert_eq!(state.current_thread_messages()[0].body_text(), "edited");

        let reply = state.apply_realtime_message(
            "C1",
            thread_message("4", "3", "reply"),
            RealtimeMessageKind::Posted,
        );
        assert!(!reply.channel_changed);
        assert!(!reply.render_channel);
        assert!(reply.render_thread);
        assert_eq!(reply.channel_scroll, None);
        assert_eq!(state.channel_messages("C1").len(), 1);
        assert_eq!(state.current_thread_messages()[0].ts, "4");

        state.show_unreads();
        let activity = state.apply_realtime_message(
            "C1",
            message("5", "activity"),
            RealtimeMessageKind::Deleted,
        );
        assert!(activity.refresh_unreads);
        assert!(!activity.render_channel);
    }

    #[test]
    fn first_realtime_messages_populate_loaded_empty_channel_and_thread() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", Vec::new());

        let channel_outcome = state.apply_realtime_message(
            "C1",
            message("1", "first post"),
            RealtimeMessageKind::Posted,
        );
        assert!(channel_outcome.channel_changed);
        assert!(channel_outcome.render_channel);
        assert_eq!(state.channel_messages("C1")[0].body_text(), "first post");

        state.open_thread("C1", "1");
        state.apply_thread("C1", "1", Vec::new(), false, None, false);

        let outcome = state.apply_realtime_message(
            "C1",
            thread_message("2", "1", "first reply"),
            RealtimeMessageKind::Posted,
        );

        assert!(!outcome.channel_changed);
        assert!(!outcome.render_channel);
        assert!(outcome.render_thread);
        assert_eq!(state.channel_messages("C1").len(), 1);
        assert_eq!(
            state.current_thread_messages()[0].body_text(),
            "first reply"
        );
        assert_eq!(
            state.open_thread("C1", "1"),
            ThreadOpenOutcome::RenderCurrent
        );
    }

    #[test]
    fn thread_broadcasts_render_in_both_channel_and_thread() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("3", "parent")]);
        state.open_thread("C1", "3");
        state.apply_thread("C1", "3", vec![message("3", "parent")], false, None, false);
        let mut broadcast = thread_message("4", "3", "broadcast reply");
        broadcast.subtype = Some("thread_broadcast".into());

        let outcome =
            state.apply_realtime_message("C1", broadcast.clone(), RealtimeMessageKind::Posted);

        assert!(outcome.render_channel);
        assert!(outcome.render_thread);
        assert_eq!(
            outcome.channel_scroll,
            Some(WorkspaceScrollBehavior::StickToBottom)
        );
        assert_eq!(state.channel_messages("C1")[0], broadcast);
        assert_eq!(state.current_thread_messages()[0].ts, "4");
    }

    #[test]
    fn confirmed_messages_survive_stale_channel_and_thread_snapshots() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("3", "parent")]);
        state.open_thread("C1", "3");
        state.apply_thread("C1", "3", vec![message("3", "parent")], false, None, false);

        state.apply_realtime_message(
            "C1",
            message("5", "confirmed channel post"),
            RealtimeMessageKind::Posted,
        );
        state.apply_realtime_message(
            "C1",
            thread_message("4", "3", "confirmed reply"),
            RealtimeMessageKind::Posted,
        );

        apply_fresh(&mut state, "C1", vec![message("3", "stale parent")]);
        state.apply_thread(
            "C1",
            "3",
            vec![message("3", "stale parent")],
            false,
            None,
            false,
        );

        assert_eq!(
            state
                .channel_messages("C1")
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["5", "3"]
        );
        assert_eq!(
            state
                .current_thread_messages()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["4", "3"]
        );
    }

    #[test]
    fn identical_snapshots_do_not_require_full_timeline_renders() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        let messages = vec![message("1", "parent")];
        assert!(apply_fresh(&mut state, "C1", messages.clone()).render);
        assert!(!apply_fresh(&mut state, "C1", messages.clone()).render);

        state.open_thread("C1", "1");
        assert!(matches!(
            state.apply_thread("C1", "1", messages.clone(), false, None, false),
            ThreadApplyOutcome::Applied { render: true, .. }
        ));
        assert!(matches!(
            state.apply_thread("C1", "1", messages, false, None, false),
            ThreadApplyOutcome::Applied { render: false, .. }
        ));
    }

    #[test]
    fn identical_realtime_redelivery_is_a_noop() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("1", "existing")]);
        let posted = message("2", "once");
        assert!(
            state
                .apply_realtime_message("C1", posted.clone(), RealtimeMessageKind::Posted)
                .render_channel
        );

        let duplicate = state.apply_realtime_message("C1", posted, RealtimeMessageKind::Posted);

        assert!(!duplicate.channel_changed);
        assert!(!duplicate.render_channel);
        assert_eq!(duplicate.channel_scroll, None);
        assert_eq!(state.channel_messages("C1").len(), 2);
    }

    #[test]
    fn realtime_post_seeds_unopened_conversation_for_immediate_render() {
        let mut state = WorkspaceViewState::default();

        let outcome = state.apply_realtime_message(
            "D1",
            message("2", "new direct message"),
            RealtimeMessageKind::Posted,
        );

        assert!(outcome.channel_changed);
        assert!(!outcome.render_channel);
        assert_eq!(
            state.channel_messages("D1")[0].body_text(),
            "new direct message"
        );
        assert_eq!(
            state.select_conversation("D1").decision,
            ConversationSelectionDecision::RenderCachedAndRefresh
        );
    }

    #[test]
    fn realtime_mutation_does_not_create_phantom_unopened_history() {
        let mut state = WorkspaceViewState::default();

        let outcome = state.apply_realtime_message(
            "D1",
            message("2", "edited"),
            RealtimeMessageKind::Changed,
        );

        assert!(!outcome.channel_changed);
        assert!(state.channel_messages("D1").is_empty());
        assert_eq!(
            state.select_conversation("D1").decision,
            ConversationSelectionDecision::RequestFresh
        );
    }

    #[test]
    fn reactions_update_channel_and_thread_without_double_counting() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("1", "parent")]);
        state.open_thread("C1", "1");
        state.apply_thread("C1", "1", vec![message("1", "parent")], false, None, false);
        let update = ReactionUpdate {
            channel_id: "C1".into(),
            ts: "1".into(),
            name: "heart".into(),
            user_id: "U1".into(),
            added: true,
        };

        let added = state.apply_reaction(&update);
        assert!(added.changed);
        assert!(added.render_channel);
        assert!(added.render_thread);
        assert_eq!(
            state.channel_messages("C1")[0].reactions.as_ref().unwrap()[0].count,
            Some(1)
        );
        assert_eq!(
            state.current_thread_messages()[0]
                .reactions
                .as_ref()
                .unwrap()[0]
                .count,
            Some(1)
        );
        assert!(!state.apply_reaction(&update).changed);
        assert_eq!(
            state.channel_messages("C1")[0].reactions.as_ref().unwrap()[0].count,
            Some(1)
        );
        assert_eq!(
            state.current_thread_messages()[0]
                .reactions
                .as_ref()
                .unwrap()[0]
                .count,
            Some(1)
        );

        let removed = state.apply_reaction(&ReactionUpdate {
            added: false,
            ..update
        });
        assert!(removed.changed);
        assert!(state.channel_messages("C1")[0]
            .reactions
            .as_ref()
            .unwrap()
            .is_empty());
        assert!(state.current_thread_messages()[0]
            .reactions
            .as_ref()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn reaction_removal_updates_counts_when_user_details_are_missing() {
        let mut reacted = message("1", "reacted");
        reacted.reactions = Some(vec![SlackReaction {
            name: Some("heart".into()),
            count: Some(1),
            users: None,
        }]);
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![reacted]);

        let outcome = state.apply_reaction(&ReactionUpdate {
            channel_id: "C1".into(),
            ts: "1".into(),
            name: "heart".into(),
            user_id: "U1".into(),
            added: false,
        });

        assert!(outcome.changed);
        assert!(state.channel_messages("C1")[0]
            .reactions
            .as_ref()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn find_message_uses_authoritative_state() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("1", "parent")]);
        assert_eq!(state.find_message("C1", "1").unwrap().body_text(), "parent");

        state.apply_saved(vec![SavedItem {
            channel: Some("C2".into()),
            message: Some(message("2", "saved")),
            ..SavedItem::default()
        }]);
        assert_eq!(state.find_message("C2", "2").unwrap().body_text(), "saved");
    }

    #[test]
    fn snapshot_uses_last_channel_but_visible_channel_requires_conversation_view() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("1", "one")]);
        state.show_unreads();

        let snapshot = state.snapshot();
        assert_eq!(snapshot.channel_id.as_deref(), Some("C1"));
        assert_eq!(snapshot.channel_messages[0].body_text(), "one");
        assert_eq!(snapshot.main_view, MainMessageView::Unreads);
        assert_eq!(state.visible_channel_id(), None);
    }

    #[test]
    fn message_focus_follows_active_channel_and_clears_on_navigation() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        let location = SearchMessageLocation::new("C1", "2", None).unwrap();

        assert!(state.focus_message(&location));
        assert_eq!(state.channel_focus_ts("C1"), Some("2"));
        assert_eq!(
            state.take_channel_focus_for_render("C1", &[message("1", "other")]),
            None
        );
        assert_eq!(state.channel_focus_ts("C1"), Some("2"));
        assert_eq!(
            state.take_channel_focus_for_render("C1", &[message("2", "target")]),
            Some("2".into())
        );
        assert_eq!(state.channel_focus_ts("C1"), None);

        assert!(state.focus_message(&location));

        state.show_unreads();
        assert_eq!(state.channel_focus_ts("C1"), None);
        assert!(!state.focus_message(&location));

        state.select_conversation("C2");
        let current = SearchMessageLocation::new("C2", "4", None).unwrap();
        assert!(state.focus_message(&current));
        assert!(!state.focus_message(&location));
        assert_eq!(state.channel_focus_ts("C2"), Some("4"));
    }

    #[test]
    fn message_focus_rejects_stale_channel_and_thread_targets() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("1", "parent")]);
        state.open_thread("C1", "1");
        let current = SearchMessageLocation::new("C1", "2", Some("1")).unwrap();
        let stale = SearchMessageLocation::new("C1", "3", Some("other")).unwrap();

        assert!(state.focus_message(&current));
        assert!(!state.focus_message(&stale));
        assert_eq!(state.thread_focus_ts(), Some("2"));
        assert_eq!(
            state.take_thread_focus_for_render("C1", "1", &[message("2", "reply")]),
            Some("2".into())
        );
        assert_eq!(state.thread_focus_ts(), None);

        assert!(state.focus_message(&current));
        state.open_thread("C1", "1");
        assert_eq!(state.thread_focus_ts(), None);
    }

    #[test]
    fn message_context_is_transient_and_never_replaces_channel_history() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("10", "latest")]);
        let location = SearchMessageLocation::new("C1", "2", None).unwrap();
        assert!(state.focus_message(&location));
        assert!(state.apply_message_context(
            &location,
            vec![message("2", "target"), message("1", "older")],
        ));
        assert!(state.has_channel_context("C1"));
        assert_eq!(state.channel_messages("C1")[0].body_text(), "target");
        assert_eq!(state.channels["C1"].messages[0].body_text(), "latest");

        let outcome = state.select_conversation("C1");
        assert_eq!(
            outcome.decision,
            ConversationSelectionDecision::RenderCurrent
        );
        assert!(!state.has_channel_context("C1"));
        assert_eq!(state.channel_messages("C1")[0].body_text(), "latest");
    }

    #[test]
    fn stale_message_context_cannot_change_the_active_view() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("10", "latest")]);
        let location = SearchMessageLocation::new("C1", "2", None).unwrap();
        assert!(state.focus_message(&location));
        state.select_conversation("C2");

        assert!(!state.apply_message_context(&location, vec![message("2", "stale")]));
        assert_eq!(state.visible_channel_id(), Some("C2"));
        assert_eq!(state.channels["C1"].messages[0].body_text(), "latest");
    }

    #[test]
    fn realtime_edits_update_transient_message_context() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        let location = SearchMessageLocation::new("C1", "2", None).unwrap();
        assert!(state.focus_message(&location));
        assert!(state.apply_message_context(&location, vec![message("2", "original")]));

        let outcome = state.apply_realtime_message(
            "C1",
            message("2", "edited"),
            RealtimeMessageKind::Changed,
        );
        assert!(outcome.render_channel);
        assert_eq!(state.channel_messages("C1")[0].body_text(), "edited");
        assert!(state.channels["C1"].messages.is_empty());
    }
}
