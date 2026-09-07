use crate::message_html::{TimelineDomPatch, TimelineScrollBehavior};
use crate::workspace_pipeline::WorkspaceRevision;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineSurface {
    Main,
    Thread,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineDocument {
    Conversation(String),
    Thread { channel_id: String, ts: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineDelta {
    document: TimelineDocument,
    base_revision: WorkspaceRevision,
    revision: WorkspaceRevision,
    patches: Vec<TimelineDomPatch>,
    scroll: TimelineScrollBehavior,
}

#[allow(dead_code)]
impl TimelineDelta {
    pub fn new(
        document: TimelineDocument,
        base_revision: WorkspaceRevision,
        revision: WorkspaceRevision,
        patches: Vec<TimelineDomPatch>,
        scroll: TimelineScrollBehavior,
    ) -> Option<Self> {
        // Derived presentation enrichments (for example a delayed asset) can
        // stay on the same authoritative workspace revision.
        (revision >= base_revision && !patches.is_empty()).then_some(Self {
            document,
            base_revision,
            revision,
            patches,
            scroll,
        })
    }

    pub fn document(&self) -> &TimelineDocument {
        &self.document
    }

    pub fn base_revision(&self) -> WorkspaceRevision {
        self.base_revision
    }

    pub fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    pub fn patches(&self) -> &[TimelineDomPatch] {
        &self.patches
    }

    pub fn scroll(&self) -> TimelineScrollBehavior {
        self.scroll
    }

    pub fn merge(&mut self, next: Self) {
        debug_assert_eq!(self.document, next.document);
        debug_assert_eq!(self.revision, next.base_revision);
        self.revision = next.revision;
        self.patches.extend(next.patches);
        self.scroll = merge_timeline_delta_scroll(self.scroll, next.scroll);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelinePresenterAction {
    LoadDocument,
    ReloadDocument,
    ScheduleFrame,
    Queued,
    Ready,
}

#[derive(Debug, Default)]
pub struct TimelinePresenter {
    document: Option<TimelineDocument>,
    presented_revision: WorkspaceRevision,
    loading: bool,
    reload_required: bool,
    pending: Option<TimelineDelta>,
    pinned_to_bottom: bool,
    user_scrolled: bool,
}

#[allow(dead_code)]
impl TimelinePresenter {
    pub fn prepare_document(
        &mut self,
        document: TimelineDocument,
        revision: WorkspaceRevision,
        scroll: TimelineScrollBehavior,
    ) -> TimelinePresenterAction {
        if self.document.is_none() || self.reload_required {
            return self.begin_document(document, revision, scroll);
        }
        if self.loading {
            if self.document.as_ref() == Some(&document) {
                return TimelinePresenterAction::Queued;
            }
            return self.begin_document(document, revision, scroll);
        }
        let expected_revision = self.expected_revision();
        if self.document.as_ref() != Some(&document) {
            self.recycle_document(document, revision, scroll);
            return TimelinePresenterAction::Ready;
        }
        if revision < expected_revision {
            return self.begin_document(document, revision, scroll);
        }
        TimelinePresenterAction::Ready
    }

    pub fn recycle_document(
        &mut self,
        document: TimelineDocument,
        revision: WorkspaceRevision,
        scroll: TimelineScrollBehavior,
    ) {
        self.document = Some(document);
        self.presented_revision = revision;
        self.loading = false;
        self.reload_required = false;
        self.pending = None;
        self.pinned_to_bottom = matches!(
            scroll,
            TimelineScrollBehavior::Bottom | TimelineScrollBehavior::StickToBottom
        );
        self.user_scrolled = false;
    }

    pub fn begin_document(
        &mut self,
        document: TimelineDocument,
        revision: WorkspaceRevision,
        scroll: TimelineScrollBehavior,
    ) -> TimelinePresenterAction {
        self.document = Some(document);
        self.presented_revision = revision;
        self.loading = true;
        self.reload_required = false;
        self.pending = None;
        self.pinned_to_bottom = matches!(
            scroll,
            TimelineScrollBehavior::Bottom | TimelineScrollBehavior::StickToBottom
        );
        self.user_scrolled = false;
        TimelinePresenterAction::LoadDocument
    }

    pub fn document_loaded(
        &mut self,
        document: &TimelineDocument,
        revision: WorkspaceRevision,
    ) -> TimelinePresenterAction {
        if self.document.as_ref() != Some(document) || self.presented_revision != revision {
            return self.require_reload();
        }
        self.loading = false;
        if self.pending.is_some() {
            TimelinePresenterAction::ScheduleFrame
        } else {
            TimelinePresenterAction::Ready
        }
    }

    pub fn queue_delta(&mut self, mut delta: TimelineDelta) -> TimelinePresenterAction {
        let expected_revision = self
            .pending
            .as_ref()
            .map(TimelineDelta::revision)
            .unwrap_or(self.presented_revision);
        if self.document.as_ref() != Some(&delta.document)
            || delta.base_revision != expected_revision
        {
            return self.require_reload();
        }

        delta.scroll = effective_timeline_delta_scroll(
            delta.scroll,
            self.pinned_to_bottom,
            self.user_scrolled,
        );
        if let Some(pending) = self.pending.as_mut() {
            pending.merge(delta);
            TimelinePresenterAction::Queued
        } else {
            self.pending = Some(delta);
            if self.loading {
                TimelinePresenterAction::Queued
            } else {
                TimelinePresenterAction::ScheduleFrame
            }
        }
    }

    pub fn take_frame(&mut self) -> Option<TimelineDelta> {
        if self.loading {
            return None;
        }
        let delta = self.pending.take()?;
        debug_assert_eq!(delta.base_revision, self.presented_revision);
        self.presented_revision = delta.revision;
        Some(delta)
    }

    pub fn note_user_scrolled(&mut self) {
        self.user_scrolled = true;
        self.pinned_to_bottom = false;
        if let Some(pending) = self.pending.as_mut() {
            pending.scroll = effective_timeline_delta_scroll(pending.scroll, false, true);
        }
    }

    pub fn note_pinned_to_bottom(&mut self) {
        self.user_scrolled = false;
        self.pinned_to_bottom = true;
    }

    pub fn patch_failed(&mut self) -> TimelinePresenterAction {
        self.require_reload()
    }

    pub fn document(&self) -> Option<&TimelineDocument> {
        self.document.as_ref()
    }

    pub fn expected_revision(&self) -> WorkspaceRevision {
        self.pending
            .as_ref()
            .map(TimelineDelta::revision)
            .unwrap_or(self.presented_revision)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn presented_revision(&self) -> WorkspaceRevision {
        self.presented_revision
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn reload_required(&self) -> bool {
        self.reload_required
    }

    pub fn require_reload(&mut self) -> TimelinePresenterAction {
        self.loading = true;
        self.reload_required = true;
        self.pending = None;
        TimelinePresenterAction::ReloadDocument
    }
}

pub fn effective_timeline_delta_scroll(
    requested: TimelineScrollBehavior,
    pinned_to_bottom: bool,
    user_scrolled: bool,
) -> TimelineScrollBehavior {
    if requested == TimelineScrollBehavior::PreservePrepend {
        TimelineScrollBehavior::PreservePrepend
    } else if user_scrolled || !pinned_to_bottom {
        TimelineScrollBehavior::Preserve
    } else if matches!(
        requested,
        TimelineScrollBehavior::Bottom | TimelineScrollBehavior::StickToBottom
    ) {
        TimelineScrollBehavior::StickToBottom
    } else {
        TimelineScrollBehavior::Preserve
    }
}

pub fn merge_timeline_delta_scroll(
    current: TimelineScrollBehavior,
    next: TimelineScrollBehavior,
) -> TimelineScrollBehavior {
    if current == TimelineScrollBehavior::PreservePrepend
        || next == TimelineScrollBehavior::PreservePrepend
    {
        TimelineScrollBehavior::PreservePrepend
    } else if matches!(
        current,
        TimelineScrollBehavior::Bottom | TimelineScrollBehavior::StickToBottom
    ) || matches!(
        next,
        TimelineScrollBehavior::Bottom | TimelineScrollBehavior::StickToBottom
    ) {
        TimelineScrollBehavior::StickToBottom
    } else {
        TimelineScrollBehavior::Preserve
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UiInvalidations(pub u8);

impl UiInvalidations {
    pub const SIDEBAR: Self = Self(1 << 0);
    pub const MAIN: Self = Self(1 << 1);
    pub const THREAD: Self = Self(1 << 2);
    pub const TITLE: Self = Self(1 << 3);
    pub const PICKER: Self = Self(1 << 4);

    pub fn contains(self, invalidation: Self) -> bool {
        self.0 & invalidation.0 != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn insert(&mut self, invalidations: Self) -> bool {
        let was_empty = self.0 == 0;
        self.0 |= invalidations.0;
        was_empty
    }

    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

impl std::ops::BitOr for UiInvalidations {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

pub fn timeline_surface_invalidation(surface: TimelineSurface) -> UiInvalidations {
    match surface {
        TimelineSurface::Main => UiInvalidations::MAIN,
        TimelineSurface::Thread => UiInvalidations::THREAD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_html::{CachedAssetKind, CachedAssetSource, TimelineInsertPosition};

    fn timeline_revision(value: usize) -> WorkspaceRevision {
        (0..value).fold(WorkspaceRevision::INITIAL, |revision, _| {
            revision.successor()
        })
    }

    fn timeline_document() -> TimelineDocument {
        TimelineDocument::Conversation("C123".to_string())
    }

    fn timeline_delta(
        base: usize,
        revision: usize,
        patch: TimelineDomPatch,
        scroll: TimelineScrollBehavior,
    ) -> TimelineDelta {
        TimelineDelta::new(
            timeline_document(),
            timeline_revision(base),
            timeline_revision(revision),
            vec![patch],
            scroll,
        )
        .unwrap()
    }

    #[test]
    fn timeline_presenter_queues_loading_deltas_and_batches_one_frame() {
        let document = timeline_document();
        let mut presenter = TimelinePresenter::default();
        assert_eq!(
            presenter.begin_document(
                document.clone(),
                timeline_revision(1),
                TimelineScrollBehavior::Bottom,
            ),
            TimelinePresenterAction::LoadDocument
        );

        let patches = [
            TimelineDomPatch::InsertMessage {
                position: TimelineInsertPosition::Append,
                message_ts: "insert".to_string(),
                arrival: None,
                html: "<li>insert</li>".to_string(),
            },
            TimelineDomPatch::ReplaceMessage {
                message_ts: "edit".to_string(),
                arrival: None,
                html: "<article>edit</article>".to_string(),
                part_html: "<div>edit</div>".to_string(),
            },
            TimelineDomPatch::RemoveMessage {
                message_ts: "delete".to_string(),
            },
            TimelineDomPatch::UpdateUser {
                user_id: "U1".to_string(),
                name: "Ada".to_string(),
                status_html: String::new(),
            },
        ];
        for (offset, patch) in patches.into_iter().enumerate() {
            assert_eq!(
                presenter.queue_delta(timeline_delta(
                    1 + offset,
                    2 + offset,
                    patch,
                    TimelineScrollBehavior::StickToBottom,
                )),
                TimelinePresenterAction::Queued
            );
        }

        assert_eq!(
            presenter.document_loaded(&document, timeline_revision(1)),
            TimelinePresenterAction::ScheduleFrame
        );
        let batch = presenter.take_frame().unwrap();
        assert_eq!(batch.base_revision(), timeline_revision(1));
        assert_eq!(batch.revision(), timeline_revision(5));
        assert_eq!(batch.patches().len(), 4);
        assert_eq!(batch.scroll(), TimelineScrollBehavior::StickToBottom);
        assert_eq!(presenter.presented_revision(), timeline_revision(5));
        assert_eq!(presenter.take_frame(), None);
    }

    #[test]
    fn timeline_presenter_loads_only_initial_mismatched_or_corrupt_documents() {
        let document = timeline_document();
        let mut presenter = TimelinePresenter::default();

        assert_eq!(
            presenter.prepare_document(
                document.clone(),
                timeline_revision(1),
                TimelineScrollBehavior::Bottom,
            ),
            TimelinePresenterAction::LoadDocument
        );
        assert_eq!(
            presenter.prepare_document(
                document.clone(),
                timeline_revision(1),
                TimelineScrollBehavior::Bottom,
            ),
            TimelinePresenterAction::Queued
        );
        assert_eq!(
            presenter.document_loaded(&document, timeline_revision(1)),
            TimelinePresenterAction::Ready
        );
        assert_eq!(
            presenter.prepare_document(
                document.clone(),
                timeline_revision(1),
                TimelineScrollBehavior::Preserve,
            ),
            TimelinePresenterAction::Ready
        );

        presenter.patch_failed();
        assert_eq!(
            presenter.prepare_document(
                document.clone(),
                timeline_revision(2),
                TimelineScrollBehavior::Preserve,
            ),
            TimelinePresenterAction::LoadDocument
        );

        assert_eq!(
            presenter.prepare_document(
                TimelineDocument::Conversation("C999".to_string()),
                timeline_revision(3),
                TimelineScrollBehavior::Preserve,
            ),
            TimelinePresenterAction::LoadDocument
        );

        // Once loaded, navigating to another channel or thread recycles in-place.
        assert_eq!(
            presenter.document_loaded(
                &TimelineDocument::Conversation("C999".to_string()),
                timeline_revision(3)
            ),
            TimelinePresenterAction::Ready
        );
        let recycled_channel = TimelineDocument::Conversation("C888".to_string());
        assert_eq!(
            presenter.prepare_document(
                recycled_channel.clone(),
                timeline_revision(4),
                TimelineScrollBehavior::Bottom,
            ),
            TimelinePresenterAction::Ready
        );
        assert_eq!(presenter.document(), Some(&recycled_channel));
        assert_eq!(presenter.presented_revision(), timeline_revision(4));
        assert!(!presenter.is_loading());
        assert!(!presenter.reload_required());

        // Deltas for the recycled document queue and schedule frames cleanly.
        let recycled_delta = TimelineDelta::new(
            recycled_channel.clone(),
            timeline_revision(4),
            timeline_revision(5),
            vec![TimelineDomPatch::InsertMessage {
                position: TimelineInsertPosition::Append,
                message_ts: "recycled_msg".to_string(),
                arrival: None,
                html: "<li>recycled</li>".to_string(),
            }],
            TimelineScrollBehavior::Bottom,
        )
        .unwrap();
        assert_eq!(
            presenter.queue_delta(recycled_delta),
            TimelinePresenterAction::ScheduleFrame
        );
        let frame = presenter.take_frame().unwrap();
        assert_eq!(frame.document(), &recycled_channel);
        assert_eq!(frame.base_revision(), timeline_revision(4));
        assert_eq!(frame.revision(), timeline_revision(5));
    }

    #[test]
    fn timeline_presenter_revision_or_document_mismatch_requires_reload() {
        let document = timeline_document();
        let mut presenter = TimelinePresenter::default();
        presenter.begin_document(
            document.clone(),
            timeline_revision(3),
            TimelineScrollBehavior::Preserve,
        );
        presenter.document_loaded(&document, timeline_revision(3));

        assert_eq!(
            presenter.queue_delta(timeline_delta(
                1,
                4,
                TimelineDomPatch::RemoveMessage {
                    message_ts: "stale".to_string(),
                },
                TimelineScrollBehavior::Preserve,
            )),
            TimelinePresenterAction::ReloadDocument
        );
        assert!(presenter.is_loading());
        assert_eq!(presenter.take_frame(), None);

        let other = TimelineDocument::Conversation("C999".to_string());
        let mismatched = TimelineDelta::new(
            other,
            timeline_revision(3),
            timeline_revision(4),
            vec![TimelineDomPatch::RemoveMessage {
                message_ts: "other".to_string(),
            }],
            TimelineScrollBehavior::Preserve,
        )
        .unwrap();
        assert_eq!(
            presenter.queue_delta(mismatched),
            TimelinePresenterAction::ReloadDocument
        );

        presenter.begin_document(
            document.clone(),
            timeline_revision(3),
            TimelineScrollBehavior::Preserve,
        );
        presenter.document_loaded(&document, timeline_revision(3));
        assert_eq!(
            presenter.patch_failed(),
            TimelinePresenterAction::ReloadDocument
        );
        assert!(presenter.is_loading());
    }

    #[test]
    fn timeline_presenter_preserves_prepend_anchor_across_enrichment() {
        let document = timeline_document();
        let mut presenter = TimelinePresenter::default();
        presenter.begin_document(
            document.clone(),
            timeline_revision(1),
            TimelineScrollBehavior::Preserve,
        );
        presenter.document_loaded(&document, timeline_revision(1));
        assert_eq!(
            presenter.queue_delta(timeline_delta(
                1,
                2,
                TimelineDomPatch::RemoveMessage {
                    message_ts: "older".to_string(),
                },
                TimelineScrollBehavior::PreservePrepend,
            )),
            TimelinePresenterAction::ScheduleFrame
        );
        assert_eq!(
            presenter.queue_delta(timeline_delta(
                2,
                3,
                TimelineDomPatch::UpdateUser {
                    user_id: "U1".to_string(),
                    name: "Ada".to_string(),
                    status_html: String::new(),
                },
                TimelineScrollBehavior::Preserve,
            )),
            TimelinePresenterAction::Queued
        );

        assert_eq!(
            presenter.take_frame().unwrap().scroll(),
            TimelineScrollBehavior::PreservePrepend
        );
    }

    #[test]
    fn timeline_presenter_user_scroll_cancels_bottom_and_delayed_media_following() {
        let document = timeline_document();
        let mut presenter = TimelinePresenter::default();
        presenter.begin_document(
            document.clone(),
            timeline_revision(1),
            TimelineScrollBehavior::Bottom,
        );
        presenter.document_loaded(&document, timeline_revision(1));
        presenter.note_user_scrolled();
        presenter.queue_delta(timeline_delta(
            1,
            1,
            TimelineDomPatch::UpdateImage {
                asset_key: "asset".to_string(),
                source: CachedAssetSource::from_cache_key(&"a".repeat(64), CachedAssetKind::Image),
            },
            TimelineScrollBehavior::StickToBottom,
        ));

        assert_eq!(
            presenter.take_frame().unwrap().scroll(),
            TimelineScrollBehavior::Preserve
        );

        presenter.note_pinned_to_bottom();
        presenter.queue_delta(timeline_delta(
            1,
            2,
            TimelineDomPatch::RemoveMessage {
                message_ts: "new".to_string(),
            },
            TimelineScrollBehavior::StickToBottom,
        ));
        assert_eq!(
            presenter.take_frame().unwrap().scroll(),
            TimelineScrollBehavior::StickToBottom
        );
    }
}
