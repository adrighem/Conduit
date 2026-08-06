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
    slack_timestamp_is_after, SavedItem, SearchMatch, SearchMessageLocation, SlackConversation,
    SlackFile, SlackMessage, SlackUser,
};
use crate::thread_catalog::ThreadCatalog;
use crate::workspace_pipeline::{
    MessageChange, TimelineTarget, WorkspaceChange, WorkspacePatch, WorkspaceRevision,
};

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
    pub(crate) users: RefCell<HashMap<String, SlackUser>>,
    pub(crate) view: RefCell<WorkspaceViewState>,
    pub(crate) threads: RefCell<ThreadCatalog>,
    workspace_patches: RefCell<WorkspacePatchConsumer>,
}

#[derive(Debug, Default)]
struct WorkspacePatchConsumer {
    revision: WorkspaceRevision,
}

#[derive(Debug)]
pub(crate) struct ConversationPatchRemoval {
    channel_id: String,
    conversation: Option<SlackConversation>,
    was_visible: bool,
}

impl ConversationPatchRemoval {
    pub(crate) fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub(crate) fn conversation(&self) -> Option<&SlackConversation> {
        self.conversation.as_ref()
    }

    pub(crate) fn was_visible(&self) -> bool {
        self.was_visible
    }
}

#[derive(Debug, Default)]
pub(crate) struct WorkspacePatchApplication {
    conversation_changed: bool,
    thread_catalog_changed: bool,
    users_reset: bool,
    changed_user_ids: Vec<String>,
    timeline_changes: Vec<TimelineProjectionApplication>,
    unread_start_by_channel: HashMap<String, String>,
    removals: Vec<ConversationPatchRemoval>,
    acknowledged_local_reads: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TimelineProjectionApplication {
    target: TimelineTarget,
    render: bool,
    derived_view_changed: bool,
    operations: Vec<TimelineProjectionOperation>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TimelineProjectionOperation {
    Upsert {
        message: Box<SlackMessage>,
        inserted: bool,
        position: TimelineProjectionPosition,
    },
    Remove {
        message_ts: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimelineProjectionPosition {
    Append,
    Prepend,
}

impl TimelineProjectionApplication {
    pub(crate) fn target(&self) -> &TimelineTarget {
        &self.target
    }

    pub(crate) fn render(&self) -> bool {
        self.render
    }

    pub(crate) fn derived_view_changed(&self) -> bool {
        self.derived_view_changed
    }

    pub(crate) fn operations(&self) -> &[TimelineProjectionOperation] {
        &self.operations
    }
}

impl WorkspacePatchApplication {
    pub(crate) fn conversation_changed(&self) -> bool {
        self.conversation_changed
    }

    pub(crate) fn thread_catalog_changed(&self) -> bool {
        self.thread_catalog_changed
    }

    pub(crate) fn users_reset(&self) -> bool {
        self.users_reset
    }

    pub(crate) fn changed_user_ids(&self) -> &[String] {
        &self.changed_user_ids
    }

    pub(crate) fn timeline_changes(&self) -> &[TimelineProjectionApplication] {
        &self.timeline_changes
    }

    pub(crate) fn unread_start(&self, channel_id: &str) -> Option<&str> {
        self.unread_start_by_channel
            .get(channel_id)
            .map(String::as_str)
    }

    pub(crate) fn removals(&self) -> &[ConversationPatchRemoval] {
        &self.removals
    }

    pub(crate) fn acknowledged_local_reads(&self) -> &[String] {
        &self.acknowledged_local_reads
    }
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
        self.users.borrow_mut().clear();
        self.view.borrow_mut().reset();
        *self.threads.borrow_mut() = ThreadCatalog::default();
        *self.workspace_patches.borrow_mut() = WorkspacePatchConsumer::default();
    }

    #[allow(dead_code)]
    pub(crate) fn workspace_patch_revision(&self) -> WorkspaceRevision {
        self.workspace_patches.borrow().revision
    }

    #[cfg(test)]
    pub(crate) fn apply_workspace_patch(
        &self,
        patch: &WorkspacePatch,
    ) -> Option<WorkspacePatchApplication> {
        self.apply_workspace_patch_with_local_reads(patch, &HashMap::new())
    }

    pub(crate) fn apply_workspace_patch_with_local_reads(
        &self,
        patch: &WorkspacePatch,
        local_read_ts_by_channel: &HashMap<String, String>,
    ) -> Option<WorkspacePatchApplication> {
        let mut consumer = self.workspace_patches.borrow_mut();
        if patch.revision() <= consumer.revision {
            return None;
        }

        let mut catalog = self.conversations.borrow_mut();
        let mut users = self.users.borrow_mut();
        let mut view = self.view.borrow_mut();
        let mut threads = self.threads.borrow_mut();
        let mut application = WorkspacePatchApplication::default();
        for change in patch.changes() {
            match change {
                WorkspaceChange::BootstrapReset(data) => {
                    replace_patch_conversations(
                        &mut catalog,
                        &mut view,
                        &data.conversations,
                        &mut application,
                    );
                    *threads = ThreadCatalog::from_records(data.threads.clone());
                    application.thread_catalog_changed = true;
                    replace_patch_users(&mut users, &data.users, &mut application);
                }
                WorkspaceChange::ConversationsReset(conversations) => {
                    replace_patch_conversations(
                        &mut catalog,
                        &mut view,
                        conversations,
                        &mut application,
                    );
                }
                WorkspaceChange::ConversationUpsert(conversation) => {
                    if local_read_ts_by_channel
                        .get(&conversation.id)
                        .is_some_and(|local_read| {
                            conversation.local_read_ts().is_some_and(|acknowledged| {
                                acknowledged == local_read
                                    || slack_timestamp_is_after(acknowledged, local_read)
                            })
                        })
                    {
                        application
                            .acknowledged_local_reads
                            .push(conversation.id.clone());
                    }
                    catalog.upsert_authoritative(conversation.clone());
                    application.conversation_changed = true;
                }
                WorkspaceChange::ConversationMetadataUpsert(conversation) => {
                    catalog.upsert_metadata(conversation.clone());
                    application.conversation_changed = true;
                }
                WorkspaceChange::ConversationAttentionObserved {
                    channel_id,
                    observations,
                } => {
                    let local_read =
                        local_read_ts_by_channel
                            .get(channel_id)
                            .cloned()
                            .or_else(|| {
                                catalog
                                    .get(channel_id)
                                    .and_then(SlackConversation::local_read_ts)
                                    .map(str::to_string)
                            });
                    for observation in observations {
                        if local_read.as_deref().is_some_and(|last_read| {
                            !slack_timestamp_is_after(&observation.message_ts, last_read)
                        }) {
                            continue;
                        }
                        let had_classified_unread = catalog
                            .get(channel_id)
                            .and_then(|conversation| conversation.attention.as_ref())
                            .is_some_and(|attention| {
                                attention.has_unread || attention.unread_count > 0
                            });
                        let changed = catalog
                            .apply_attention_observation(
                                channel_id,
                                &observation.message_ts,
                                observation.record_unread,
                            )
                            .1;
                        application.conversation_changed |= changed;
                        if changed
                            && observation.record_unread
                            && !had_classified_unread
                            && catalog
                                .get(channel_id)
                                .is_some_and(SlackConversation::has_unread_activity)
                        {
                            application
                                .unread_start_by_channel
                                .entry(channel_id.clone())
                                .and_modify(|existing| {
                                    if slack_timestamp_is_after(existing, &observation.message_ts) {
                                        *existing = observation.message_ts.clone();
                                    }
                                })
                                .or_insert_with(|| observation.message_ts.clone());
                        }
                    }
                }
                WorkspaceChange::ConversationRemoved { channel_id } => {
                    application.removals.push(ConversationPatchRemoval {
                        channel_id: channel_id.clone(),
                        conversation: catalog.remove(channel_id),
                        was_visible: view.visible_channel_id() == Some(channel_id),
                    });
                    view.remove_conversation(channel_id);
                    application.conversation_changed = true;
                }
                WorkspaceChange::UnreadChanged { snapshot } => {
                    if !snapshot.unread_state.known || snapshot.channel_id.trim().is_empty() {
                        continue;
                    }
                    let local_read = local_read_ts_by_channel.get(&snapshot.channel_id);
                    let newer_local_read = local_read.is_some_and(|local| {
                        snapshot
                            .last_read
                            .as_deref()
                            .is_none_or(|server| slack_timestamp_is_after(local.as_str(), server))
                    });
                    if newer_local_read {
                        continue;
                    }
                    if local_read.is_some() && snapshot.last_read.is_some() {
                        application
                            .acknowledged_local_reads
                            .push(snapshot.channel_id.clone());
                    }
                    application.conversation_changed |= catalog.apply_unread_snapshot(snapshot);
                }
                WorkspaceChange::ThreadCatalogChanged(records) => {
                    *threads = ThreadCatalog::from_records(records.clone());
                    application.thread_catalog_changed = true;
                }
                WorkspaceChange::UsersReset(updated) => {
                    replace_patch_users(&mut users, updated, &mut application);
                }
                WorkspaceChange::UserUpsert(user) => {
                    let Some(user_id) = user
                        .id
                        .as_deref()
                        .map(str::trim)
                        .filter(|user_id| !user_id.is_empty())
                    else {
                        continue;
                    };
                    if users.get(user_id) != Some(user) {
                        users.insert(user_id.to_string(), user.clone());
                        application.changed_user_ids.push(user_id.to_string());
                    }
                }
                WorkspaceChange::TimelineChanged { target, changes } => {
                    if let Some(timeline_change) = view.apply_timeline_changes(target, changes) {
                        if let Some(existing) = application
                            .timeline_changes
                            .iter_mut()
                            .find(|existing| existing.target == timeline_change.target)
                        {
                            existing.render |= timeline_change.render;
                            existing.derived_view_changed |= timeline_change.derived_view_changed;
                            existing.operations.extend(timeline_change.operations);
                        } else {
                            application.timeline_changes.push(timeline_change);
                        }
                    }
                }
            }
        }
        consumer.revision = patch.revision();
        Some(application)
    }
}

fn replace_patch_users(
    users: &mut HashMap<String, SlackUser>,
    updated: &[SlackUser],
    application: &mut WorkspacePatchApplication,
) {
    users.clear();
    for user in updated {
        let Some(user_id) = user
            .id
            .as_deref()
            .map(str::trim)
            .filter(|user_id| !user_id.is_empty())
        else {
            continue;
        };
        users.insert(user_id.to_string(), user.clone());
    }
    application.users_reset = true;
    application.changed_user_ids = users.keys().cloned().collect();
    application.changed_user_ids.sort();
}

fn replace_patch_conversations(
    catalog: &mut ConversationCatalog,
    view: &mut WorkspaceViewState,
    conversations: &[SlackConversation],
    application: &mut WorkspacePatchApplication,
) {
    let incoming_ids = conversations
        .iter()
        .map(|conversation| conversation.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    for conversation in catalog
        .conversations()
        .into_iter()
        .filter(|conversation| !incoming_ids.contains(conversation.id.as_str()))
    {
        let channel_id = conversation.id.clone();
        application.removals.push(ConversationPatchRemoval {
            was_visible: view.visible_channel_id() == Some(channel_id.as_str()),
            channel_id: channel_id.clone(),
            conversation: Some(conversation),
        });
        view.remove_conversation(&channel_id);
    }
    *catalog = ConversationCatalog::from_cached(conversations.iter().cloned());
    application.conversation_changed = true;
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
    RenderCachedAndRefresh,
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

pub(crate) type ConversationOpenGeneration = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConversationOpenIntent {
    Latest,
    FirstUnread {
        last_read: Option<String>,
        unread_count: u64,
    },
    Message(String),
}

impl ConversationOpenIntent {
    pub(crate) fn choose(
        explicit_message_ts: Option<&str>,
        has_unread: bool,
        last_read: Option<&str>,
        unread_count: u64,
    ) -> Self {
        if let Some(message_ts) = explicit_message_ts.filter(|ts| !ts.trim().is_empty()) {
            Self::Message(message_ts.to_string())
        } else if has_unread {
            Self::FirstUnread {
                last_read: last_read
                    .filter(|ts| !ts.trim().is_empty())
                    .map(ToString::to_string),
                unread_count,
            }
        } else {
            Self::Latest
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConversationOpenPosition {
    Latest,
    Message(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConversationOpenPhase {
    Positioning,
    Interactive,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConversationOpenRenderAction {
    InitialDocument,
    HoldReconciliation,
    Reconcile,
}

#[derive(Debug, Clone)]
struct ConversationOpenSession {
    generation: ConversationOpenGeneration,
    channel_id: String,
    intent: ConversationOpenIntent,
    resolved_position: Option<ConversationOpenPosition>,
    phase: ConversationOpenPhase,
    document_submitted: bool,
    pending_reconciliation: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ConversationOpenCoordinator {
    next_generation: ConversationOpenGeneration,
    active: Option<ConversationOpenSession>,
}

impl ConversationOpenCoordinator {
    pub(crate) fn begin(
        &mut self,
        channel_id: &str,
        intent: ConversationOpenIntent,
    ) -> ConversationOpenGeneration {
        self.next_generation = self.next_generation.saturating_add(1);
        let generation = self.next_generation;
        self.active = Some(ConversationOpenSession {
            generation,
            channel_id: channel_id.to_string(),
            intent,
            resolved_position: None,
            phase: ConversationOpenPhase::Positioning,
            document_submitted: false,
            pending_reconciliation: false,
        });
        generation
    }

    #[cfg(test)]
    pub(crate) fn active_phase(&self) -> Option<ConversationOpenPhase> {
        self.active.as_ref().map(|session| session.phase)
    }

    pub(crate) fn positioning_generation_for(
        &self,
        channel_id: &str,
    ) -> Option<ConversationOpenGeneration> {
        self.active
            .as_ref()
            .filter(|session| {
                session.channel_id == channel_id
                    && session.phase == ConversationOpenPhase::Positioning
            })
            .map(|session| session.generation)
    }

    pub(crate) fn active_generation_for(
        &self,
        channel_id: &str,
    ) -> Option<ConversationOpenGeneration> {
        self.active
            .as_ref()
            .filter(|session| session.channel_id == channel_id)
            .map(|session| session.generation)
    }

    pub(crate) fn active_waits_for_explicit_target(&self, channel_id: &str) -> bool {
        self.active.as_ref().is_some_and(|session| {
            session.channel_id == channel_id
                && session.phase == ConversationOpenPhase::Positioning
                && matches!(session.intent, ConversationOpenIntent::Message(_))
                && session.resolved_position.is_none()
        })
    }

    pub(crate) fn reset(&mut self) {
        self.active = None;
    }

    pub(crate) fn note_render_requested(
        &mut self,
        generation: ConversationOpenGeneration,
    ) -> Option<ConversationOpenRenderAction> {
        let session = self
            .active
            .as_mut()
            .filter(|session| session.generation == generation)?;
        match session.phase {
            ConversationOpenPhase::Positioning if !session.document_submitted => {
                session.document_submitted = true;
                Some(ConversationOpenRenderAction::InitialDocument)
            }
            ConversationOpenPhase::Positioning => {
                session.pending_reconciliation = true;
                Some(ConversationOpenRenderAction::HoldReconciliation)
            }
            ConversationOpenPhase::Interactive | ConversationOpenPhase::Cancelled => {
                Some(ConversationOpenRenderAction::Reconcile)
            }
        }
    }

    pub(crate) fn take_pending_reconciliation(
        &mut self,
        generation: ConversationOpenGeneration,
    ) -> bool {
        let Some(session) = self
            .active
            .as_mut()
            .filter(|session| session.generation == generation)
        else {
            return false;
        };
        std::mem::take(&mut session.pending_reconciliation)
    }

    pub(crate) fn resolve_position(
        &mut self,
        generation: ConversationOpenGeneration,
        channel_id: &str,
        messages: &[SlackMessage],
    ) -> Option<ConversationOpenPosition> {
        let session = self.active.as_mut().filter(|session| {
            session.generation == generation
                && session.channel_id == channel_id
                && session.phase == ConversationOpenPhase::Positioning
        })?;
        if let Some(position) = session.resolved_position.clone() {
            return Some(position);
        }
        let position = match &session.intent {
            ConversationOpenIntent::Latest => ConversationOpenPosition::Latest,
            ConversationOpenIntent::FirstUnread {
                last_read,
                unread_count,
            } => ConversationOpenPosition::Message(resolve_first_unread_message_ts(
                messages,
                last_read.as_deref(),
                *unread_count,
            )?),
            ConversationOpenIntent::Message(message_ts) => messages
                .iter()
                .any(|message| message.ts == *message_ts)
                .then(|| ConversationOpenPosition::Message(message_ts.clone()))?,
        };
        session.resolved_position = Some(position.clone());
        Some(position)
    }

    pub(crate) fn commit_position(&mut self, generation: ConversationOpenGeneration) -> bool {
        let Some(session) = self
            .active
            .as_mut()
            .filter(|session| session.generation == generation)
        else {
            return false;
        };
        if session.phase != ConversationOpenPhase::Positioning
            || session.resolved_position.is_none()
        {
            return false;
        }
        session.phase = ConversationOpenPhase::Interactive;
        true
    }

    pub(crate) fn note_user_interaction(&mut self, generation: ConversationOpenGeneration) -> bool {
        let Some(session) = self
            .active
            .as_mut()
            .filter(|session| session.generation == generation)
        else {
            return false;
        };
        if session.phase != ConversationOpenPhase::Positioning {
            return false;
        }
        session.phase = ConversationOpenPhase::Cancelled;
        true
    }
}

pub(crate) fn resolve_first_unread_message_ts(
    messages: &[SlackMessage],
    last_read: Option<&str>,
    unread_count: u64,
) -> Option<String> {
    let mut timestamps = messages
        .iter()
        .map(|message| message.ts.as_str())
        .filter(|ts| !ts.is_empty())
        .collect::<Vec<_>>();
    timestamps.sort_unstable();
    if let Some(last_read) = last_read.filter(|ts| !ts.trim().is_empty()) {
        if let Some(timestamp) = timestamps
            .iter()
            .copied()
            .find(|timestamp| *timestamp > last_read)
        {
            return Some(timestamp.to_string());
        }
    }
    if unread_count == 0 {
        return None;
    }
    let index = timestamps.len().saturating_sub(unread_count as usize);
    timestamps
        .get(index)
        .map(|timestamp| (*timestamp).to_string())
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
    thread_timelines: HashMap<(String, String), Vec<SlackMessage>>,
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

    pub(crate) fn selected_thread_target(&self) -> Option<(&str, &str)> {
        self.thread
            .as_ref()
            .map(|thread| (thread.channel_id.as_str(), thread.ts.as_str()))
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

    pub(crate) fn channel_tail_messages(&self, channel_id: &str) -> &[SlackMessage] {
        self.channels
            .get(channel_id)
            .map(|history| history.messages.as_slice())
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

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn show_placeholder(&mut self) {
        self.navigate_to(MainMessageView::Placeholder);
    }

    pub(crate) fn remove_conversation(&mut self, channel_id: &str) {
        self.channels.remove(channel_id);
        self.thread_timelines
            .retain(|(known_channel_id, _), _| known_channel_id != channel_id);
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

    pub(crate) fn apply_saved_update(
        &mut self,
        channel_id: &str,
        message_ts: &str,
        saved: bool,
        message: Option<SlackMessage>,
    ) -> bool {
        self.saved_items.retain(|item| {
            let item_channel = item.channel.as_deref().or(item.group.as_deref());
            item_channel != Some(channel_id)
                || item.message.as_ref().map(|message| message.ts.as_str()) != Some(message_ts)
        });
        if saved {
            if let Some(message) = message {
                self.saved_items.push(SavedItem {
                    kind: Some("message".to_string()),
                    channel: Some(channel_id.to_string()),
                    message: Some(message),
                    ..Default::default()
                });
                self.saved_items.sort_by(|left, right| {
                    right
                        .message
                        .as_ref()
                        .map(|message| message.ts.as_str())
                        .cmp(&left.message.as_ref().map(|message| message.ts.as_str()))
                });
            }
        }
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

    #[cfg(test)]
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

    pub(crate) fn complete_history_load(
        &mut self,
        channel_id: &str,
        has_more: bool,
        next_cursor: Option<String>,
        append_older: bool,
        cached: bool,
    ) -> HistoryApplyOutcome {
        let visible = self.visible_channel_id() == Some(channel_id);
        let history = self.channels.entry(channel_id.to_string()).or_default();
        let was_loaded = history.loaded;
        let had_context = history.context_messages.is_some();
        history.loaded = true;
        if !cached {
            history.next_cursor = usable_cursor(has_more, next_cursor);
            history.loading = false;
            history.context_messages = None;
        }

        HistoryApplyOutcome {
            visible,
            render: visible && (!was_loaded || (had_context && !cached && !append_older)),
            notify_new_messages: visible && !cached && !append_older,
            scroll: visible.then(|| self.take_channel_scroll(channel_id, append_older)),
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

        let messages = self
            .thread_timelines
            .get(&(channel_id.to_string(), ts.to_string()))
            .cloned()
            .unwrap_or_default();
        let has_cached_messages = !messages.is_empty();
        self.thread = Some(ThreadViewState {
            channel_id: channel_id.to_string(),
            ts: ts.to_string(),
            messages,
            context_messages: None,
            next_cursor: None,
            status: ThreadLoadStatus::Loading,
            focus_ts: None,
        });
        if has_cached_messages {
            ThreadOpenOutcome::RenderCachedAndRefresh
        } else {
            ThreadOpenOutcome::RequestFresh
        }
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
    #[cfg(test)]
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
        self.thread_timelines.insert(
            (channel_id.to_string(), ts.to_string()),
            thread.messages.clone(),
        );
        ThreadApplyOutcome::Applied {
            scroll: if append_older {
                WorkspaceScrollBehavior::PreservePrepend
            } else {
                WorkspaceScrollBehavior::StickToBottom
            },
            render: !was_ready || had_context || thread.messages != previous_messages,
        }
    }

    pub(crate) fn complete_thread_load(
        &mut self,
        channel_id: &str,
        ts: &str,
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
        let had_context = thread.context_messages.is_some();
        thread.status = ThreadLoadStatus::Ready;
        thread.context_messages = None;
        thread.next_cursor = usable_cursor(has_more, next_cursor);
        ThreadApplyOutcome::Applied {
            scroll: if append_older {
                WorkspaceScrollBehavior::PreservePrepend
            } else {
                WorkspaceScrollBehavior::StickToBottom
            },
            render: !was_ready || had_context,
        }
    }

    pub(crate) fn thread_cursor(&self) -> Option<&str> {
        self.thread
            .as_ref()
            .and_then(|thread| thread.next_cursor.as_deref())
    }

    fn apply_timeline_changes(
        &mut self,
        target: &TimelineTarget,
        changes: &[MessageChange],
    ) -> Option<TimelineProjectionApplication> {
        if changes.is_empty() {
            return None;
        }
        let channel_id = match target {
            TimelineTarget::Channel(channel_id) => channel_id.as_str(),
            TimelineTarget::Thread { channel_id, .. } => channel_id.as_str(),
        };
        let mut search_changed = false;
        let mut saved_changed = false;
        for change in changes {
            match change {
                MessageChange::Upsert(message) => {
                    search_changed |=
                        update_search_message(&mut self.search_results, channel_id, message);
                    saved_changed |=
                        update_saved_message(&mut self.saved_items, channel_id, message);
                }
                MessageChange::Remove { message_ts } => {
                    search_changed |=
                        remove_search_message(&mut self.search_results, channel_id, message_ts);
                    saved_changed |=
                        remove_saved_message(&mut self.saved_items, channel_id, message_ts);
                }
            }
        }

        let (timeline_changed, render, operations) = match target {
            TimelineTarget::Channel(channel_id) => {
                let visible = self.visible_channel_id() == Some(channel_id.as_str());
                let history = self.channels.entry(channel_id.clone()).or_default();
                let operations = timeline_projection_operations(
                    &history.messages,
                    changes,
                    SlackMessage::belongs_in_channel_timeline,
                );
                let base_changed = apply_projection_message_changes(
                    &mut history.messages,
                    changes,
                    SlackMessage::belongs_in_channel_timeline,
                );
                let context_changed = history
                    .context_messages
                    .as_mut()
                    .is_some_and(|messages| apply_context_message_changes(messages, changes));
                if base_changed {
                    history.loaded = true;
                }
                let changed = base_changed || context_changed;
                (changed, visible && changed, operations)
            }
            TimelineTarget::Thread {
                channel_id,
                thread_ts,
            } => {
                let key = (channel_id.clone(), thread_ts.clone());
                let (base_changed, projected_messages, operations) = {
                    let messages = self.thread_timelines.entry(key).or_default();
                    let operations = timeline_projection_operations(messages, changes, |message| {
                        message.belongs_to_thread(thread_ts)
                    });
                    let changed = apply_projection_message_changes(messages, changes, |message| {
                        message.belongs_to_thread(thread_ts)
                    });
                    (changed, changed.then(|| messages.clone()), operations)
                };
                let active_changed = self
                    .thread
                    .as_mut()
                    .filter(|thread| thread.channel_id == *channel_id && thread.ts == *thread_ts)
                    .is_some_and(|thread| {
                        if let Some(messages) = projected_messages.as_ref() {
                            thread.messages.clone_from(messages);
                        }
                        let context_changed = thread
                            .context_messages
                            .as_mut()
                            .is_some_and(|context| apply_context_message_changes(context, changes));
                        base_changed || context_changed
                    });
                (base_changed || active_changed, active_changed, operations)
            }
        };
        let derived_view_changed = (self.main_view == MainMessageView::Search && search_changed)
            || (self.main_view == MainMessageView::Saved && saved_changed);
        let projection_changed = timeline_changed || search_changed || saved_changed;
        projection_changed.then(|| TimelineProjectionApplication {
            target: target.clone(),
            render,
            derived_view_changed,
            operations,
        })
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

fn update_search_message(
    results: &mut [SearchMatch],
    channel_id: &str,
    message: &SlackMessage,
) -> bool {
    let matches_message = |result: &SearchMatch| {
        result
            .channel
            .as_ref()
            .and_then(|channel| channel.id.as_deref())
            == Some(channel_id)
            && result.ts.as_deref() == Some(message.ts.as_str())
    };
    let text = Some(message.body_text());
    let mut changed = false;
    for result in results.iter_mut().filter(|result| matches_message(result)) {
        if result.text != text {
            result.text.clone_from(&text);
            changed = true;
        }
    }
    changed
}

fn remove_search_message(
    results: &mut Vec<SearchMatch>,
    channel_id: &str,
    message_ts: &str,
) -> bool {
    let previous_len = results.len();
    results.retain(|result| {
        result
            .channel
            .as_ref()
            .and_then(|channel| channel.id.as_deref())
            != Some(channel_id)
            || result.ts.as_deref() != Some(message_ts)
    });
    results.len() != previous_len
}

fn update_saved_message(items: &mut [SavedItem], channel_id: &str, message: &SlackMessage) -> bool {
    let matches_message = |item: &SavedItem| {
        item.channel.as_deref() == Some(channel_id)
            && item.message.as_ref().map(|message| message.ts.as_str()) == Some(message.ts.as_str())
    };
    let mut changed = false;
    for item in items.iter_mut().filter(|item| matches_message(item)) {
        if item.message.as_ref() != Some(message) {
            item.message = Some(message.clone());
            changed = true;
        }
    }
    changed
}

fn remove_saved_message(items: &mut Vec<SavedItem>, channel_id: &str, message_ts: &str) -> bool {
    let previous_len = items.len();
    items.retain(|item| {
        item.channel.as_deref() != Some(channel_id)
            || item.message.as_ref().map(|message| message.ts.as_str()) != Some(message_ts)
    });
    items.len() != previous_len
}

fn usable_cursor(has_more: bool, cursor: Option<String>) -> Option<String> {
    cursor.filter(|cursor| has_more && !cursor.trim().is_empty())
}

fn apply_projection_message_changes(
    messages: &mut Vec<SlackMessage>,
    changes: &[MessageChange],
    accepts: impl Fn(&SlackMessage) -> bool,
) -> bool {
    let mut changed = false;
    for change in changes {
        match change {
            MessageChange::Upsert(message) if accepts(message) => {
                if let Some(existing) = messages.iter_mut().find(|known| known.ts == message.ts) {
                    if existing != message.as_ref() {
                        existing.clone_from(message);
                        changed = true;
                    }
                } else {
                    messages.push((**message).clone());
                    changed = true;
                }
            }
            MessageChange::Upsert(_) => {}
            MessageChange::Remove { message_ts } => {
                let previous_len = messages.len();
                messages.retain(|message| message.ts != *message_ts);
                changed |= messages.len() != previous_len;
            }
        }
    }
    if changed {
        messages.sort_by(|left, right| right.ts.cmp(&left.ts));
        messages.dedup_by(|left, right| !left.ts.is_empty() && left.ts == right.ts);
    }
    changed
}

fn timeline_projection_operations(
    messages: &[SlackMessage],
    changes: &[MessageChange],
    accepts: impl Fn(&SlackMessage) -> bool,
) -> Vec<TimelineProjectionOperation> {
    let mut known = messages
        .iter()
        .map(|message| (message.ts.clone(), message.clone()))
        .collect::<HashMap<_, _>>();
    let mut operations = Vec::new();
    for change in changes {
        match change {
            MessageChange::Upsert(message) if accepts(message) => {
                let inserted = !known.contains_key(&message.ts);
                if known.get(&message.ts) != Some(message.as_ref()) {
                    let position = if inserted
                        && !messages.is_empty()
                        && messages
                            .iter()
                            .all(|known| slack_timestamp_is_after(&known.ts, &message.ts))
                    {
                        TimelineProjectionPosition::Prepend
                    } else {
                        TimelineProjectionPosition::Append
                    };
                    known.insert(message.ts.clone(), (**message).clone());
                    operations.push(TimelineProjectionOperation::Upsert {
                        message: message.clone(),
                        inserted,
                        position,
                    });
                }
            }
            MessageChange::Upsert(_) => {}
            MessageChange::Remove { message_ts } => {
                if known.remove(message_ts).is_some() {
                    operations.push(TimelineProjectionOperation::Remove {
                        message_ts: message_ts.clone(),
                    });
                }
            }
        }
    }
    operations
}

fn apply_context_message_changes(
    messages: &mut Vec<SlackMessage>,
    changes: &[MessageChange],
) -> bool {
    let mut changed = false;
    for change in changes {
        match change {
            MessageChange::Upsert(message) => {
                let Some(existing) = messages.iter_mut().find(|known| known.ts == message.ts)
                else {
                    continue;
                };
                if existing != message.as_ref() {
                    existing.clone_from(message);
                    changed = true;
                }
            }
            MessageChange::Remove { message_ts } => {
                let previous_len = messages.len();
                messages.retain(|message| message.ts != *message_ts);
                changed |= messages.len() != previous_len;
            }
        }
    }
    changed
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

#[cfg(test)]
fn merge_message_pages(existing: &[SlackMessage], page: &[SlackMessage]) -> Vec<SlackMessage> {
    let mut messages = existing.to_vec();
    messages.extend(page.iter().cloned());
    normalize_messages(messages)
}

#[cfg(test)]
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
    normalize_channel_messages(page.iter().chain(existing).cloned().collect::<Vec<_>>())
}

fn merge_channel_message_refresh(
    _existing: &[SlackMessage],
    snapshot: &[SlackMessage],
) -> Vec<SlackMessage> {
    normalize_channel_messages(snapshot.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{SlackConversationUnreadSnapshot, SlackUnreadState};
    use crate::workspace_pipeline::{
        ConversationAttentionObservation, MessageChange, TimelineTarget, WorkspaceBootstrapData,
        WorkspaceChange, WorkspacePatch, WorkspaceRevision,
    };

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

    fn conversation(id: &str, name: &str) -> crate::models::SlackConversation {
        crate::models::SlackConversation {
            id: id.to_string(),
            name: Some(name.to_string()),
            is_channel: Some(true),
            ..Default::default()
        }
    }

    fn conversation_patch(revision: WorkspaceRevision, change: WorkspaceChange) -> WorkspacePatch {
        WorkspacePatch::new(revision, vec![change]).unwrap()
    }

    #[test]
    fn conversation_patch_consumer_accepts_gaps_and_rejects_stale_or_duplicate_rollbacks() {
        let state = WorkspaceSessionState::default();
        let revision_one = WorkspaceRevision::INITIAL.successor();
        let revision_two = revision_one.successor();
        let revision_three = revision_two.successor();
        let revision_four = revision_three.successor();

        let bootstrap = conversation_patch(
            revision_one,
            WorkspaceChange::BootstrapReset(WorkspaceBootstrapData {
                conversations: vec![conversation("C1", "old")],
                ..Default::default()
            }),
        );
        assert!(state
            .apply_workspace_patch(&bootstrap)
            .unwrap()
            .conversation_changed());
        state.view.borrow_mut().select_conversation("C1");

        let newer = conversation_patch(
            revision_three,
            WorkspaceChange::ConversationUpsert(conversation("C1", "new")),
        );
        assert!(state
            .apply_workspace_patch(&newer)
            .unwrap()
            .conversation_changed());
        assert_eq!(state.workspace_patch_revision(), revision_three);

        let duplicate = conversation_patch(
            revision_three,
            WorkspaceChange::ConversationRemoved {
                channel_id: "C1".to_string(),
            },
        );
        assert!(state.apply_workspace_patch(&duplicate).is_none());
        let stale = conversation_patch(
            revision_two,
            WorkspaceChange::ConversationUpsert(conversation("C1", "rollback")),
        );
        assert!(state.apply_workspace_patch(&stale).is_none());
        assert_eq!(
            state
                .conversations
                .borrow()
                .get("C1")
                .and_then(|conversation| conversation.name.as_deref()),
            Some("new")
        );
        assert_eq!(
            state.view.borrow().visible_channel_id(),
            Some("C1"),
            "rejected patches must not mutate navigation"
        );

        let removal = conversation_patch(
            revision_four,
            WorkspaceChange::ConversationRemoved {
                channel_id: "C1".to_string(),
            },
        );
        let application = state.apply_workspace_patch(&removal).unwrap();
        assert_eq!(application.removals().len(), 1);
        assert_eq!(application.removals()[0].channel_id(), "C1");
        assert!(application.removals()[0].was_visible());
        assert!(state.conversations.borrow().get("C1").is_none());
        assert_eq!(state.view.borrow().visible_channel_id(), None);
    }

    #[test]
    fn conversation_patch_consumer_applies_unread_gaps_without_rolling_back_a_local_read() {
        let state = WorkspaceSessionState::default();
        let revision_one = WorkspaceRevision::INITIAL.successor();
        let revision_two = revision_one.successor();
        let revision_three = revision_two.successor();
        let revision_four = revision_three.successor();
        let revision_five = revision_four.successor();
        let mut initial = conversation("C1", "general");
        initial.apply_unread_snapshot(&SlackConversationUnreadSnapshot {
            channel_id: "C1".to_string(),
            unread_state: SlackUnreadState::from_parts(true, true, 5),
            last_read: Some("10.0".to_string()),
            latest: Some("15.0".to_string()),
            mention_count: Some(1),
            is_open: None,
        });
        state
            .apply_workspace_patch(&conversation_patch(
                revision_one,
                WorkspaceChange::BootstrapReset(WorkspaceBootstrapData {
                    conversations: vec![initial],
                    ..Default::default()
                }),
            ))
            .unwrap();

        state
            .conversations
            .borrow_mut()
            .advance_read_cursor("C1", "20.0", 0);
        let local_reads = HashMap::from([("C1".to_string(), "20.0".to_string())]);
        let delayed_unread = conversation_patch(
            revision_three,
            WorkspaceChange::UnreadChanged {
                snapshot: SlackConversationUnreadSnapshot {
                    channel_id: "C1".to_string(),
                    unread_state: SlackUnreadState::from_parts(true, true, 9),
                    last_read: Some("11.0".to_string()),
                    latest: Some("19.0".to_string()),
                    mention_count: Some(3),
                    is_open: None,
                },
            },
        );
        let delayed = state
            .apply_workspace_patch_with_local_reads(&delayed_unread, &local_reads)
            .unwrap();
        assert!(!delayed.conversation_changed());
        assert!(delayed.acknowledged_local_reads().is_empty());
        assert_eq!(state.workspace_patch_revision(), revision_three);
        assert!(state
            .apply_workspace_patch_with_local_reads(
                &conversation_patch(
                    revision_three,
                    WorkspaceChange::ConversationRemoved {
                        channel_id: "C1".to_string(),
                    },
                ),
                &local_reads,
            )
            .is_none());
        assert!(state
            .apply_workspace_patch_with_local_reads(
                &conversation_patch(
                    revision_two,
                    WorkspaceChange::UnreadChanged {
                        snapshot: SlackConversationUnreadSnapshot {
                            channel_id: "C1".to_string(),
                            unread_state: SlackUnreadState::from_parts(true, true, 7),
                            last_read: None,
                            latest: Some("18.0".to_string()),
                            mention_count: Some(2),
                            is_open: None,
                        },
                    },
                ),
                &local_reads,
            )
            .is_none());

        {
            let catalog = state.conversations.borrow();
            let current = catalog.get("C1").unwrap();
            assert_eq!(current.unread_state().display_count, 0);
            assert_eq!(current.raw_unread_state().display_count, 0);
            assert_eq!(current.raw_unread_activity_count(), 0);
            assert_eq!(current.last_read_ts(), Some("20.0"));
        }

        let cursorless = state
            .apply_workspace_patch_with_local_reads(
                &conversation_patch(
                    revision_four,
                    WorkspaceChange::UnreadChanged {
                        snapshot: SlackConversationUnreadSnapshot {
                            channel_id: "C1".to_string(),
                            unread_state: SlackUnreadState::from_parts(true, true, 8),
                            last_read: None,
                            latest: Some("21.0".to_string()),
                            mention_count: Some(4),
                            is_open: None,
                        },
                    },
                ),
                &local_reads,
            )
            .unwrap();
        assert!(!cursorless.conversation_changed());
        assert!(cursorless.acknowledged_local_reads().is_empty());
        {
            let catalog = state.conversations.borrow();
            let current = catalog.get("C1").unwrap();
            assert_eq!(current.raw_unread_state().display_count, 0);
            assert_eq!(current.last_read_ts(), Some("20.0"));
        }

        let acknowledged = state
            .apply_workspace_patch_with_local_reads(
                &conversation_patch(
                    revision_five,
                    WorkspaceChange::UnreadChanged {
                        snapshot: SlackConversationUnreadSnapshot {
                            channel_id: "C1".to_string(),
                            unread_state: SlackUnreadState::from_parts(true, false, 0),
                            last_read: Some("20.0".to_string()),
                            ..Default::default()
                        },
                    },
                ),
                &local_reads,
            )
            .unwrap();
        assert!(
            !acknowledged.conversation_changed(),
            "a marker-only acknowledgement must not rebuild the sidebar"
        );
        assert_eq!(acknowledged.acknowledged_local_reads(), &["C1".to_string()]);
    }

    #[test]
    fn conversation_attention_patches_are_idempotent_and_local_read_safe() {
        let state = WorkspaceSessionState::default();
        let revision_one = WorkspaceRevision::INITIAL.successor();
        let revision_two = revision_one.successor();
        let revision_three = revision_two.successor();
        let revision_four = revision_three.successor();
        let revision_five = revision_four.successor();
        let mut initial = conversation("C1", "general");
        initial.is_starred = Some(true);
        initial.unread_count = Some(5);
        initial.extra.extend(HashMap::from([
            ("has_unreads".to_string(), serde_json::json!(true)),
            ("last_read".to_string(), serde_json::json!("10.0")),
            ("topic".to_string(), serde_json::json!("Keep me")),
        ]));
        state
            .apply_workspace_patch(&conversation_patch(
                revision_one,
                WorkspaceChange::BootstrapReset(WorkspaceBootstrapData {
                    conversations: vec![initial],
                    ..Default::default()
                }),
            ))
            .unwrap();

        let observation = ConversationAttentionObservation {
            message_ts: "11.0".to_string(),
            record_unread: true,
        };
        let first = state
            .apply_workspace_patch(&conversation_patch(
                revision_two,
                WorkspaceChange::ConversationAttentionObserved {
                    channel_id: "C1".to_string(),
                    observations: vec![observation.clone()],
                },
            ))
            .unwrap();
        assert!(first.conversation_changed());
        assert_eq!(first.unread_start("C1"), Some("11.0"));
        let duplicate = state
            .apply_workspace_patch(&conversation_patch(
                revision_three,
                WorkspaceChange::ConversationAttentionObserved {
                    channel_id: "C1".to_string(),
                    observations: vec![observation],
                },
            ))
            .unwrap();
        assert!(!duplicate.conversation_changed());
        assert_eq!(duplicate.unread_start("C1"), None);
        {
            let conversations = state.conversations.borrow();
            let current = conversations.get("C1").unwrap();
            assert_eq!(current.unread_activity_count(), 1);
            assert_eq!(current.raw_unread_activity_count(), 5);
            assert!(current.is_starred());
            assert_eq!(current.name.as_deref(), Some("general"));
            assert_eq!(
                current.extra.get("topic"),
                Some(&serde_json::json!("Keep me"))
            );
        }

        state
            .conversations
            .borrow_mut()
            .advance_read_cursor("C1", "20.0", 0);
        let local_reads = HashMap::from([("C1".to_string(), "20.0".to_string())]);
        let stale = state
            .apply_workspace_patch_with_local_reads(
                &conversation_patch(
                    revision_four,
                    WorkspaceChange::ConversationAttentionObserved {
                        channel_id: "C1".to_string(),
                        observations: vec![ConversationAttentionObservation {
                            message_ts: "19.0".to_string(),
                            record_unread: true,
                        }],
                    },
                ),
                &local_reads,
            )
            .unwrap();
        assert!(!stale.conversation_changed());
        {
            let conversations = state.conversations.borrow();
            let current = conversations.get("C1").unwrap();
            assert_eq!(current.unread_activity_count(), 0);
            assert_eq!(current.raw_unread_activity_count(), 0);
            assert_eq!(current.last_read_ts(), Some("20.0"));
            assert!(current.is_starred());
            assert_eq!(current.name.as_deref(), Some("general"));
            assert_eq!(
                current.extra.get("topic"),
                Some(&serde_json::json!("Keep me"))
            );
        }

        let newer = state
            .apply_workspace_patch_with_local_reads(
                &conversation_patch(
                    revision_five,
                    WorkspaceChange::ConversationAttentionObserved {
                        channel_id: "C1".to_string(),
                        observations: vec![ConversationAttentionObservation {
                            message_ts: "21.0".to_string(),
                            record_unread: true,
                        }],
                    },
                ),
                &local_reads,
            )
            .unwrap();
        assert!(newer.conversation_changed());
        assert_eq!(newer.unread_start("C1"), Some("21.0"));
        let conversations = state.conversations.borrow();
        let current = conversations.get("C1").unwrap();
        assert_eq!(current.unread_activity_count(), 1);
        assert_eq!(current.raw_unread_activity_count(), 0);
        assert_eq!(current.last_read_ts(), Some("20.0"));
        assert!(current.is_starred());
        assert_eq!(current.name.as_deref(), Some("general"));

        let embedded_marker_state = WorkspaceSessionState::default();
        let mut embedded_marker = conversation("C2", "embedded marker");
        embedded_marker.advance_read_cursor("30.0", 0);
        embedded_marker.set_local_read_ts("30.0");
        embedded_marker_state
            .apply_workspace_patch(&conversation_patch(
                revision_one,
                WorkspaceChange::BootstrapReset(WorkspaceBootstrapData {
                    conversations: vec![embedded_marker],
                    ..Default::default()
                }),
            ))
            .unwrap();
        let embedded_stale = embedded_marker_state
            .apply_workspace_patch(&conversation_patch(
                revision_two,
                WorkspaceChange::ConversationAttentionObserved {
                    channel_id: "C2".to_string(),
                    observations: vec![ConversationAttentionObservation {
                        message_ts: "29.0".to_string(),
                        record_unread: true,
                    }],
                },
            ))
            .unwrap();
        assert!(!embedded_stale.conversation_changed());
        let conversations = embedded_marker_state.conversations.borrow();
        let embedded_current = conversations.get("C2").unwrap();
        assert_eq!(embedded_current.unread_activity_count(), 0);
        assert_eq!(embedded_current.last_read_ts(), Some("30.0"));
        assert_eq!(embedded_current.local_read_ts(), Some("30.0"));
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
    fn workspace_patch_alone_hydrates_and_replaces_the_thread_catalog() {
        let session = WorkspaceSessionState::default();
        let mut catalog = ThreadCatalog::default();
        let mut root = message("1", "thread root");
        root.reply_count = Some(1);
        catalog.observe_history("C1", &[root]);
        let records = catalog.into_records();
        let revision_one = WorkspaceRevision::INITIAL.successor();

        let hydrated = session
            .apply_workspace_patch(&conversation_patch(
                revision_one,
                WorkspaceChange::BootstrapReset(WorkspaceBootstrapData {
                    threads: records.clone(),
                    ..Default::default()
                }),
            ))
            .unwrap();

        assert!(hydrated.thread_catalog_changed());
        assert_eq!(session.threads.borrow().clone().into_records(), records);

        let replaced = session
            .apply_workspace_patch(&conversation_patch(
                revision_one.successor(),
                WorkspaceChange::ThreadCatalogChanged(Vec::new()),
            ))
            .unwrap();

        assert!(replaced.thread_catalog_changed());
        assert!(session.threads.borrow().clone().into_records().is_empty());
    }

    #[test]
    fn workspace_patch_alone_hydrates_updates_and_clears_users() {
        let session = WorkspaceSessionState::default();
        let revision_one = WorkspaceRevision::INITIAL.successor();
        let user = SlackUser {
            id: Some("U1".into()),
            name: Some("old".into()),
            ..Default::default()
        };

        let hydrated = session
            .apply_workspace_patch(&conversation_patch(
                revision_one,
                WorkspaceChange::BootstrapReset(WorkspaceBootstrapData {
                    users: vec![user],
                    ..Default::default()
                }),
            ))
            .unwrap();
        assert!(hydrated.users_reset());
        assert_eq!(hydrated.changed_user_ids(), &["U1"]);

        let updated = session
            .apply_workspace_patch(&conversation_patch(
                revision_one.successor(),
                WorkspaceChange::UserUpsert(SlackUser {
                    id: Some("U1".into()),
                    name: Some("new".into()),
                    ..Default::default()
                }),
            ))
            .unwrap();
        assert!(!updated.users_reset());
        assert_eq!(updated.changed_user_ids(), &["U1"]);
        assert_eq!(
            session
                .users
                .borrow()
                .get("U1")
                .and_then(|user| user.name.as_deref()),
            Some("new")
        );

        let cleared = session
            .apply_workspace_patch(&conversation_patch(
                revision_one.successor().successor(),
                WorkspaceChange::UsersReset(Vec::new()),
            ))
            .unwrap();
        assert!(cleared.users_reset());
        assert!(cleared.changed_user_ids().is_empty());
        assert!(session.users.borrow().is_empty());
    }

    #[test]
    fn workspace_patch_projects_timelines_and_keeps_derived_surfaces_fresh() {
        let session = WorkspaceSessionState::default();
        {
            let mut view = session.view.borrow_mut();
            view.select_conversation("C1");
            view.apply_search_results(vec![SearchMatch {
                channel: Some(crate::models::SlackSearchChannel {
                    id: Some("C1".into()),
                    ..Default::default()
                }),
                ts: Some("1".into()),
                text: Some("old".into()),
                ..Default::default()
            }]);
            view.apply_saved(vec![SavedItem {
                channel: Some("C1".into()),
                message: Some(message("1", "old")),
                ..Default::default()
            }]);
        }
        let revision_one = WorkspaceRevision::INITIAL.successor();
        let changed = session
            .apply_workspace_patch(&conversation_patch(
                revision_one,
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Channel("C1".into()),
                    changes: vec![MessageChange::Upsert(Box::new(message("1", "new")))],
                },
            ))
            .unwrap();

        assert_eq!(changed.timeline_changes().len(), 1);
        assert_eq!(
            changed.timeline_changes()[0].target(),
            &TimelineTarget::Channel("C1".into())
        );
        assert!(changed.timeline_changes()[0].render());
        let view = session.view.borrow();
        assert_eq!(view.channel_messages("C1")[0].body_text(), "new");
        assert_eq!(view.search_results()[0].text.as_deref(), Some("new"));
        assert_eq!(
            view.saved_items()[0].message.as_ref().unwrap().body_text(),
            "new"
        );
        drop(view);

        let thread_changed = session
            .apply_workspace_patch(&conversation_patch(
                revision_one.successor(),
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Thread {
                        channel_id: "C1".into(),
                        thread_ts: "1".into(),
                    },
                    changes: vec![MessageChange::Upsert(Box::new(thread_message(
                        "2", "1", "reply",
                    )))],
                },
            ))
            .unwrap();
        assert!(!thread_changed.timeline_changes()[0].render());
        let mut view = session.view.borrow_mut();
        assert_eq!(
            view.open_thread("C1", "1"),
            ThreadOpenOutcome::RenderCachedAndRefresh
        );
        assert_eq!(view.current_thread_messages()[0].body_text(), "reply");
        drop(view);

        session
            .apply_workspace_patch(&conversation_patch(
                revision_one.successor().successor(),
                WorkspaceChange::TimelineChanged {
                    target: TimelineTarget::Channel("C1".into()),
                    changes: vec![MessageChange::Remove {
                        message_ts: "1".into(),
                    }],
                },
            ))
            .unwrap();
        let view = session.view.borrow();
        assert!(view.channel_messages("C1").is_empty());
        assert!(view.search_results().is_empty());
        assert!(view.saved_items().is_empty());
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
    fn conversation_open_sessions_increment_generation_and_reject_stale_commits() {
        let mut coordinator = ConversationOpenCoordinator::default();
        let first = coordinator.begin("C1", ConversationOpenIntent::Latest);
        let second = coordinator.begin("C2", ConversationOpenIntent::Latest);

        assert!(second > first);
        assert!(!coordinator.commit_position(first));
        assert_eq!(
            coordinator.resolve_position(second, "C2", &[message("1", "one")]),
            Some(ConversationOpenPosition::Latest)
        );
        assert!(coordinator.commit_position(second));
        assert_eq!(
            coordinator.active_phase(),
            Some(ConversationOpenPhase::Interactive)
        );
    }

    #[test]
    fn conversation_open_target_priority_is_explicit_then_unread_then_latest() {
        assert_eq!(
            ConversationOpenIntent::choose(Some("42"), true, Some("10"), 3,),
            ConversationOpenIntent::Message("42".to_string())
        );
        assert_eq!(
            ConversationOpenIntent::choose(None, true, Some("10"), 3),
            ConversationOpenIntent::FirstUnread {
                last_read: Some("10".to_string()),
                unread_count: 3,
            }
        );
        assert_eq!(
            ConversationOpenIntent::choose(None, false, Some("10"), 3),
            ConversationOpenIntent::Latest
        );
    }

    #[test]
    fn conversation_open_session_pins_first_resolved_unread_target() {
        let mut coordinator = ConversationOpenCoordinator::default();
        let generation = coordinator.begin(
            "C1",
            ConversationOpenIntent::FirstUnread {
                last_read: Some("2".to_string()),
                unread_count: 0,
            },
        );

        assert_eq!(
            coordinator.resolve_position(
                generation,
                "C1",
                &[message("4", "four"), message("3", "three")],
            ),
            Some(ConversationOpenPosition::Message("3".to_string()))
        );
        assert_eq!(
            coordinator.resolve_position(
                generation,
                "C1",
                &[
                    message("4", "four"),
                    message("2.5", "earlier unread"),
                    message("3", "three"),
                ],
            ),
            Some(ConversationOpenPosition::Message("3".to_string()))
        );
    }

    #[test]
    fn conversation_open_session_stops_automatic_positioning_after_user_interaction() {
        let mut coordinator = ConversationOpenCoordinator::default();
        let generation = coordinator.begin("C1", ConversationOpenIntent::Latest);
        assert_eq!(
            coordinator.resolve_position(generation, "C1", &[message("1", "one")]),
            Some(ConversationOpenPosition::Latest)
        );

        assert!(coordinator.note_user_interaction(generation));
        assert_eq!(
            coordinator.resolve_position(generation, "C1", &[message("2", "two")]),
            None
        );
        assert!(!coordinator.commit_position(generation));
    }

    #[test]
    fn conversation_open_session_holds_reconciliation_until_initial_commit() {
        let mut coordinator = ConversationOpenCoordinator::default();
        let generation = coordinator.begin("C1", ConversationOpenIntent::Latest);
        assert_eq!(
            coordinator.resolve_position(generation, "C1", &[message("1", "one")]),
            Some(ConversationOpenPosition::Latest)
        );

        assert_eq!(
            coordinator.note_render_requested(generation),
            Some(ConversationOpenRenderAction::InitialDocument)
        );
        assert_eq!(
            coordinator.note_render_requested(generation),
            Some(ConversationOpenRenderAction::HoldReconciliation)
        );
        assert!(coordinator.commit_position(generation));
        assert!(coordinator.take_pending_reconciliation(generation));
        assert!(!coordinator.take_pending_reconciliation(generation));
        assert_eq!(
            coordinator.note_render_requested(generation),
            Some(ConversationOpenRenderAction::Reconcile)
        );
    }

    #[test]
    fn first_unread_open_target_uses_count_when_cursor_is_missing() {
        let messages = vec![
            message("4", "four"),
            message("3", "three"),
            message("2", "two"),
            message("1", "one"),
        ];

        assert_eq!(
            resolve_first_unread_message_ts(&messages, None, 2).as_deref(),
            Some("3")
        );
        assert_eq!(
            resolve_first_unread_message_ts(&messages, None, 99).as_deref(),
            Some("1")
        );
    }

    #[test]
    fn conversation_open_session_waits_for_an_explicit_target_and_rejects_other_channels() {
        let mut coordinator = ConversationOpenCoordinator::default();
        let generation =
            coordinator.begin("C1", ConversationOpenIntent::Message("target".to_string()));

        assert_eq!(
            coordinator.resolve_position(generation, "C2", &[message("target", "wrong")]),
            None
        );
        assert_eq!(
            coordinator.resolve_position(generation, "C1", &[message("other", "cached")]),
            None
        );
        assert_eq!(
            coordinator.resolve_position(generation, "C1", &[message("target", "context")]),
            Some(ConversationOpenPosition::Message("target".to_string()))
        );
    }

    #[test]
    fn first_unread_open_target_falls_back_to_count_when_cursor_is_outside_loaded_history() {
        let messages = vec![
            message("4", "four"),
            message("3", "three"),
            message("2", "two"),
            message("1", "one"),
        ];

        assert_eq!(
            resolve_first_unread_message_ts(&messages, Some("9"), 2).as_deref(),
            Some("3")
        );
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
        assert_eq!(
            state
                .channel_messages("C1")
                .iter()
                .find(|message| message.ts == "2")
                .unwrap()
                .body_text(),
            "duplicate",
            "canonical append entries must replace duplicate compatibility state"
        );
        assert!(state.begin_history_request("C1"));
        assert_eq!(
            outcome.scroll,
            Some(WorkspaceScrollBehavior::PreservePrepend)
        );
        assert!(!outcome.notify_new_messages);
    }

    #[test]
    fn canonical_history_refresh_removes_absent_stale_entries() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(
            &mut state,
            "C1",
            vec![
                message("3", "stale edit"),
                message("2", "deleted"),
                message("1", "old"),
            ],
        );

        state.apply_history(
            "C1",
            vec![
                message("4", "concurrent post"),
                message("3", "canonical edit"),
            ],
            true,
            Some("older".into()),
            false,
            false,
        );

        assert_eq!(
            state
                .channel_messages("C1")
                .iter()
                .map(|message| (message.ts.as_str(), message.body_text()))
                .collect::<Vec<_>>(),
            vec![
                ("4", "concurrent post".into()),
                ("3", "canonical edit".into())
            ]
        );
        assert_eq!(state.channel_cursor("C1"), Some("older"));
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
    fn channel_projection_remains_cached_while_another_main_view_is_visible() {
        let mut state = WorkspaceViewState::default();
        state.select_conversation("C1");
        apply_fresh(&mut state, "C1", vec![message("1", "one")]);
        state.show_unreads();

        assert_eq!(state.last_channel_id(), Some("C1"));
        assert_eq!(state.channel_messages("C1")[0].body_text(), "one");
        assert_eq!(state.main_view(), MainMessageView::Unreads);
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
        assert_eq!(state.channel_tail_messages("C1")[0].body_text(), "latest");

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
}
