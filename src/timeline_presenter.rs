/* timeline_presenter.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Pure lifecycle and batching state for one timeline WebView.
//!
//! The presenter deliberately does not depend on GTK or WebKit. A window owns one
//! presenter per timeline surface and translates the returned actions into document
//! loads, frame callbacks, and JavaScript evaluations.

use serde::Serialize;

use crate::message_html::TimelineDomPatch;
use crate::workspace_pipeline::{TimelineTarget, WorkspaceRevision};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct TimelineDocumentGeneration(u64);

impl TimelineDocumentGeneration {
    pub(crate) fn value(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
/// Presenter-local DOM revision.
///
/// This is intentionally distinct from `WorkspaceRevision`: compatibility
/// events and presentation enrichments can legitimately produce multiple DOM
/// changes at the same workspace revision.
pub(crate) struct TimelineRevision(u64);

impl TimelineRevision {
    pub(crate) fn from_value(value: u64) -> Option<Self> {
        (value > 0).then_some(Self(value))
    }

    pub(crate) fn value(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct TimelineDeltaId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimelineDocumentLoadReason {
    InitialNavigation,
    RevisionMismatch,
    Corruption,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimelineDocumentRequest {
    generation: TimelineDocumentGeneration,
    revision: TimelineRevision,
    target: TimelineTarget,
    source_workspace_revision: Option<WorkspaceRevision>,
    reason: TimelineDocumentLoadReason,
}

impl TimelineDocumentRequest {
    pub(crate) fn generation(&self) -> TimelineDocumentGeneration {
        self.generation
    }

    pub(crate) fn revision(&self) -> TimelineRevision {
        self.revision
    }

    pub(crate) fn target(&self) -> &TimelineTarget {
        &self.target
    }

    pub(crate) fn source_workspace_revision(&self) -> Option<WorkspaceRevision> {
        self.source_workspace_revision
    }

    pub(crate) fn reason(&self) -> TimelineDocumentLoadReason {
        self.reason
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TimelinePresenterAction {
    LoadDocument(TimelineDocumentRequest),
    ScheduleFrame {
        generation: TimelineDocumentGeneration,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TimelineDelta {
    id: TimelineDeltaId,
    document_generation: TimelineDocumentGeneration,
    #[serde(rename = "base_timeline_revision")]
    base_revision: TimelineRevision,
    #[serde(rename = "timeline_revision")]
    revision: TimelineRevision,
    operations: Vec<TimelineDomPatch>,
    #[serde(skip)]
    target: TimelineTarget,
    #[serde(skip)]
    source_workspace_revision: Option<WorkspaceRevision>,
}

impl TimelineDelta {
    pub(crate) fn id(&self) -> TimelineDeltaId {
        self.id
    }

    pub(crate) fn document_generation(&self) -> TimelineDocumentGeneration {
        self.document_generation
    }

    pub(crate) fn base_revision(&self) -> TimelineRevision {
        self.base_revision
    }

    pub(crate) fn revision(&self) -> TimelineRevision {
        self.revision
    }

    pub(crate) fn operations(&self) -> &[TimelineDomPatch] {
        &self.operations
    }

    pub(crate) fn source_workspace_revision(&self) -> Option<WorkspaceRevision> {
        self.source_workspace_revision
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimelineDeltaApplyResult {
    Applied { revision: TimelineRevision },
    RevisionMismatch { actual: Option<TimelineRevision> },
    Corrupt,
}

impl TimelineDeltaApplyResult {
    /// Translate the small result object returned by the timeline JavaScript.
    pub(crate) fn from_status(status: &str, timeline_revision: Option<u64>) -> Option<Self> {
        match status {
            "applied" => Some(Self::Applied {
                revision: TimelineRevision::from_value(timeline_revision?)?,
            }),
            "revision-mismatch" => Some(Self::RevisionMismatch {
                actual: timeline_revision.and_then(TimelineRevision::from_value),
            }),
            "corrupt" => Some(Self::Corrupt),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TimelinePresenterCounters {
    pub(crate) document_loads: u64,
    pub(crate) initial_document_loads: u64,
    pub(crate) revision_mismatch_loads: u64,
    pub(crate) corruption_loads: u64,
    pub(crate) documents_ready: u64,
    pub(crate) frame_schedules: u64,
    pub(crate) deltas: u64,
    pub(crate) delta_operations: u64,
    pub(crate) applied_deltas: u64,
    pub(crate) queued_operations: u64,
    pub(crate) queued_while_loading: u64,
    pub(crate) queued_while_in_flight: u64,
    pub(crate) ignored_operations: u64,
    pub(crate) revision_mismatches: u64,
    pub(crate) corruptions: u64,
    pub(crate) stale_callbacks: u64,
    pub(crate) pending_operations: u64,
    pub(crate) peak_pending_operations: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum TimelinePresenterState {
    #[default]
    Detached,
    Loading,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DocumentPhase {
    Loading,
    Ready,
}

#[derive(Debug, Clone)]
struct ActiveDocument {
    generation: TimelineDocumentGeneration,
    revision: TimelineRevision,
    target: TimelineTarget,
    source_workspace_revision: Option<WorkspaceRevision>,
    phase: DocumentPhase,
}

#[derive(Debug, Clone, Copy)]
struct InFlightDelta {
    id: TimelineDeltaId,
    generation: TimelineDocumentGeneration,
    revision: TimelineRevision,
}

#[derive(Debug, Default)]
pub(crate) struct TimelinePresenter {
    next_generation: u64,
    next_revision: u64,
    next_delta_id: u64,
    active: Option<ActiveDocument>,
    pending: Vec<TimelineDomPatch>,
    frame_scheduled: bool,
    in_flight: Option<InFlightDelta>,
    counters: TimelinePresenterCounters,
}

impl TimelinePresenter {
    pub(crate) fn navigate(
        &mut self,
        target: TimelineTarget,
        source_workspace_revision: Option<WorkspaceRevision>,
    ) -> Vec<TimelinePresenterAction> {
        self.begin_document(
            target,
            source_workspace_revision,
            TimelineDocumentLoadReason::InitialNavigation,
        )
    }

    pub(crate) fn detach(&mut self) {
        self.active = None;
        self.pending.clear();
        self.frame_scheduled = false;
        self.in_flight = None;
        self.counters.pending_operations = 0;
    }

    pub(crate) fn queue_operations(
        &mut self,
        target: &TimelineTarget,
        source_workspace_revision: Option<WorkspaceRevision>,
        operations: impl IntoIterator<Item = TimelineDomPatch>,
    ) -> Vec<TimelinePresenterAction> {
        let operations = operations.into_iter().collect::<Vec<_>>();
        let operation_count = operations.len() as u64;
        let Some(active) = self.active.as_mut() else {
            self.counters.ignored_operations = self
                .counters
                .ignored_operations
                .saturating_add(operation_count);
            return Vec::new();
        };
        if &active.target != target {
            self.counters.ignored_operations = self
                .counters
                .ignored_operations
                .saturating_add(operation_count);
            return Vec::new();
        }

        active.source_workspace_revision =
            latest_workspace_revision(active.source_workspace_revision, source_workspace_revision);
        if operations.is_empty() {
            return Vec::new();
        }

        self.counters.queued_operations = self
            .counters
            .queued_operations
            .saturating_add(operation_count);
        match active.phase {
            DocumentPhase::Loading => {
                self.counters.queued_while_loading = self
                    .counters
                    .queued_while_loading
                    .saturating_add(operation_count);
            }
            DocumentPhase::Ready if self.in_flight.is_some() => {
                self.counters.queued_while_in_flight = self
                    .counters
                    .queued_while_in_flight
                    .saturating_add(operation_count);
            }
            DocumentPhase::Ready => {}
        }
        self.pending.extend(operations);
        self.record_pending_depth();
        self.schedule_frame_if_ready()
    }

    pub(crate) fn document_ready(
        &mut self,
        generation: TimelineDocumentGeneration,
        revision: TimelineRevision,
    ) -> Vec<TimelinePresenterAction> {
        let Some(active) = self.active.as_ref() else {
            self.counters.stale_callbacks = self.counters.stale_callbacks.saturating_add(1);
            return Vec::new();
        };
        if active.generation != generation || active.phase != DocumentPhase::Loading {
            self.counters.stale_callbacks = self.counters.stale_callbacks.saturating_add(1);
            return Vec::new();
        }
        if active.revision != revision {
            self.counters.revision_mismatches = self.counters.revision_mismatches.saturating_add(1);
            return self.recover_document(TimelineDocumentLoadReason::RevisionMismatch);
        }

        self.active
            .as_mut()
            .expect("the matching timeline document disappeared")
            .phase = DocumentPhase::Ready;
        self.counters.documents_ready = self.counters.documents_ready.saturating_add(1);
        self.schedule_frame_if_ready()
    }

    pub(crate) fn document_failed(
        &mut self,
        generation: TimelineDocumentGeneration,
    ) -> Vec<TimelinePresenterAction> {
        let Some(active) = self.active.as_ref() else {
            self.counters.stale_callbacks = self.counters.stale_callbacks.saturating_add(1);
            return Vec::new();
        };
        if active.generation != generation {
            self.counters.stale_callbacks = self.counters.stale_callbacks.saturating_add(1);
            return Vec::new();
        }

        self.counters.corruptions = self.counters.corruptions.saturating_add(1);
        self.recover_document(TimelineDocumentLoadReason::Corruption)
    }

    pub(crate) fn take_frame(
        &mut self,
        generation: TimelineDocumentGeneration,
    ) -> Option<TimelineDelta> {
        let Some(active) = self.active.as_ref() else {
            self.counters.stale_callbacks = self.counters.stale_callbacks.saturating_add(1);
            return None;
        };
        if active.generation != generation {
            self.counters.stale_callbacks = self.counters.stale_callbacks.saturating_add(1);
            return None;
        }
        if active.phase != DocumentPhase::Ready || !self.frame_scheduled || self.in_flight.is_some()
        {
            return None;
        }
        if self.pending.is_empty() {
            self.frame_scheduled = false;
            return None;
        }

        let base_revision = active.revision;
        let target = active.target.clone();
        let source_workspace_revision = active.source_workspace_revision;
        let revision = self.allocate_revision();
        let id = self.allocate_delta_id();
        let operations = std::mem::take(&mut self.pending);
        self.frame_scheduled = false;
        self.counters.pending_operations = 0;
        self.counters.deltas = self.counters.deltas.saturating_add(1);
        self.counters.delta_operations = self
            .counters
            .delta_operations
            .saturating_add(operations.len() as u64);
        self.in_flight = Some(InFlightDelta {
            id,
            generation,
            revision,
        });

        Some(TimelineDelta {
            id,
            document_generation: generation,
            base_revision,
            revision,
            operations,
            target,
            source_workspace_revision,
        })
    }

    pub(crate) fn acknowledge(
        &mut self,
        generation: TimelineDocumentGeneration,
        id: TimelineDeltaId,
        result: TimelineDeltaApplyResult,
    ) -> Vec<TimelinePresenterAction> {
        let Some(in_flight) = self.in_flight else {
            self.counters.stale_callbacks = self.counters.stale_callbacks.saturating_add(1);
            return Vec::new();
        };
        if in_flight.generation != generation || in_flight.id != id {
            self.counters.stale_callbacks = self.counters.stale_callbacks.saturating_add(1);
            return Vec::new();
        }
        if !self.active.as_ref().is_some_and(|active| {
            active.generation == generation && active.phase == DocumentPhase::Ready
        }) {
            self.counters.stale_callbacks = self.counters.stale_callbacks.saturating_add(1);
            return Vec::new();
        }

        self.in_flight = None;
        match result {
            TimelineDeltaApplyResult::Applied { revision } if revision == in_flight.revision => {
                self.active
                    .as_mut()
                    .expect("the acknowledged timeline document disappeared")
                    .revision = revision;
                self.counters.applied_deltas = self.counters.applied_deltas.saturating_add(1);
                self.schedule_frame_if_ready()
            }
            TimelineDeltaApplyResult::Applied { .. }
            | TimelineDeltaApplyResult::RevisionMismatch { .. } => {
                self.counters.revision_mismatches =
                    self.counters.revision_mismatches.saturating_add(1);
                self.recover_document(TimelineDocumentLoadReason::RevisionMismatch)
            }
            TimelineDeltaApplyResult::Corrupt => {
                self.counters.corruptions = self.counters.corruptions.saturating_add(1);
                self.recover_document(TimelineDocumentLoadReason::Corruption)
            }
        }
    }

    pub(crate) fn state(&self) -> TimelinePresenterState {
        match self.active.as_ref().map(|active| active.phase) {
            None => TimelinePresenterState::Detached,
            Some(DocumentPhase::Loading) => TimelinePresenterState::Loading,
            Some(DocumentPhase::Ready) => TimelinePresenterState::Ready,
        }
    }

    pub(crate) fn active_target(&self) -> Option<&TimelineTarget> {
        self.active.as_ref().map(|active| &active.target)
    }

    pub(crate) fn active_generation(&self) -> Option<TimelineDocumentGeneration> {
        self.active.as_ref().map(|active| active.generation)
    }

    pub(crate) fn active_revision(&self) -> Option<TimelineRevision> {
        self.active.as_ref().map(|active| active.revision)
    }

    #[cfg(test)]
    pub(crate) fn pending_operations(&self) -> usize {
        self.pending.len()
    }

    #[cfg(test)]
    pub(crate) fn has_in_flight_delta(&self) -> bool {
        self.in_flight.is_some()
    }

    pub(crate) fn counters(&self) -> TimelinePresenterCounters {
        self.counters
    }

    fn begin_document(
        &mut self,
        target: TimelineTarget,
        source_workspace_revision: Option<WorkspaceRevision>,
        reason: TimelineDocumentLoadReason,
    ) -> Vec<TimelinePresenterAction> {
        let generation = self.allocate_generation();
        let revision = self.allocate_revision();
        let request = TimelineDocumentRequest {
            generation,
            revision,
            target: target.clone(),
            source_workspace_revision,
            reason,
        };
        self.active = Some(ActiveDocument {
            generation,
            revision,
            target,
            source_workspace_revision,
            phase: DocumentPhase::Loading,
        });
        self.pending.clear();
        self.frame_scheduled = false;
        self.in_flight = None;
        self.counters.pending_operations = 0;
        self.counters.document_loads = self.counters.document_loads.saturating_add(1);
        match reason {
            TimelineDocumentLoadReason::InitialNavigation => {
                self.counters.initial_document_loads =
                    self.counters.initial_document_loads.saturating_add(1);
            }
            TimelineDocumentLoadReason::RevisionMismatch => {
                self.counters.revision_mismatch_loads =
                    self.counters.revision_mismatch_loads.saturating_add(1);
            }
            TimelineDocumentLoadReason::Corruption => {
                self.counters.corruption_loads = self.counters.corruption_loads.saturating_add(1);
            }
        }
        vec![TimelinePresenterAction::LoadDocument(request)]
    }

    fn recover_document(
        &mut self,
        reason: TimelineDocumentLoadReason,
    ) -> Vec<TimelinePresenterAction> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        let target = active.target.clone();
        let source_workspace_revision = active.source_workspace_revision;
        self.begin_document(target, source_workspace_revision, reason)
    }

    fn schedule_frame_if_ready(&mut self) -> Vec<TimelinePresenterAction> {
        let Some(active) = self.active.as_ref() else {
            return Vec::new();
        };
        if active.phase != DocumentPhase::Ready
            || self.pending.is_empty()
            || self.frame_scheduled
            || self.in_flight.is_some()
        {
            return Vec::new();
        }
        let generation = active.generation;
        self.frame_scheduled = true;
        self.counters.frame_schedules = self.counters.frame_schedules.saturating_add(1);
        vec![TimelinePresenterAction::ScheduleFrame { generation }]
    }

    fn record_pending_depth(&mut self) {
        let depth = self.pending.len() as u64;
        self.counters.pending_operations = depth;
        self.counters.peak_pending_operations = self.counters.peak_pending_operations.max(depth);
    }

    fn allocate_generation(&mut self) -> TimelineDocumentGeneration {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("timeline document generation space exhausted");
        TimelineDocumentGeneration(self.next_generation)
    }

    fn allocate_revision(&mut self) -> TimelineRevision {
        self.next_revision = self
            .next_revision
            .checked_add(1)
            .expect("timeline revision space exhausted");
        TimelineRevision(self.next_revision)
    }

    fn allocate_delta_id(&mut self) -> TimelineDeltaId {
        self.next_delta_id = self
            .next_delta_id
            .checked_add(1)
            .expect("timeline delta identity space exhausted");
        TimelineDeltaId(self.next_delta_id)
    }
}

fn latest_workspace_revision(
    current: Option<WorkspaceRevision>,
    incoming: Option<WorkspaceRevision>,
) -> Option<WorkspaceRevision> {
    match (current, incoming) {
        (Some(current), Some(incoming)) => Some(current.max(incoming)),
        (Some(current), None) => Some(current),
        (None, Some(incoming)) => Some(incoming),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_html::{TimelineInsertPosition, TimelineMessageArrival};

    fn workspace_revision(value: u64) -> WorkspaceRevision {
        let mut revision = WorkspaceRevision::INITIAL;
        for _ in 0..value {
            revision = revision.successor();
        }
        revision
    }

    fn channel(id: &str) -> TimelineTarget {
        TimelineTarget::Channel(id.to_string())
    }

    fn insert(message_ts: &str) -> TimelineDomPatch {
        TimelineDomPatch::InsertMessage {
            position: TimelineInsertPosition::Append,
            message_ts: message_ts.to_string(),
            arrival: Some(TimelineMessageArrival::Sent),
            html: format!("<li data-test=\"{message_ts}\"></li>"),
        }
    }

    fn edit(message_ts: &str) -> TimelineDomPatch {
        TimelineDomPatch::ReplaceMessage {
            message_ts: message_ts.to_string(),
            arrival: None,
            html: "<article>edited</article>".to_string(),
            part_html: "<div>edited</div>".to_string(),
        }
    }

    fn delete(message_ts: &str) -> TimelineDomPatch {
        TimelineDomPatch::RemoveMessage {
            message_ts: message_ts.to_string(),
        }
    }

    fn enrichment(asset_key: &str) -> TimelineDomPatch {
        TimelineDomPatch::UpdateImage {
            asset_key: asset_key.to_string(),
            source: Some("data:image/png;base64,c2FmZQ==".to_string()),
        }
    }

    fn load_action(actions: &[TimelinePresenterAction]) -> &TimelineDocumentRequest {
        let [TimelinePresenterAction::LoadDocument(request)] = actions else {
            panic!("expected one document load action, got {actions:?}");
        };
        request
    }

    fn schedule_generation(actions: &[TimelinePresenterAction]) -> TimelineDocumentGeneration {
        let [TimelinePresenterAction::ScheduleFrame { generation }] = actions else {
            panic!("expected one frame action, got {actions:?}");
        };
        *generation
    }

    fn ready_presenter(target: TimelineTarget) -> (TimelinePresenter, TimelineDocumentRequest) {
        let mut presenter = TimelinePresenter::default();
        let request = load_action(&presenter.navigate(target, Some(workspace_revision(4)))).clone();
        assert!(presenter
            .document_ready(request.generation(), request.revision())
            .is_empty());
        (presenter, request)
    }

    #[test]
    fn initial_navigation_allocates_one_revisioned_document_load() {
        let mut presenter = TimelinePresenter::default();

        let actions = presenter.navigate(channel("C1"), Some(workspace_revision(7)));
        let request = load_action(&actions);

        assert_eq!(request.generation().value(), 1);
        assert_eq!(request.revision().value(), 1);
        assert_eq!(request.target(), &channel("C1"));
        assert_eq!(
            request.source_workspace_revision(),
            Some(workspace_revision(7))
        );
        assert_eq!(
            request.reason(),
            TimelineDocumentLoadReason::InitialNavigation
        );
        assert_eq!(presenter.state(), TimelinePresenterState::Loading);
        assert_eq!(presenter.counters().document_loads, 1);
        assert_eq!(presenter.counters().initial_document_loads, 1);
    }

    #[test]
    fn superseded_loading_completion_and_old_target_changes_are_ignored() {
        let mut presenter = TimelinePresenter::default();
        let first = load_action(&presenter.navigate(channel("C1"), None)).clone();
        let second = load_action(&presenter.navigate(channel("C2"), None)).clone();

        assert!(presenter
            .document_ready(first.generation(), first.revision())
            .is_empty());
        assert!(presenter
            .queue_operations(&channel("C1"), None, [insert("1")])
            .is_empty());
        assert_eq!(presenter.pending_operations(), 0);
        assert_eq!(presenter.counters().ignored_operations, 1);
        assert_eq!(presenter.active_target(), Some(&channel("C2")));

        let ready = presenter.document_ready(second.generation(), second.revision());
        assert!(ready.is_empty());
        assert_eq!(presenter.state(), TimelinePresenterState::Ready);
        assert_eq!(presenter.counters().stale_callbacks, 1);
    }

    #[test]
    fn loading_queue_flushes_once_after_matching_document_ready() {
        let mut presenter = TimelinePresenter::default();
        let request =
            load_action(&presenter.navigate(channel("C1"), Some(workspace_revision(4)))).clone();
        let operations = vec![insert("1"), edit("2"), delete("3"), enrichment("asset")];

        assert!(presenter
            .queue_operations(
                &channel("C1"),
                Some(workspace_revision(5)),
                operations.clone(),
            )
            .is_empty());
        assert_eq!(presenter.pending_operations(), 4);
        assert_eq!(presenter.counters().queued_while_loading, 4);

        let actions = presenter.document_ready(request.generation(), request.revision());
        assert_eq!(schedule_generation(&actions), request.generation());
        assert!(presenter
            .document_ready(request.generation(), request.revision())
            .is_empty());

        let delta = presenter.take_frame(request.generation()).unwrap();
        assert_eq!(delta.base_revision(), request.revision());
        assert_eq!(delta.revision().value(), 2);
        assert_eq!(delta.operations(), operations);
        assert_eq!(
            delta.source_workspace_revision(),
            Some(workspace_revision(5))
        );
        assert_eq!(presenter.counters().frame_schedules, 1);
        assert_eq!(presenter.counters().deltas, 1);
        assert_eq!(presenter.counters().delta_operations, 4);
    }

    #[test]
    fn ready_changes_schedule_only_one_frame_and_preserve_order() {
        let (mut presenter, request) = ready_presenter(channel("C1"));
        let first = presenter.queue_operations(&channel("C1"), None, [insert("1")]);
        assert_eq!(schedule_generation(&first), request.generation());
        assert!(presenter
            .queue_operations(&channel("C1"), None, [edit("1")])
            .is_empty());
        assert!(presenter
            .queue_operations(&channel("C1"), None, [delete("2"), enrichment("asset")])
            .is_empty());

        let delta = presenter.take_frame(request.generation()).unwrap();
        assert_eq!(
            delta.operations(),
            [insert("1"), edit("1"), delete("2"), enrichment("asset")]
        );
        assert!(presenter.take_frame(request.generation()).is_none());
        assert_eq!(presenter.counters().frame_schedules, 1);
    }

    #[test]
    fn one_hundred_changes_before_the_frame_form_one_delta() {
        let (mut presenter, request) = ready_presenter(channel("C1"));

        for index in 0..100 {
            let actions = presenter.queue_operations(
                &channel("C1"),
                Some(workspace_revision(8)),
                [insert(&index.to_string())],
            );
            if index == 0 {
                assert_eq!(schedule_generation(&actions), request.generation());
            } else {
                assert!(actions.is_empty());
            }
        }

        let delta = presenter.take_frame(request.generation()).unwrap();
        assert_eq!(delta.operations().len(), 100);
        assert_eq!(presenter.counters().frame_schedules, 1);
        assert_eq!(presenter.counters().deltas, 1);
        assert_eq!(presenter.counters().delta_operations, 100);
    }

    #[test]
    fn same_workspace_revision_structural_and_enrichment_changes_are_accepted() {
        let (mut presenter, request) = ready_presenter(channel("C1"));
        let source = Some(workspace_revision(9));

        assert_eq!(
            schedule_generation(
                &presenter.queue_operations(&channel("C1"), source, [insert("1")],)
            ),
            request.generation()
        );
        assert!(presenter
            .queue_operations(&channel("C1"), source, [edit("1"), enrichment("asset")])
            .is_empty());

        let delta = presenter.take_frame(request.generation()).unwrap();
        assert_eq!(
            delta.operations(),
            [insert("1"), edit("1"), enrichment("asset")]
        );
        assert_eq!(delta.source_workspace_revision(), source);
    }

    #[test]
    fn an_in_flight_delta_serializes_the_next_frame() {
        let (mut presenter, request) = ready_presenter(channel("C1"));
        presenter.queue_operations(&channel("C1"), None, [insert("1")]);
        let first = presenter.take_frame(request.generation()).unwrap();

        assert!(presenter
            .queue_operations(&channel("C1"), None, [edit("1")])
            .is_empty());
        assert!(presenter.take_frame(request.generation()).is_none());
        assert_eq!(presenter.counters().queued_while_in_flight, 1);

        let actions = presenter.acknowledge(
            first.document_generation(),
            first.id(),
            TimelineDeltaApplyResult::Applied {
                revision: first.revision(),
            },
        );
        assert_eq!(schedule_generation(&actions), request.generation());
        let second = presenter.take_frame(request.generation()).unwrap();
        assert_eq!(second.base_revision(), first.revision());
        assert_eq!(second.operations(), [edit("1")]);
    }

    #[test]
    fn document_ready_revision_mismatch_requests_canonical_reload() {
        let mut presenter = TimelinePresenter::default();
        let request =
            load_action(&presenter.navigate(channel("C1"), Some(workspace_revision(3)))).clone();
        presenter.queue_operations(&channel("C1"), Some(workspace_revision(5)), [insert("1")]);

        let actions = presenter.document_ready(
            request.generation(),
            TimelineRevision(request.revision().value() + 10),
        );
        let recovery = load_action(&actions);

        assert_eq!(
            recovery.reason(),
            TimelineDocumentLoadReason::RevisionMismatch
        );
        assert!(recovery.generation() > request.generation());
        assert!(recovery.revision() > request.revision());
        assert_eq!(
            recovery.source_workspace_revision(),
            Some(workspace_revision(5))
        );
        assert_eq!(presenter.pending_operations(), 0);
        assert_eq!(presenter.counters().revision_mismatches, 1);
        assert_eq!(presenter.counters().revision_mismatch_loads, 1);
    }

    #[test]
    fn matching_document_failure_requests_one_corruption_reload() {
        let mut presenter = TimelinePresenter::default();
        let request = load_action(&presenter.navigate(channel("C1"), None)).clone();

        let actions = presenter.document_failed(request.generation());
        let recovery = load_action(&actions);

        assert_eq!(recovery.reason(), TimelineDocumentLoadReason::Corruption);
        assert!(recovery.generation() > request.generation());
        assert_eq!(presenter.counters().corruptions, 1);
        assert_eq!(presenter.counters().corruption_loads, 1);
        assert!(presenter.document_failed(request.generation()).is_empty());
        assert_eq!(presenter.counters().corruption_loads, 1);
        assert_eq!(presenter.counters().stale_callbacks, 1);
    }

    #[test]
    fn delta_revision_mismatch_requests_canonical_reload() {
        let (mut presenter, request) = ready_presenter(channel("C1"));
        presenter.queue_operations(&channel("C1"), Some(workspace_revision(8)), [insert("1")]);
        let delta = presenter.take_frame(request.generation()).unwrap();

        let actions = presenter.acknowledge(
            delta.document_generation(),
            delta.id(),
            TimelineDeltaApplyResult::Applied {
                revision: delta.base_revision(),
            },
        );
        let recovery = load_action(&actions);

        assert_eq!(
            recovery.reason(),
            TimelineDocumentLoadReason::RevisionMismatch
        );
        assert_eq!(
            recovery.source_workspace_revision(),
            Some(workspace_revision(8))
        );
        assert_eq!(presenter.counters().applied_deltas, 0);
        assert_eq!(presenter.counters().revision_mismatches, 1);
    }

    #[test]
    fn explicit_revision_mismatch_requests_canonical_reload() {
        let (mut presenter, request) = ready_presenter(channel("C1"));
        presenter.queue_operations(&channel("C1"), None, [insert("1")]);
        let delta = presenter.take_frame(request.generation()).unwrap();

        let actions = presenter.acknowledge(
            delta.document_generation(),
            delta.id(),
            TimelineDeltaApplyResult::RevisionMismatch {
                actual: Some(delta.base_revision()),
            },
        );

        assert_eq!(
            load_action(&actions).reason(),
            TimelineDocumentLoadReason::RevisionMismatch
        );
        assert_eq!(presenter.counters().revision_mismatches, 1);
    }

    #[test]
    fn corrupt_delta_requests_canonical_reload() {
        let (mut presenter, request) = ready_presenter(channel("C1"));
        presenter.queue_operations(&channel("C1"), None, [delete("missing")]);
        let delta = presenter.take_frame(request.generation()).unwrap();

        let actions = presenter.acknowledge(
            delta.document_generation(),
            delta.id(),
            TimelineDeltaApplyResult::Corrupt,
        );
        let recovery = load_action(&actions);

        assert_eq!(recovery.reason(), TimelineDocumentLoadReason::Corruption);
        assert_eq!(presenter.counters().corruptions, 1);
        assert_eq!(presenter.counters().corruption_loads, 1);
        assert!(!presenter.has_in_flight_delta());
    }

    #[test]
    fn stale_delta_ack_after_navigation_is_ignored() {
        let (mut presenter, first_request) = ready_presenter(channel("C1"));
        presenter.queue_operations(&channel("C1"), None, [insert("1")]);
        let delta = presenter.take_frame(first_request.generation()).unwrap();
        let second = load_action(&presenter.navigate(channel("C2"), None)).clone();

        assert!(presenter
            .acknowledge(
                delta.document_generation(),
                delta.id(),
                TimelineDeltaApplyResult::Applied {
                    revision: delta.revision(),
                },
            )
            .is_empty());
        assert_eq!(presenter.active_target(), Some(second.target()));
        assert_eq!(presenter.state(), TimelinePresenterState::Loading);
        assert_eq!(presenter.counters().stale_callbacks, 1);
    }

    #[test]
    fn no_op_idle_does_not_increment_load_or_delta_counters() {
        let (mut presenter, request) = ready_presenter(channel("C1"));
        let baseline = presenter.counters();

        assert!(presenter
            .queue_operations(&channel("C1"), Some(workspace_revision(12)), [])
            .is_empty());
        assert!(presenter.take_frame(request.generation()).is_none());

        let current = presenter.counters();
        assert_eq!(current.document_loads, baseline.document_loads);
        assert_eq!(current.deltas, baseline.deltas);
        assert_eq!(current.delta_operations, baseline.delta_operations);
        assert_eq!(current.frame_schedules, baseline.frame_schedules);
    }

    #[test]
    fn delta_serializes_the_revisioned_javascript_handshake() {
        let (mut presenter, request) = ready_presenter(channel("C1"));
        presenter.queue_operations(&channel("C1"), None, [insert("1")]);
        let delta = presenter.take_frame(request.generation()).unwrap();

        let value = serde_json::to_value(&delta).unwrap();
        let fields = value.as_object().unwrap();
        assert_eq!(fields.len(), 5);
        assert_eq!(fields["id"], 1);
        assert_eq!(fields["document_generation"], 1);
        assert_eq!(fields["base_timeline_revision"], 1);
        assert_eq!(fields["timeline_revision"], 2);
        assert_eq!(fields["operations"].as_array().unwrap().len(), 1);
        assert!(!fields.contains_key("target"));
        assert!(!fields.contains_key("source_workspace_revision"));
    }

    #[test]
    fn javascript_result_statuses_require_valid_revisions() {
        assert_eq!(
            TimelineDeltaApplyResult::from_status("applied", Some(3)),
            Some(TimelineDeltaApplyResult::Applied {
                revision: TimelineRevision(3),
            })
        );
        assert_eq!(
            TimelineDeltaApplyResult::from_status("revision-mismatch", None),
            Some(TimelineDeltaApplyResult::RevisionMismatch { actual: None })
        );
        assert_eq!(
            TimelineDeltaApplyResult::from_status("revision-mismatch", Some(2)),
            Some(TimelineDeltaApplyResult::RevisionMismatch {
                actual: Some(TimelineRevision(2)),
            })
        );
        assert_eq!(
            TimelineDeltaApplyResult::from_status("corrupt", None),
            Some(TimelineDeltaApplyResult::Corrupt)
        );
        assert_eq!(
            TimelineDeltaApplyResult::from_status("applied", Some(0)),
            None
        );
        assert_eq!(
            TimelineDeltaApplyResult::from_status("unknown", Some(1)),
            None
        );
    }
}
