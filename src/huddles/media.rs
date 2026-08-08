// The production Slack/Chime adapter is capability-gated, so this reusable
// media implementation is exercised by the synthetic harness until that
// private contract can be verified.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::fmt;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::huddles::state::{
    HuddleControls, HuddleDeviceKind, HuddleDeviceSelection, HuddleSessionStatistics,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MediaSourceMode {
    #[default]
    System,
    Synthetic,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MediaSinkMode {
    #[default]
    System,
    Fake,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaSessionConfig {
    pub source_mode: MediaSourceMode,
    pub sink_mode: MediaSinkMode,
    pub controls: HuddleControls,
    pub devices: HuddleDeviceSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaDescriptionKind {
    Offer,
    Answer,
}

const MEDIA_EVENT_CAPACITY: usize = 64;
const MAX_SESSION_DESCRIPTION_BYTES: usize = 256 * 1024;
const MAX_ICE_CANDIDATE_BYTES: usize = 8 * 1024;
const MAX_REMOTE_ICE_CANDIDATES: usize = 256;

struct SensitiveMediaValue(Box<[u8]>);

impl SensitiveMediaValue {
    fn new(value: &str) -> Result<Self, MediaError> {
        if value.trim().is_empty() {
            return Err(MediaError::InvalidSessionData);
        }
        Ok(Self(value.as_bytes().to_vec().into_boxed_slice()))
    }

    fn expose(&self) -> Result<&str, MediaError> {
        std::str::from_utf8(&self.0).map_err(|_| MediaError::InvalidSessionData)
    }
}

impl fmt::Debug for SensitiveMediaValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Drop for SensitiveMediaValue {
    fn drop(&mut self) {
        volatile_zeroize(&mut self.0);
        #[cfg(test)]
        notify_sensitive_media_value_dropped();
    }
}

#[cfg(test)]
std::thread_local! {
    static SENSITIVE_MEDIA_VALUE_DROP_HOOK: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_sensitive_media_value_drop_hook(hook: Option<Box<dyn Fn()>>) {
    SENSITIVE_MEDIA_VALUE_DROP_HOOK.with(|current| *current.borrow_mut() = hook);
}

#[cfg(test)]
fn notify_sensitive_media_value_dropped() {
    SENSITIVE_MEDIA_VALUE_DROP_HOOK.with(|current| {
        if let Some(hook) = current.borrow().as_ref() {
            hook();
        }
    });
}

struct SensitiveMediaString(String);

impl SensitiveMediaString {
    fn new(value: String) -> Self {
        Self(value)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for SensitiveMediaString {
    fn drop(&mut self) {
        // SAFETY: bytes are only overwritten, never read or resized, while String remains owned.
        let bytes = unsafe { self.0.as_mut_vec() };
        volatile_zeroize(bytes);
    }
}

fn volatile_zeroize(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: byte is a valid, uniquely borrowed pointer for a one-byte volatile write.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}

pub struct MediaDescription {
    kind: MediaDescriptionKind,
    sdp: SensitiveMediaValue,
}

impl MediaDescription {
    pub fn offer(sdp: &str) -> Result<Self, MediaError> {
        Self::new(MediaDescriptionKind::Offer, sdp)
    }

    pub fn answer(sdp: &str) -> Result<Self, MediaError> {
        Self::new(MediaDescriptionKind::Answer, sdp)
    }

    fn new(kind: MediaDescriptionKind, sdp: &str) -> Result<Self, MediaError> {
        if sdp.len() > MAX_SESSION_DESCRIPTION_BYTES {
            return Err(MediaError::PayloadTooLarge);
        }
        if !sdp.trim_start().starts_with("v=0") {
            return Err(MediaError::InvalidSessionData);
        }
        Ok(Self {
            kind,
            sdp: SensitiveMediaValue::new(sdp)?,
        })
    }

    pub fn kind(&self) -> MediaDescriptionKind {
        self.kind
    }

    pub fn sdp(&self) -> Result<&str, MediaError> {
        self.sdp.expose()
    }
}

impl fmt::Debug for MediaDescription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MediaDescription")
            .field("kind", &self.kind)
            .field("sdp", &self.sdp)
            .finish()
    }
}

pub struct IceCandidate {
    sdp_m_line_index: u32,
    value: SensitiveMediaValue,
}

impl IceCandidate {
    pub fn new(sdp_m_line_index: u32, value: &str) -> Result<Self, MediaError> {
        if value.len() > MAX_ICE_CANDIDATE_BYTES {
            return Err(MediaError::PayloadTooLarge);
        }
        let value = value.trim();
        if !value.starts_with("candidate:") {
            return Err(MediaError::InvalidSessionData);
        }
        Ok(Self {
            sdp_m_line_index,
            value: SensitiveMediaValue::new(value)?,
        })
    }

    pub fn sdp_m_line_index(&self) -> u32 {
        self.sdp_m_line_index
    }

    pub fn value(&self) -> Result<&str, MediaError> {
        self.value.expose()
    }
}

impl fmt::Debug for IceCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IceCandidate")
            .field("sdp_m_line_index", &self.sdp_m_line_index)
            .field("value", &self.value)
            .finish()
    }
}

#[derive(Debug)]
pub enum MediaEvent {
    LocalDescription(MediaDescription),
    LocalIceCandidate(IceCandidate),
    Statistics(HuddleSessionStatistics),
    Failed(MediaError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    #[error("the media session is already running")]
    AlreadyRunning,
    #[error("no media session is running")]
    NotRunning,
    #[error("the media session data was invalid")]
    InvalidSessionData,
    #[error("the required media components are unavailable")]
    ComponentsUnavailable,
    #[error("the selected media device is unavailable")]
    DeviceUnavailable,
    #[error("the media operation failed")]
    OperationFailed,
    #[error("another media operation is still pending")]
    OperationPending,
    #[error("the media event queue was saturated")]
    AdmissionSaturated,
    #[error("the media session generation is closed")]
    GenerationClosed,
    #[error("the media payload exceeded its size limit")]
    PayloadTooLarge,
    #[error("the remote ICE candidate limit was exceeded")]
    RemoteIceLimitExceeded,
    #[error("the incoming media stream was unsupported or duplicated")]
    IncomingMediaRejected,
}

#[derive(Debug, Default)]
struct MediaEventMailbox {
    state: Mutex<MediaEventMailboxState>,
}

#[derive(Debug, Default)]
struct MediaEventMailboxState {
    events: VecDeque<MediaEvent>,
    closed: bool,
    metrics: MediaEventMailboxMetrics,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MediaEventMailboxMetrics {
    admitted_reliable: u64,
    admitted_statistics: u64,
    coalesced_statistics: u64,
    dropped_statistics: u64,
    evicted_statistics: u64,
    discarded_on_seal: u64,
    discarded_on_close: u64,
    dequeued: u64,
    rejected_closed: u64,
    sealed: u64,
    depth: usize,
    peak_depth: usize,
}

impl MediaEventMailbox {
    fn publish(&self, event: MediaEvent) -> Result<(), MediaError> {
        let mut state = self.state();
        if state.closed {
            state.metrics.rejected_closed = state.metrics.rejected_closed.saturating_add(1);
            return Err(MediaError::GenerationClosed);
        }

        if matches!(event, MediaEvent::Statistics(_)) {
            if let Some(queued) = state
                .events
                .iter_mut()
                .find(|queued| matches!(queued, MediaEvent::Statistics(_)))
            {
                *queued = event;
                state.metrics.admitted_statistics =
                    state.metrics.admitted_statistics.saturating_add(1);
                state.metrics.coalesced_statistics =
                    state.metrics.coalesced_statistics.saturating_add(1);
            } else if state.events.len() < MEDIA_EVENT_CAPACITY {
                state.events.push_back(event);
                state.metrics.admitted_statistics =
                    state.metrics.admitted_statistics.saturating_add(1);
                Self::record_depth(&mut state);
            } else {
                state.metrics.dropped_statistics =
                    state.metrics.dropped_statistics.saturating_add(1);
            }
            return Ok(());
        }

        if state.events.len() == MEDIA_EVENT_CAPACITY {
            if let Some(index) = state
                .events
                .iter()
                .position(|queued| matches!(queued, MediaEvent::Statistics(_)))
            {
                drop(state.events.remove(index));
                state.metrics.evicted_statistics =
                    state.metrics.evicted_statistics.saturating_add(1);
            } else {
                let discarded = Self::seal_locked(&mut state, MediaError::AdmissionSaturated);
                drop(state);
                drop(discarded);
                return Err(MediaError::AdmissionSaturated);
            }
        }
        state.events.push_back(event);
        state.metrics.admitted_reliable = state.metrics.admitted_reliable.saturating_add(1);
        Self::record_depth(&mut state);
        Ok(())
    }

    fn seal(&self, error: MediaError) {
        let discarded = {
            let mut state = self.state();
            (!state.closed).then(|| Self::seal_locked(&mut state, error))
        };
        drop(discarded);
    }

    fn close_and_clear(&self) {
        let discarded = {
            let mut state = self.state();
            state.closed = true;
            let discarded = std::mem::take(&mut state.events);
            state.metrics.discarded_on_close = state
                .metrics
                .discarded_on_close
                .saturating_add(discarded.len() as u64);
            state.metrics.depth = 0;
            discarded
        };
        drop(discarded);
    }

    fn ensure_open(&self) -> Result<(), MediaError> {
        (!self.state().closed)
            .then_some(())
            .ok_or(MediaError::GenerationClosed)
    }

    fn drain(&self) -> Vec<MediaEvent> {
        let mut state = self.state();
        let events: Vec<_> = state.events.drain(..).collect();
        state.metrics.dequeued = state.metrics.dequeued.saturating_add(events.len() as u64);
        state.metrics.depth = 0;
        events
    }

    #[cfg(test)]
    fn depth(&self) -> usize {
        self.state().events.len()
    }

    fn snapshot(&self) -> MediaEventMailboxMetrics {
        let state = self.state();
        let mut metrics = state.metrics;
        metrics.depth = state.events.len();
        metrics
    }

    fn state(&self) -> MutexGuard<'_, MediaEventMailboxState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn seal_locked(state: &mut MediaEventMailboxState, error: MediaError) -> VecDeque<MediaEvent> {
        let discarded = std::mem::take(&mut state.events);
        state.metrics.discarded_on_seal = state
            .metrics
            .discarded_on_seal
            .saturating_add(discarded.len() as u64);
        state.events.push_back(MediaEvent::Failed(error));
        state.closed = true;
        state.metrics.sealed = state.metrics.sealed.saturating_add(1);
        Self::record_depth(state);
        discarded
    }

    fn record_depth(state: &mut MediaEventMailboxState) {
        state.metrics.depth = state.events.len();
        state.metrics.peak_depth = state.metrics.peak_depth.max(state.metrics.depth);
    }
}

#[derive(Clone, Debug, Default)]
struct OperationGate(Arc<AtomicBool>);

impl OperationGate {
    fn begin(&self) -> Result<OperationGuard, MediaError> {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| MediaError::OperationPending)?;
        Ok(OperationGuard { gate: self.clone() })
    }
}

struct OperationGuard {
    gate: OperationGate,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.gate.0.store(false, Ordering::Release);
    }
}

pub trait MediaEngine {
    fn start(&mut self, config: MediaSessionConfig) -> Result<(), MediaError>;
    fn create_offer(&mut self) -> Result<(), MediaError>;
    fn set_remote_description(&mut self, description: MediaDescription) -> Result<(), MediaError>;
    fn add_remote_ice_candidate(&mut self, candidate: IceCandidate) -> Result<(), MediaError>;
    fn apply_controls(&mut self, controls: HuddleControls) -> Result<(), MediaError>;
    fn select_device(&mut self, kind: HuddleDeviceKind, id: &str) -> Result<(), MediaError>;
    fn attach_screen_share(&mut self, remote_fd: OwnedFd, node_id: u32) -> Result<(), MediaError>;
    fn detach_screen_share(&mut self) -> Result<(), MediaError>;
    fn screen_share_active(&self) -> bool;
    fn request_statistics(&mut self) -> Result<(), MediaError>;
    fn drain_events(&mut self) -> Vec<MediaEvent>;
    fn stop(&mut self) -> Result<(), MediaError>;
    fn is_running(&self) -> bool;
}

#[derive(Debug, Default)]
pub struct SyntheticMediaEngine {
    config: Option<MediaSessionConfig>,
    remote_description_set: bool,
    remote_candidates: usize,
    screen_share_remote: Option<OwnedFd>,
    event_mailbox: Option<Arc<MediaEventMailbox>>,
}

impl MediaEngine for SyntheticMediaEngine {
    fn start(&mut self, config: MediaSessionConfig) -> Result<(), MediaError> {
        if self.config.is_some() {
            return Err(MediaError::AlreadyRunning);
        }
        if config.controls.screen_share_enabled {
            return Err(MediaError::OperationFailed);
        }
        self.config = Some(config);
        self.remote_description_set = false;
        self.remote_candidates = 0;
        self.event_mailbox = Some(Arc::new(MediaEventMailbox::default()));
        Ok(())
    }

    fn create_offer(&mut self) -> Result<(), MediaError> {
        self.require_open()?;
        self.event_mailbox()?
            .publish(MediaEvent::LocalDescription(MediaDescription::offer(
                "v=0\r\no=conduit-synthetic 0 0 IN IP4 127.0.0.1\r\n",
            )?))
    }

    fn set_remote_description(&mut self, _description: MediaDescription) -> Result<(), MediaError> {
        self.require_open()?;
        self.remote_description_set = true;
        Ok(())
    }

    fn add_remote_ice_candidate(&mut self, _candidate: IceCandidate) -> Result<(), MediaError> {
        self.require_open()?;
        if self.remote_candidates >= MAX_REMOTE_ICE_CANDIDATES {
            self.event_mailbox()?
                .seal(MediaError::RemoteIceLimitExceeded);
            return Err(MediaError::RemoteIceLimitExceeded);
        }
        self.remote_candidates = self.remote_candidates.saturating_add(1);
        Ok(())
    }

    fn apply_controls(&mut self, controls: HuddleControls) -> Result<(), MediaError> {
        self.require_open()?;
        if let Some(config) = self.config.as_mut() {
            config.controls = controls;
        }
        Ok(())
    }

    fn select_device(&mut self, kind: HuddleDeviceKind, id: &str) -> Result<(), MediaError> {
        self.require_open()?;
        if id.trim().is_empty() {
            return Err(MediaError::DeviceUnavailable);
        }
        if let Some(config) = self.config.as_mut() {
            config.devices.select(kind, id.to_string());
        }
        Ok(())
    }

    fn request_statistics(&mut self) -> Result<(), MediaError> {
        self.require_open()?;
        self.event_mailbox()?.publish(MediaEvent::Statistics(
            MediaStatisticsSample::default().into_session_statistics(),
        ))
    }

    fn attach_screen_share(&mut self, remote_fd: OwnedFd, node_id: u32) -> Result<(), MediaError> {
        self.require_open()?;
        if self.screen_share_remote.is_some() || node_id == 0 {
            return Err(MediaError::OperationFailed);
        }
        self.screen_share_remote = Some(remote_fd);
        if let Some(config) = self.config.as_mut() {
            config.controls.screen_share_enabled = true;
        }
        Ok(())
    }

    fn detach_screen_share(&mut self) -> Result<(), MediaError> {
        self.require_open()?;
        self.screen_share_remote = None;
        if let Some(config) = self.config.as_mut() {
            config.controls.screen_share_enabled = false;
        }
        Ok(())
    }

    fn screen_share_active(&self) -> bool {
        self.screen_share_remote.is_some()
    }

    fn drain_events(&mut self) -> Vec<MediaEvent> {
        self.event_mailbox
            .as_ref()
            .map_or_else(Vec::new, |mailbox| mailbox.drain())
    }

    fn stop(&mut self) -> Result<(), MediaError> {
        if let Some(mailbox) = self.event_mailbox.take() {
            mailbox.close_and_clear();
        }
        self.config = None;
        self.remote_description_set = false;
        self.remote_candidates = 0;
        self.screen_share_remote = None;
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.config.is_some()
    }
}

impl SyntheticMediaEngine {
    fn require_running(&self) -> Result<(), MediaError> {
        self.is_running()
            .then_some(())
            .ok_or(MediaError::NotRunning)
    }

    fn event_mailbox(&self) -> Result<&Arc<MediaEventMailbox>, MediaError> {
        self.event_mailbox.as_ref().ok_or(MediaError::NotRunning)
    }

    fn require_open(&self) -> Result<(), MediaError> {
        self.require_running()?;
        self.event_mailbox()?.ensure_open()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MediaStatisticsSample {
    pub round_trip_seconds: Option<f64>,
    pub jitter_seconds: Option<f64>,
    pub packets_lost: Option<i64>,
    pub packets_received: Option<u64>,
    pub audio_bitrate_bps: Option<f64>,
    pub video_bitrate_bps: Option<f64>,
}

impl MediaStatisticsSample {
    pub fn into_session_statistics(self) -> HuddleSessionStatistics {
        HuddleSessionStatistics {
            round_trip_ms: seconds_to_milliseconds(self.round_trip_seconds),
            jitter_ms: seconds_to_milliseconds(self.jitter_seconds),
            packets_lost: self.packets_lost.unwrap_or_default().max(0) as u64,
            packets_received: self.packets_received.unwrap_or_default(),
            audio_bitrate_bps: finite_rate(self.audio_bitrate_bps),
            video_bitrate_bps: finite_rate(self.video_bitrate_bps),
        }
    }
}

fn seconds_to_milliseconds(value: Option<f64>) -> u32 {
    let value = value.unwrap_or_default();
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    (value * 1_000.0).floor().min(u32::MAX as f64) as u32
}

fn finite_rate(value: Option<f64>) -> u64 {
    let value = value.unwrap_or_default();
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    value.floor().min(u64::MAX as f64) as u64
}

#[cfg(feature = "native-media")]
mod native {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::sync::{Arc, Condvar, Mutex};

    use gst::glib;
    use gst::prelude::*;
    use gstreamer as gst;
    use gstreamer_sdp as gst_sdp;
    use gstreamer_webrtc as gst_webrtc;

    use super::{
        IceCandidate, MediaDescription, MediaDescriptionKind, MediaEngine, MediaError, MediaEvent,
        MediaEventMailbox, MediaSessionConfig, MediaSinkMode, MediaSourceMode,
        MediaStatisticsSample, OperationGate, SensitiveMediaString, MAX_REMOTE_ICE_CANDIDATES,
    };
    use crate::huddles::devices::NativeDeviceCatalog;
    use crate::huddles::state::{HuddleControls, HuddleDevice, HuddleDeviceKind};

    pub struct GStreamerMediaEngine {
        catalog: Option<NativeDeviceCatalog>,
        session: Option<NativeSession>,
    }

    struct NativeSession {
        pipeline: gst::Pipeline,
        peer: gst::Element,
        microphone_source: gst::Element,
        microphone_valve: gst::Element,
        speaker_sink: gst::Element,
        camera_source: gst::Element,
        camera_valve: gst::Element,
        controls: HuddleControls,
        event_mailbox: Arc<MediaEventMailbox>,
        remote_candidates: usize,
        offer_gate: OperationGate,
        statistics_gate: OperationGate,
        callback_gate: Arc<NativeCallbackGate>,
        #[cfg(feature = "screen-share")]
        screen_share: Option<ScreenShareBranch>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum IncomingMediaKind {
        Audio,
        Video,
    }

    #[derive(Debug, Default)]
    struct IncomingBranchAdmission {
        claimed: Mutex<IncomingBranches>,
    }

    #[derive(Debug, Default)]
    struct IncomingBranches {
        audio: bool,
        video: bool,
    }

    #[derive(Debug, Default)]
    struct NativeCallbackGate {
        state: Mutex<NativeCallbackGateState>,
        idle: Condvar,
    }

    #[derive(Debug, Default)]
    struct NativeCallbackGateState {
        active: usize,
        closed: bool,
    }

    struct NativeCallbackGuard {
        gate: Arc<NativeCallbackGate>,
    }

    impl IncomingBranchAdmission {
        fn claim(&self, caps: &gst::CapsRef) -> Result<IncomingMediaKind, MediaError> {
            let kind = match caps.structure(0).and_then(|structure| {
                (structure.name() == "application/x-rtp")
                    .then(|| structure.get::<&str>("media").ok())
                    .flatten()
            }) {
                Some("audio") => IncomingMediaKind::Audio,
                Some("video") => IncomingMediaKind::Video,
                _ => return Err(MediaError::IncomingMediaRejected),
            };
            let mut claimed = self
                .claimed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let already_claimed = match kind {
                IncomingMediaKind::Audio => &mut claimed.audio,
                IncomingMediaKind::Video => &mut claimed.video,
            };
            if *already_claimed {
                return Err(MediaError::IncomingMediaRejected);
            }
            *already_claimed = true;
            Ok(kind)
        }
    }

    impl NativeCallbackGate {
        fn begin(self: &Arc<Self>) -> Result<NativeCallbackGuard, MediaError> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.closed {
                return Err(MediaError::GenerationClosed);
            }
            state.active = state.active.saturating_add(1);
            Ok(NativeCallbackGuard { gate: self.clone() })
        }

        fn close_and_wait(&self) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.closed = true;
            while state.active != 0 {
                state = self
                    .idle
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        }
    }

    impl Drop for NativeCallbackGuard {
        fn drop(&mut self) {
            let mut state = self
                .gate
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.active = state.active.saturating_sub(1);
            if state.active == 0 {
                self.gate.idle.notify_all();
            }
        }
    }

    #[cfg(feature = "screen-share")]
    struct ScreenShareBranch {
        elements: Vec<gst::Element>,
        peer_pad: gst::Pad,
        remote_fd: OwnedFd,
    }

    impl GStreamerMediaEngine {
        pub fn new() -> Result<Self, MediaError> {
            gst::init().map_err(|_| MediaError::ComponentsUnavailable)?;
            ensure_factories()?;
            Ok(Self {
                catalog: None,
                session: None,
            })
        }

        pub fn devices(&self) -> &[HuddleDevice] {
            self.catalog
                .as_ref()
                .map(NativeDeviceCatalog::descriptions)
                .unwrap_or_default()
        }

        pub fn refresh_devices(&mut self) -> Result<&[HuddleDevice], MediaError> {
            self.catalog = Some(NativeDeviceCatalog::scan()?);
            Ok(self.devices())
        }

        pub fn camera_capture_active(&self) -> bool {
            self.session
                .as_ref()
                .is_some_and(|session| session.controls.camera_enabled)
        }

        fn build_session(
            &self,
            config: &MediaSessionConfig,
            event_mailbox: Arc<MediaEventMailbox>,
        ) -> Result<NativeSession, MediaError> {
            if config.controls.screen_share_enabled {
                return Err(MediaError::OperationFailed);
            }
            let pipeline = gst::Pipeline::with_name("conduit-huddle-media");
            let peer = make("webrtcbin", "huddle-webrtc")?;
            peer.set_property_from_str("bundle-policy", "max-bundle");
            pipeline
                .add(&peer)
                .map_err(|_| MediaError::OperationFailed)?;

            let callback_gate = Arc::new(NativeCallbackGate::default());
            let cleanup_pipeline = pipeline.clone();
            let cleanup_mailbox = event_mailbox.clone();
            let cleanup_callback_gate = callback_gate.clone();
            let result = (|| {
                connect_ice_events(&peer, event_mailbox.clone(), callback_gate.clone())?;
                let (microphone_source, microphone_valve) =
                    self.add_audio_sender(&pipeline, &peer, config)?;
                let (camera_source, camera_valve) =
                    self.add_video_sender(&pipeline, &peer, config)?;
                let (audio_target, video_target, speaker_sink) =
                    self.add_receivers(&pipeline, config)?;
                let incoming_admission = Arc::new(IncomingBranchAdmission::default());
                connect_incoming_streams(
                    &pipeline,
                    &peer,
                    audio_target,
                    video_target,
                    event_mailbox.clone(),
                    incoming_admission,
                    callback_gate.clone(),
                );

                camera_valve.set_property("drop", true);
                camera_source.set_locked_state(true);
                camera_source
                    .set_state(gst::State::Null)
                    .map_err(|_| MediaError::OperationFailed)?;
                microphone_valve.set_property("drop", config.controls.microphone_muted);

                let session = NativeSession {
                    pipeline,
                    peer,
                    microphone_source,
                    microphone_valve,
                    speaker_sink,
                    camera_source,
                    camera_valve,
                    controls: config.controls.clone(),
                    event_mailbox,
                    remote_candidates: 0,
                    offer_gate: OperationGate::default(),
                    statistics_gate: OperationGate::default(),
                    callback_gate,
                    #[cfg(feature = "screen-share")]
                    screen_share: None,
                };
                session
                    .pipeline
                    .set_state(gst::State::Playing)
                    .map_err(|_| MediaError::OperationFailed)?;
                if config.controls.camera_enabled {
                    set_camera_capture(&session.camera_source, &session.camera_valve, true)?;
                }
                Ok(session)
            })();
            if result.is_err() {
                cleanup_callback_gate.close_and_wait();
                cleanup_mailbox.close_and_clear();
                let _ = cleanup_pipeline.set_state(gst::State::Null);
            }
            result
        }

        fn add_audio_sender(
            &self,
            pipeline: &gst::Pipeline,
            peer: &gst::Element,
            config: &MediaSessionConfig,
        ) -> Result<(gst::Element, gst::Element), MediaError> {
            let source = match config.source_mode {
                MediaSourceMode::Synthetic => {
                    let source = make("audiotestsrc", "huddle-audio-source")?;
                    source.set_property("is-live", true);
                    source.set_property_from_str("wave", "silence");
                    source
                }
                MediaSourceMode::System => self.system_element(
                    HuddleDeviceKind::Microphone,
                    config.devices.microphone_id.as_deref(),
                    "autoaudiosrc",
                    "huddle-audio-source",
                )?,
            };
            let convert = make("audioconvert", "huddle-audio-convert")?;
            let resample = make("audioresample", "huddle-audio-resample")?;
            let valve = make("valve", "huddle-microphone-valve")?;
            let encoder = make("opusenc", "huddle-opus-encoder")?;
            let payloader = make("rtpopuspay", "huddle-opus-payloader")?;
            let capsfilter = make("capsfilter", "huddle-audio-rtp-caps")?;
            capsfilter.set_property(
                "caps",
                gst::Caps::builder("application/x-rtp")
                    .field("media", "audio")
                    .field("encoding-name", "OPUS")
                    .field("payload", 96i32)
                    .build(),
            );
            let queue = make("queue", "huddle-audio-send-queue")?;

            let mut elements = vec![source.clone(), convert, resample];
            if config.source_mode == MediaSourceMode::System {
                elements.push(make("webrtcdsp", "huddle-audio-dsp")?);
            }
            elements.extend([valve.clone(), encoder, payloader, capsfilter, queue]);
            add_and_link(pipeline, &elements)?;
            let _ = link_to_peer(peer, elements.last().expect("audio queue"))?;
            Ok((source, valve))
        }

        fn add_video_sender(
            &self,
            pipeline: &gst::Pipeline,
            peer: &gst::Element,
            config: &MediaSessionConfig,
        ) -> Result<(gst::Element, gst::Element), MediaError> {
            let source = match config.source_mode {
                MediaSourceMode::Synthetic => {
                    let source = make("videotestsrc", "huddle-camera-source")?;
                    source.set_property("is-live", true);
                    source.set_property_from_str("pattern", "smpte");
                    source
                }
                MediaSourceMode::System => self.system_element(
                    HuddleDeviceKind::Camera,
                    config.devices.camera_id.as_deref(),
                    "autovideosrc",
                    "huddle-camera-source",
                )?,
            };
            let queue = make("queue", "huddle-camera-source-queue")?;
            let convert = make("videoconvert", "huddle-camera-convert")?;
            let scale = make("videoscale", "huddle-camera-scale")?;
            let raw_caps = make("capsfilter", "huddle-camera-raw-caps")?;
            raw_caps.set_property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 1280i32)
                    .field("height", 720i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            );
            let valve = make("valve", "huddle-camera-valve")?;
            let encoder = make("vp8enc", "huddle-vp8-encoder")?;
            encoder.set_property("deadline", 1i64);
            let payloader = make("rtpvp8pay", "huddle-vp8-payloader")?;
            let rtp_caps = make("capsfilter", "huddle-video-rtp-caps")?;
            rtp_caps.set_property(
                "caps",
                gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("encoding-name", "VP8")
                    .field("payload", 97i32)
                    .build(),
            );
            let send_queue = make("queue", "huddle-video-send-queue")?;
            let elements = vec![
                source.clone(),
                queue,
                convert,
                scale,
                raw_caps,
                valve.clone(),
                encoder,
                payloader,
                rtp_caps,
                send_queue,
            ];
            add_and_link(pipeline, &elements)?;
            let _ = link_to_peer(peer, elements.last().expect("video queue"))?;
            Ok((source, valve))
        }

        fn add_receivers(
            &self,
            pipeline: &gst::Pipeline,
            config: &MediaSessionConfig,
        ) -> Result<(gst::Element, gst::Element, gst::Element), MediaError> {
            let audio_queue = make("queue", "huddle-audio-receive-queue")?;
            let audio_convert = make("audioconvert", "huddle-audio-playback-convert")?;
            let audio_resample = make("audioresample", "huddle-audio-playback-resample")?;
            let audio_sink = match config.sink_mode {
                MediaSinkMode::Fake => {
                    let sink = make("fakesink", "huddle-audio-fake-sink")?;
                    sink.set_property("sync", false);
                    sink
                }
                MediaSinkMode::System => self.system_element(
                    HuddleDeviceKind::Speaker,
                    config.devices.speaker_id.as_deref(),
                    "autoaudiosink",
                    "huddle-audio-sink",
                )?,
            };
            let mut audio_elements = vec![audio_queue.clone(), audio_convert, audio_resample];
            if config.source_mode == MediaSourceMode::System {
                audio_elements.push(make("webrtcechoprobe", "huddle-echo-probe")?);
            }
            audio_elements.push(audio_sink.clone());
            add_and_link(pipeline, &audio_elements)?;

            let video_queue = make("queue", "huddle-video-receive-queue")?;
            let video_convert = make("videoconvert", "huddle-video-playback-convert")?;
            let video_sink = make("fakesink", "huddle-video-fake-sink")?;
            video_sink.set_property("sync", false);
            add_and_link(pipeline, &[video_queue.clone(), video_convert, video_sink])?;
            Ok((audio_queue, video_queue, audio_sink))
        }

        fn system_element(
            &self,
            kind: HuddleDeviceKind,
            selected_id: Option<&str>,
            automatic_factory: &str,
            name: &str,
        ) -> Result<gst::Element, MediaError> {
            match selected_id {
                Some(id) => self
                    .catalog
                    .as_ref()
                    .ok_or(MediaError::DeviceUnavailable)?
                    .create_element(kind, id, name),
                None => make(automatic_factory, name),
            }
        }

        fn session(&self) -> Result<&NativeSession, MediaError> {
            self.session.as_ref().ok_or(MediaError::NotRunning)
        }

        fn session_mut(&mut self) -> Result<&mut NativeSession, MediaError> {
            self.session.as_mut().ok_or(MediaError::NotRunning)
        }
    }

    impl MediaEngine for GStreamerMediaEngine {
        fn start(&mut self, config: MediaSessionConfig) -> Result<(), MediaError> {
            if self.session.is_some() {
                return Err(MediaError::AlreadyRunning);
            }
            let event_mailbox = Arc::new(MediaEventMailbox::default());
            match self.build_session(&config, event_mailbox.clone()) {
                Ok(session) => self.session = Some(session),
                Err(error) => {
                    event_mailbox.close_and_clear();
                    return Err(error);
                }
            }
            Ok(())
        }

        fn create_offer(&mut self) -> Result<(), MediaError> {
            let session = self.session()?;
            session.event_mailbox.ensure_open()?;
            let pending = session.offer_gate.begin()?;
            let peer = session.peer.clone();
            let event_mailbox = session.event_mailbox.clone();
            let callback_gate = session.callback_gate.clone();
            let callback_peer = peer.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                let _pending = pending;
                let Ok(_callback) = callback_gate.begin() else {
                    return;
                };
                if event_mailbox.ensure_open().is_err() {
                    return;
                }
                let result = (|| {
                    let reply = reply
                        .map_err(|_| MediaError::OperationFailed)?
                        .ok_or(MediaError::OperationFailed)?;
                    let offer = reply
                        .get::<gst_webrtc::WebRTCSessionDescription>("offer")
                        .map_err(|_| MediaError::OperationFailed)?;
                    let sdp = SensitiveMediaString::new(
                        offer
                            .sdp()
                            .as_text()
                            .map_err(|_| MediaError::OperationFailed)?,
                    );
                    let description = MediaDescription::offer(sdp.expose())?;
                    event_mailbox.ensure_open()?;
                    callback_peer.emit_by_name::<()>(
                        "set-local-description",
                        &[&offer, &None::<gst::Promise>],
                    );
                    Ok(description)
                })();
                match result {
                    Ok(description) => {
                        let _ = event_mailbox.publish(MediaEvent::LocalDescription(description));
                    }
                    Err(error) => {
                        let _ = event_mailbox.publish(MediaEvent::Failed(error));
                    }
                }
            });
            peer.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
            Ok(())
        }

        fn set_remote_description(
            &mut self,
            description: MediaDescription,
        ) -> Result<(), MediaError> {
            let session = self.session()?;
            session.event_mailbox.ensure_open()?;
            let peer = session.peer.clone();
            let sdp = gst_sdp::SDPMessage::parse_buffer(description.sdp()?.as_bytes())
                .map_err(|_| MediaError::InvalidSessionData)?;
            let kind = match description.kind() {
                MediaDescriptionKind::Offer => gst_webrtc::WebRTCSDPType::Offer,
                MediaDescriptionKind::Answer => gst_webrtc::WebRTCSDPType::Answer,
            };
            let description = gst_webrtc::WebRTCSessionDescription::new(kind, sdp);
            peer.emit_by_name::<()>(
                "set-remote-description",
                &[&description, &None::<gst::Promise>],
            );
            Ok(())
        }

        fn add_remote_ice_candidate(&mut self, candidate: IceCandidate) -> Result<(), MediaError> {
            let session = self.session_mut()?;
            session.event_mailbox.ensure_open()?;
            if session.remote_candidates >= MAX_REMOTE_ICE_CANDIDATES {
                session
                    .event_mailbox
                    .seal(MediaError::RemoteIceLimitExceeded);
                return Err(MediaError::RemoteIceLimitExceeded);
            }
            session.peer.emit_by_name::<()>(
                "add-ice-candidate",
                &[&candidate.sdp_m_line_index(), &candidate.value()?],
            );
            session.remote_candidates += 1;
            Ok(())
        }

        fn apply_controls(&mut self, controls: HuddleControls) -> Result<(), MediaError> {
            self.session()?.event_mailbox.ensure_open()?;
            if controls.screen_share_enabled && !self.screen_share_active() {
                return Err(MediaError::OperationFailed);
            }
            let session = self.session_mut()?;
            session
                .microphone_valve
                .set_property("drop", controls.microphone_muted);
            set_camera_capture(
                &session.camera_source,
                &session.camera_valve,
                controls.camera_enabled,
            )?;
            session.controls = controls;
            Ok(())
        }

        fn select_device(&mut self, kind: HuddleDeviceKind, id: &str) -> Result<(), MediaError> {
            let session = self.session()?;
            session.event_mailbox.ensure_open()?;
            let element = match kind {
                HuddleDeviceKind::Microphone => &session.microphone_source,
                HuddleDeviceKind::Speaker => &session.speaker_sink,
                HuddleDeviceKind::Camera => &session.camera_source,
            };
            let camera_was_enabled =
                kind == HuddleDeviceKind::Camera && session.controls.camera_enabled;
            if kind == HuddleDeviceKind::Camera {
                set_camera_capture(&session.camera_source, &session.camera_valve, false)?;
            }
            let result = self
                .catalog
                .as_ref()
                .ok_or(MediaError::DeviceUnavailable)?
                .reconfigure_element(kind, id, element);
            if kind == HuddleDeviceKind::Camera && camera_was_enabled {
                set_camera_capture(&session.camera_source, &session.camera_valve, true)?;
            }
            result
        }

        fn attach_screen_share(
            &mut self,
            remote_fd: OwnedFd,
            node_id: u32,
        ) -> Result<(), MediaError> {
            self.session()?.event_mailbox.ensure_open()?;
            #[cfg(not(feature = "screen-share"))]
            {
                let _ = (remote_fd, node_id);
                Err(MediaError::ComponentsUnavailable)
            }
            #[cfg(feature = "screen-share")]
            {
                if node_id == 0 {
                    return Err(MediaError::InvalidSessionData);
                }
                let session = self.session_mut()?;
                if session.screen_share.is_some() {
                    return Err(MediaError::AlreadyRunning);
                }
                let source = make("pipewiresrc", "huddle-screen-source")?;
                source.set_property("fd", remote_fd.as_raw_fd());
                source.set_property("target-object", node_id.to_string());
                let queue = make("queue", "huddle-screen-source-queue")?;
                let convert = make("videoconvert", "huddle-screen-convert")?;
                let scale = make("videoscale", "huddle-screen-scale")?;
                let raw_caps = make("capsfilter", "huddle-screen-raw-caps")?;
                raw_caps.set_property(
                    "caps",
                    gst::Caps::builder("video/x-raw")
                        .field("width", 1920i32)
                        .field("height", 1080i32)
                        .field("framerate", gst::Fraction::new(30, 1))
                        .build(),
                );
                let encoder = make("vp8enc", "huddle-screen-vp8-encoder")?;
                encoder.set_property("deadline", 1i64);
                let payloader = make("rtpvp8pay", "huddle-screen-vp8-payloader")?;
                let rtp_caps = make("capsfilter", "huddle-screen-rtp-caps")?;
                rtp_caps.set_property(
                    "caps",
                    gst::Caps::builder("application/x-rtp")
                        .field("media", "video")
                        .field("encoding-name", "VP8")
                        .field("payload", 98i32)
                        .build(),
                );
                let send_queue = make("queue", "huddle-screen-send-queue")?;
                let elements = vec![
                    source, queue, convert, scale, raw_caps, encoder, payloader, rtp_caps,
                    send_queue,
                ];
                add_and_link(&session.pipeline, &elements)?;
                let peer_pad =
                    match link_to_peer(&session.peer, elements.last().expect("screen queue")) {
                        Ok(peer_pad) => peer_pad,
                        Err(error) => {
                            let _ = session.pipeline.remove_many(&elements);
                            return Err(error);
                        }
                    };
                for element in &elements {
                    if element.sync_state_with_parent().is_err() {
                        for element in &elements {
                            let _ = element.set_state(gst::State::Null);
                        }
                        session.peer.release_request_pad(&peer_pad);
                        let _ = session.pipeline.remove_many(&elements);
                        return Err(MediaError::OperationFailed);
                    }
                }
                session.controls.screen_share_enabled = true;
                session.screen_share = Some(ScreenShareBranch {
                    elements,
                    peer_pad,
                    remote_fd,
                });
                Ok(())
            }
        }

        fn detach_screen_share(&mut self) -> Result<(), MediaError> {
            self.session()?.event_mailbox.ensure_open()?;
            #[cfg(not(feature = "screen-share"))]
            {
                self.session()?;
                Err(MediaError::ComponentsUnavailable)
            }
            #[cfg(feature = "screen-share")]
            {
                let session = self.session_mut()?;
                let Some(branch) = session.screen_share.take() else {
                    session.controls.screen_share_enabled = false;
                    return Ok(());
                };
                for element in &branch.elements {
                    let _ = element.set_state(gst::State::Null);
                }
                session.peer.release_request_pad(&branch.peer_pad);
                session
                    .pipeline
                    .remove_many(&branch.elements)
                    .map_err(|_| MediaError::OperationFailed)?;
                session.controls.screen_share_enabled = false;
                drop(branch.remote_fd);
                Ok(())
            }
        }

        fn screen_share_active(&self) -> bool {
            #[cfg(not(feature = "screen-share"))]
            {
                false
            }
            #[cfg(feature = "screen-share")]
            {
                self.session
                    .as_ref()
                    .is_some_and(|session| session.screen_share.is_some())
            }
        }

        fn request_statistics(&mut self) -> Result<(), MediaError> {
            let session = self.session()?;
            session.event_mailbox.ensure_open()?;
            let pending = session.statistics_gate.begin()?;
            let peer = session.peer.clone();
            let event_mailbox = session.event_mailbox.clone();
            let callback_gate = session.callback_gate.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                let _pending = pending;
                let Ok(_callback) = callback_gate.begin() else {
                    return;
                };
                if event_mailbox.ensure_open().is_err() {
                    return;
                }
                let result = reply
                    .ok()
                    .flatten()
                    .map(statistics_from_structure)
                    .ok_or(MediaError::OperationFailed);
                match result {
                    Ok(statistics) => {
                        let _ = event_mailbox.publish(MediaEvent::Statistics(statistics));
                    }
                    Err(error) => {
                        let _ = event_mailbox.publish(MediaEvent::Failed(error));
                    }
                }
            });
            peer.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
            Ok(())
        }

        fn drain_events(&mut self) -> Vec<MediaEvent> {
            self.session
                .as_ref()
                .map_or_else(Vec::new, |session| session.event_mailbox.drain())
        }

        fn stop(&mut self) -> Result<(), MediaError> {
            let Some(session) = self.session.take() else {
                return Ok(());
            };
            session.callback_gate.close_and_wait();
            session.event_mailbox.close_and_clear();
            session.camera_valve.set_property("drop", true);
            session.microphone_valve.set_property("drop", true);
            session.camera_source.set_locked_state(true);
            session
                .pipeline
                .set_state(gst::State::Null)
                .map_err(|_| MediaError::OperationFailed)?;
            Ok(())
        }

        fn is_running(&self) -> bool {
            self.session.is_some()
        }
    }

    impl Drop for GStreamerMediaEngine {
        fn drop(&mut self) {
            if let Some(session) = self.session.take() {
                session.callback_gate.close_and_wait();
                session.event_mailbox.close_and_clear();
                session.camera_valve.set_property("drop", true);
                session.microphone_valve.set_property("drop", true);
                session.camera_source.set_locked_state(true);
                let _ = session.pipeline.set_state(gst::State::Null);
            }
        }
    }

    fn ensure_factories() -> Result<(), MediaError> {
        for factory in [
            "webrtcbin",
            "opusenc",
            "rtpopuspay",
            "vp8enc",
            "rtpvp8pay",
            "decodebin",
            "valve",
        ] {
            if gst::ElementFactory::find(factory).is_none() {
                return Err(MediaError::ComponentsUnavailable);
            }
        }
        Ok(())
    }

    fn make(factory: &str, name: &str) -> Result<gst::Element, MediaError> {
        let element = gst::ElementFactory::make(factory)
            .name(name)
            .build()
            .map_err(|_| MediaError::ComponentsUnavailable)?;
        if factory == "queue" {
            element.set_property("max-size-buffers", 8u32);
            element.set_property("max-size-time", 250_000_000u64);
            element.set_property("max-size-bytes", 0u32);
            element.set_property_from_str("leaky", "downstream");
        }
        Ok(element)
    }

    fn add_and_link(pipeline: &gst::Pipeline, elements: &[gst::Element]) -> Result<(), MediaError> {
        pipeline
            .add_many(elements)
            .map_err(|_| MediaError::OperationFailed)?;
        gst::Element::link_many(elements).map_err(|_| MediaError::OperationFailed)
    }

    fn link_to_peer(peer: &gst::Element, element: &gst::Element) -> Result<gst::Pad, MediaError> {
        let source_pad = element
            .static_pad("src")
            .ok_or(MediaError::OperationFailed)?;
        let sink_pad = peer
            .request_pad_simple("sink_%u")
            .ok_or(MediaError::OperationFailed)?;
        source_pad
            .link(&sink_pad)
            .map_err(|_| MediaError::OperationFailed)?;
        Ok(sink_pad)
    }

    fn set_camera_capture(
        source: &gst::Element,
        valve: &gst::Element,
        enabled: bool,
    ) -> Result<(), MediaError> {
        valve.set_property("drop", !enabled);
        if enabled {
            source.set_locked_state(false);
            source
                .sync_state_with_parent()
                .map_err(|_| MediaError::OperationFailed)
        } else {
            source.set_locked_state(true);
            source
                .set_state(gst::State::Null)
                .map(|_| ())
                .map_err(|_| MediaError::OperationFailed)
        }
    }

    fn connect_ice_events(
        peer: &gst::Element,
        event_mailbox: Arc<MediaEventMailbox>,
        callback_gate: Arc<NativeCallbackGate>,
    ) -> Result<(), MediaError> {
        peer.connect("on-ice-candidate", false, move |values| {
            let Ok(_callback) = callback_gate.begin() else {
                return None;
            };
            if event_mailbox.ensure_open().is_err() {
                return None;
            }
            let index = values.get(1).and_then(|value| value.get::<u32>().ok());
            let candidate = values.get(2).and_then(|value| value.get::<String>().ok());
            match index
                .zip(candidate)
                .ok_or(MediaError::InvalidSessionData)
                .and_then(|(index, candidate)| {
                    let candidate = SensitiveMediaString::new(candidate);
                    IceCandidate::new(index, candidate.expose())
                }) {
                Ok(candidate) => {
                    let _ = event_mailbox.publish(MediaEvent::LocalIceCandidate(candidate));
                }
                Err(error) => {
                    let _ = event_mailbox.publish(MediaEvent::Failed(error));
                }
            }
            None
        });
        Ok(())
    }

    fn connect_incoming_streams(
        pipeline: &gst::Pipeline,
        peer: &gst::Element,
        audio_target: gst::Element,
        video_target: gst::Element,
        event_mailbox: Arc<MediaEventMailbox>,
        branch_admission: Arc<IncomingBranchAdmission>,
        callback_gate: Arc<NativeCallbackGate>,
    ) {
        let pipeline = pipeline.downgrade();
        peer.connect_pad_added(move |_peer, incoming_pad| {
            let Ok(_callback) = callback_gate.begin() else {
                return;
            };
            if event_mailbox.ensure_open().is_err() {
                return;
            }
            let Some(pipeline) = pipeline.upgrade() else {
                return;
            };
            let caps = incoming_pad
                .current_caps()
                .unwrap_or_else(|| incoming_pad.query_caps(None));
            let kind = match branch_admission.claim(caps.as_ref()) {
                Ok(kind) => kind,
                Err(error) => {
                    event_mailbox.seal(error);
                    return;
                }
            };
            let Ok(decodebin) = gst::ElementFactory::make("decodebin").build() else {
                event_mailbox.seal(MediaError::OperationFailed);
                return;
            };
            let target = match kind {
                IncomingMediaKind::Audio => audio_target.clone(),
                IncomingMediaKind::Video => video_target.clone(),
            };
            let decoded_event_mailbox = event_mailbox.clone();
            let decoded_callback_gate = callback_gate.clone();
            decodebin.connect_pad_added(move |_decodebin, decoded_pad| {
                let Ok(_callback) = decoded_callback_gate.begin() else {
                    return;
                };
                if decoded_event_mailbox.ensure_open().is_err() {
                    return;
                }
                let caps = decoded_pad
                    .current_caps()
                    .unwrap_or_else(|| decoded_pad.query_caps(None));
                let expected_prefix = match kind {
                    IncomingMediaKind::Audio => "audio/",
                    IncomingMediaKind::Video => "video/",
                };
                if !caps
                    .structure(0)
                    .is_some_and(|structure| structure.name().starts_with(expected_prefix))
                {
                    decoded_event_mailbox.seal(MediaError::IncomingMediaRejected);
                    return;
                }
                let Some(sink_pad) = target.static_pad("sink") else {
                    decoded_event_mailbox.seal(MediaError::OperationFailed);
                    return;
                };
                if sink_pad.is_linked() {
                    decoded_event_mailbox.seal(MediaError::IncomingMediaRejected);
                    return;
                }
                if decoded_pad.link(&sink_pad).is_err() {
                    decoded_event_mailbox.seal(MediaError::OperationFailed);
                }
            });
            if pipeline.add(&decodebin).is_err() || decodebin.sync_state_with_parent().is_err() {
                event_mailbox.seal(MediaError::OperationFailed);
                return;
            }
            let Some(sink_pad) = decodebin.static_pad("sink") else {
                event_mailbox.seal(MediaError::OperationFailed);
                return;
            };
            if incoming_pad.link(&sink_pad).is_err() {
                event_mailbox.seal(MediaError::OperationFailed);
            }
        });
    }

    fn statistics_from_structure(
        reply: &gst::StructureRef,
    ) -> crate::huddles::state::HuddleSessionStatistics {
        let mut sample = MediaStatisticsSample::default();
        accumulate_statistics(reply, &mut sample, None);
        sample.into_session_statistics()
    }

    fn accumulate_statistics(
        structure: &gst::StructureRef,
        sample: &mut MediaStatisticsSample,
        inherited_media: Option<&str>,
    ) {
        let media = structure
            .get_optional::<&str>("media-type")
            .ok()
            .flatten()
            .or_else(|| structure.get_optional::<&str>("kind").ok().flatten())
            .or(inherited_media);
        for (name, value) in structure.iter() {
            let name = name.as_str();
            match name {
                "round-trip-time" | "current-round-trip-time" => {
                    sample.round_trip_seconds = max_float(sample.round_trip_seconds, number(value));
                }
                "jitter" => {
                    sample.jitter_seconds = max_float(sample.jitter_seconds, number(value));
                }
                "packets-lost" => {
                    let value = signed_number(value).unwrap_or_default();
                    sample.packets_lost = Some(
                        sample
                            .packets_lost
                            .unwrap_or_default()
                            .saturating_add(value),
                    );
                }
                "packets-received" => {
                    let value = unsigned_number(value).unwrap_or_default();
                    sample.packets_received = Some(
                        sample
                            .packets_received
                            .unwrap_or_default()
                            .saturating_add(value),
                    );
                }
                "bitrate" | "bitrate-mean" => {
                    let target = if media.is_some_and(|media| media.contains("video")) {
                        &mut sample.video_bitrate_bps
                    } else {
                        &mut sample.audio_bitrate_bps
                    };
                    *target = Some(target.unwrap_or_default() + number(value).unwrap_or_default());
                }
                _ => {}
            }
            if let Ok(nested) = value.get::<gst::Structure>() {
                accumulate_statistics(nested.as_ref(), sample, media);
            }
        }
    }

    fn number(value: &glib::SendValue) -> Option<f64> {
        value
            .get::<f64>()
            .ok()
            .or_else(|| value.get::<f32>().ok().map(f64::from))
            .or_else(|| value.get::<u64>().ok().map(|value| value as f64))
            .or_else(|| value.get::<u32>().ok().map(f64::from))
            .or_else(|| value.get::<i64>().ok().map(|value| value as f64))
            .or_else(|| value.get::<i32>().ok().map(f64::from))
    }

    fn signed_number(value: &glib::SendValue) -> Option<i64> {
        value
            .get::<i64>()
            .ok()
            .or_else(|| value.get::<i32>().ok().map(i64::from))
            .or_else(|| {
                value
                    .get::<u64>()
                    .ok()
                    .map(|value| value.min(i64::MAX as u64) as i64)
            })
    }

    fn unsigned_number(value: &glib::SendValue) -> Option<u64> {
        value
            .get::<u64>()
            .ok()
            .or_else(|| value.get::<u32>().ok().map(u64::from))
            .or_else(|| value.get::<i64>().ok().map(|value| value.max(0) as u64))
    }

    fn max_float(current: Option<f64>, next: Option<f64>) -> Option<f64> {
        match (current, next) {
            (Some(current), Some(next)) => Some(current.max(next)),
            (current, next) => current.or(next),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_native_queue_uses_bounded_downstream_leaky_limits() {
            gst::init().unwrap();
            let queue = make("queue", "bounded-queue-test").unwrap();

            assert_eq!(queue.property::<u32>("max-size-buffers"), 8);
            assert_eq!(queue.property::<u64>("max-size-time"), 250_000_000);
            assert_eq!(queue.property::<u32>("max-size-bytes"), 0);
            let leaky = queue.property_value("leaky");
            let (_, leaky) = glib::EnumValue::from_value(&leaky).unwrap();
            assert_eq!(leaky.nick(), "downstream");
        }

        #[test]
        fn incoming_rtp_admission_rejects_duplicate_and_unknown_media() {
            gst::init().unwrap();
            let admission = IncomingBranchAdmission::default();
            let audio = gst::Caps::builder("application/x-rtp")
                .field("media", "audio")
                .build();
            let video = gst::Caps::builder("application/x-rtp")
                .field("media", "video")
                .build();
            let unknown = gst::Caps::builder("application/x-rtp")
                .field("media", "data")
                .build();

            assert_eq!(
                admission.claim(audio.as_ref()),
                Ok(IncomingMediaKind::Audio)
            );
            assert_eq!(
                admission.claim(video.as_ref()),
                Ok(IncomingMediaKind::Video)
            );
            assert_eq!(
                admission.claim(audio.as_ref()),
                Err(MediaError::IncomingMediaRejected)
            );
            assert_eq!(
                IncomingBranchAdmission::default().claim(unknown.as_ref()),
                Err(MediaError::IncomingMediaRejected)
            );
        }

        #[test]
        fn native_callback_gate_rejects_callbacks_after_close() {
            let gate = Arc::new(NativeCallbackGate::default());
            let callback = gate.begin().unwrap();
            drop(callback);
            gate.close_and_wait();
            assert!(matches!(gate.begin(), Err(MediaError::GenerationClosed)));
        }
    }
}

#[cfg(feature = "native-media")]
#[allow(unused_imports)]
pub use native::GStreamerMediaEngine;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huddles::state::HuddleControls;

    fn statistics(packets_received: u64) -> HuddleSessionStatistics {
        HuddleSessionStatistics {
            packets_received,
            ..Default::default()
        }
    }

    fn assert_sensitive_value_drops_after_mailbox_unlock(
        mailbox: &Arc<MediaEventMailbox>,
        action: impl FnOnce(&MediaEventMailbox),
    ) {
        let all_unlocked = Arc::new(AtomicBool::new(true));
        let drop_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_mailbox = Arc::downgrade(mailbox);
        let observed_all_unlocked = all_unlocked.clone();
        let observed_drop_count = drop_count.clone();
        set_sensitive_media_value_drop_hook(Some(Box::new(move || {
            let unlocked = observed_mailbox
                .upgrade()
                .is_some_and(|mailbox| mailbox.state.try_lock().map(drop).is_ok());
            observed_all_unlocked.fetch_and(unlocked, Ordering::Relaxed);
            observed_drop_count.fetch_add(1, Ordering::Relaxed);
        })));

        action(mailbox.as_ref());
        set_sensitive_media_value_drop_hook(None);

        assert!(all_unlocked.load(Ordering::Relaxed));
        assert_eq!(drop_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn descriptions_and_candidates_are_redacted_from_debug_output() {
        let description = MediaDescription::offer("v=0\r\na=ice-ufrag:private\r\n").unwrap();
        let candidate = IceCandidate::new(0, "candidate:private-address").unwrap();

        let debug = format!("{description:?} {candidate:?}");
        assert!(!debug.contains("private"));
        assert!(debug.contains("<redacted>"));
        assert_eq!(description.sdp().unwrap(), "v=0\r\na=ice-ufrag:private\r\n");
        assert_eq!(candidate.value().unwrap(), "candidate:private-address");
    }

    #[test]
    fn sensitive_media_wipe_overwrites_every_byte() {
        let mut bytes = *b"private-media";
        volatile_zeroize(&mut bytes);
        assert_eq!(bytes, [0; 13]);
    }

    #[test]
    fn mailbox_discard_wipes_sensitive_values_after_unlock() {
        let closed = Arc::new(MediaEventMailbox::default());
        closed
            .publish(MediaEvent::LocalDescription(
                MediaDescription::offer("v=0\r\n").unwrap(),
            ))
            .unwrap();
        assert_sensitive_value_drops_after_mailbox_unlock(&closed, |mailbox| {
            mailbox.close_and_clear();
        });

        let sealed = Arc::new(MediaEventMailbox::default());
        sealed
            .publish(MediaEvent::LocalIceCandidate(
                IceCandidate::new(0, "candidate:seal").unwrap(),
            ))
            .unwrap();
        assert_sensitive_value_drops_after_mailbox_unlock(&sealed, |mailbox| {
            mailbox.seal(MediaError::OperationFailed);
        });
        assert!(matches!(
            sealed.drain().as_slice(),
            [MediaEvent::Failed(MediaError::OperationFailed)]
        ));

        let saturated = Arc::new(MediaEventMailbox::default());
        saturated
            .publish(MediaEvent::LocalDescription(
                MediaDescription::offer("v=0\r\n").unwrap(),
            ))
            .unwrap();
        for _ in 1..MEDIA_EVENT_CAPACITY {
            saturated
                .publish(MediaEvent::Failed(MediaError::OperationFailed))
                .unwrap();
        }
        assert_sensitive_value_drops_after_mailbox_unlock(&saturated, |mailbox| {
            assert_eq!(
                mailbox.publish(MediaEvent::Failed(MediaError::OperationFailed)),
                Err(MediaError::AdmissionSaturated)
            );
        });
        assert!(matches!(
            saturated.drain().as_slice(),
            [MediaEvent::Failed(MediaError::AdmissionSaturated)]
        ));
    }

    #[test]
    fn media_payload_limits_are_checked_before_sensitive_values_are_copied() {
        let mut maximum_sdp = String::from("v=0");
        maximum_sdp.push_str(&"s".repeat(MAX_SESSION_DESCRIPTION_BYTES - maximum_sdp.len()));
        assert!(MediaDescription::offer(&maximum_sdp).is_ok());
        maximum_sdp.push('s');
        assert!(matches!(
            MediaDescription::offer(&maximum_sdp),
            Err(MediaError::PayloadTooLarge)
        ));

        let mut maximum_candidate = String::from("candidate:");
        maximum_candidate.push_str(&"c".repeat(MAX_ICE_CANDIDATE_BYTES - maximum_candidate.len()));
        assert!(IceCandidate::new(0, &maximum_candidate).is_ok());
        maximum_candidate.push('c');
        assert!(matches!(
            IceCandidate::new(0, &maximum_candidate),
            Err(MediaError::PayloadTooLarge)
        ));
        let padded_candidate =
            format!("{}candidate:short", " ".repeat(MAX_ICE_CANDIDATE_BYTES + 1));
        assert!(matches!(
            IceCandidate::new(0, &padded_candidate),
            Err(MediaError::PayloadTooLarge)
        ));
    }

    #[test]
    fn media_mailbox_preserves_reliable_fifo_and_coalesces_statistics_in_place() {
        let mailbox = MediaEventMailbox::default();
        mailbox
            .publish(MediaEvent::LocalDescription(
                MediaDescription::offer("v=0\r\n").unwrap(),
            ))
            .unwrap();
        mailbox
            .publish(MediaEvent::Statistics(statistics(1)))
            .unwrap();
        mailbox
            .publish(MediaEvent::LocalIceCandidate(
                IceCandidate::new(0, "candidate:first").unwrap(),
            ))
            .unwrap();
        mailbox
            .publish(MediaEvent::Statistics(statistics(2)))
            .unwrap();
        mailbox
            .publish(MediaEvent::Failed(MediaError::OperationFailed))
            .unwrap();

        let snapshot = mailbox.snapshot();
        assert_eq!(snapshot.admitted_reliable, 3);
        assert_eq!(snapshot.admitted_statistics, 2);
        assert_eq!(snapshot.coalesced_statistics, 1);
        assert_eq!(snapshot.depth, 4);
        assert_eq!(snapshot.peak_depth, 4);
        let events = mailbox.drain();
        assert!(matches!(
            events.first(),
            Some(MediaEvent::LocalDescription(_))
        ));
        assert!(matches!(
            events.get(1),
            Some(MediaEvent::Statistics(statistics)) if statistics.packets_received == 2
        ));
        assert!(matches!(
            events.get(2),
            Some(MediaEvent::LocalIceCandidate(candidate))
                if candidate.value().unwrap() == "candidate:first"
        ));
        assert!(matches!(
            events.get(3),
            Some(MediaEvent::Failed(MediaError::OperationFailed))
        ));
        assert_eq!(events.len(), 4);
        let snapshot = mailbox.snapshot();
        assert_eq!(snapshot.dequeued, 4);
        assert_eq!(snapshot.depth, 0);
    }

    #[test]
    fn reliable_media_events_evict_statistics_before_saturating() {
        let mailbox = MediaEventMailbox::default();
        mailbox
            .publish(MediaEvent::Statistics(statistics(1)))
            .unwrap();
        for _ in 0..MEDIA_EVENT_CAPACITY - 1 {
            mailbox
                .publish(MediaEvent::Failed(MediaError::OperationFailed))
                .unwrap();
        }
        mailbox
            .publish(MediaEvent::LocalDescription(
                MediaDescription::offer("v=0\r\n").unwrap(),
            ))
            .unwrap();

        assert_eq!(mailbox.depth(), MEDIA_EVENT_CAPACITY);
        let snapshot = mailbox.snapshot();
        assert_eq!(snapshot.evicted_statistics, 1);
        assert_eq!(snapshot.peak_depth, MEDIA_EVENT_CAPACITY);
        let events = mailbox.drain();
        assert_eq!(events.len(), MEDIA_EVENT_CAPACITY);
        assert!(!events
            .iter()
            .any(|event| matches!(event, MediaEvent::Statistics(_))));
        assert!(matches!(
            events.last(),
            Some(MediaEvent::LocalDescription(_))
        ));
    }

    #[test]
    fn reliable_media_saturation_emits_one_terminal_and_closes_generation() {
        let mailbox = MediaEventMailbox::default();
        for _ in 0..MEDIA_EVENT_CAPACITY {
            mailbox
                .publish(MediaEvent::Failed(MediaError::OperationFailed))
                .unwrap();
            assert!(mailbox.depth() <= MEDIA_EVENT_CAPACITY);
        }
        mailbox
            .publish(MediaEvent::Statistics(statistics(7)))
            .unwrap();
        assert_eq!(mailbox.depth(), MEDIA_EVENT_CAPACITY);

        assert_eq!(
            mailbox.publish(MediaEvent::Failed(MediaError::OperationFailed)),
            Err(MediaError::AdmissionSaturated)
        );
        assert_eq!(mailbox.depth(), 1);
        assert_eq!(
            mailbox.publish(MediaEvent::Statistics(statistics(9))),
            Err(MediaError::GenerationClosed)
        );
        let snapshot = mailbox.snapshot();
        assert_eq!(snapshot.sealed, 1);
        assert_eq!(snapshot.discarded_on_seal, MEDIA_EVENT_CAPACITY as u64);
        assert_eq!(snapshot.dropped_statistics, 1);
        assert_eq!(snapshot.rejected_closed, 1);
        assert_eq!(snapshot.depth, 1);
        assert_eq!(snapshot.peak_depth, MEDIA_EVENT_CAPACITY);
        assert!(matches!(
            mailbox.drain().as_slice(),
            [MediaEvent::Failed(MediaError::AdmissionSaturated)]
        ));
        assert!(mailbox.drain().is_empty());
    }

    #[test]
    fn operation_gate_reopens_after_callback_guard_drops() {
        let gate = OperationGate::default();
        let pending = gate.begin().unwrap();
        assert!(matches!(gate.begin(), Err(MediaError::OperationPending)));
        drop(pending);
        assert!(gate.begin().is_ok());

        let gate = OperationGate::default();
        let mailbox = MediaEventMailbox::default();
        let pending = gate.begin().unwrap();
        mailbox.close_and_clear();
        assert_eq!(
            mailbox.publish(MediaEvent::Statistics(statistics(1))),
            Err(MediaError::GenerationClosed)
        );
        drop(pending);
        assert!(gate.begin().is_ok());
    }

    #[test]
    fn stopped_generation_cannot_publish_into_restarted_synthetic_session() {
        let mut engine = SyntheticMediaEngine::default();
        engine.start(MediaSessionConfig::default()).unwrap();
        let stopped_mailbox = engine.event_mailbox.as_ref().unwrap().clone();
        engine.stop().unwrap();
        engine.start(MediaSessionConfig::default()).unwrap();

        assert_eq!(
            stopped_mailbox.publish(MediaEvent::Failed(MediaError::OperationFailed)),
            Err(MediaError::GenerationClosed)
        );
        engine.create_offer().unwrap();
        assert!(matches!(
            engine.drain_events().as_slice(),
            [MediaEvent::LocalDescription(_)]
        ));
    }

    #[test]
    fn synthetic_remote_ice_limit_seals_generation_and_stop_is_idempotent() {
        let mut engine = SyntheticMediaEngine::default();
        engine.stop().unwrap();
        engine.start(MediaSessionConfig::default()).unwrap();
        for index in 0..MAX_REMOTE_ICE_CANDIDATES {
            engine
                .add_remote_ice_candidate(
                    IceCandidate::new(index as u32, "candidate:remote").unwrap(),
                )
                .unwrap();
        }
        assert_eq!(
            engine.add_remote_ice_candidate(IceCandidate::new(0, "candidate:overflow").unwrap()),
            Err(MediaError::RemoteIceLimitExceeded)
        );
        assert!(matches!(
            engine.drain_events().as_slice(),
            [MediaEvent::Failed(MediaError::RemoteIceLimitExceeded)]
        ));
        assert_eq!(engine.create_offer(), Err(MediaError::GenerationClosed));
        assert_eq!(
            engine.set_remote_description(MediaDescription::answer("v=0\r\n").unwrap()),
            Err(MediaError::GenerationClosed)
        );
        assert_eq!(
            engine.add_remote_ice_candidate(IceCandidate::new(0, "candidate:late").unwrap()),
            Err(MediaError::GenerationClosed)
        );
        assert_eq!(
            engine.apply_controls(HuddleControls::default()),
            Err(MediaError::GenerationClosed)
        );
        assert_eq!(
            engine.select_device(HuddleDeviceKind::Camera, "camera:late"),
            Err(MediaError::GenerationClosed)
        );
        let (remote, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(
            engine.attach_screen_share(remote.into(), 42),
            Err(MediaError::GenerationClosed)
        );
        assert_eq!(
            engine.detach_screen_share(),
            Err(MediaError::GenerationClosed)
        );
        assert_eq!(
            engine.request_statistics(),
            Err(MediaError::GenerationClosed)
        );
        engine.stop().unwrap();
        engine.stop().unwrap();
        assert!(!engine.is_running());
    }

    #[test]
    fn synthetic_engine_exercises_negotiation_controls_statistics_and_teardown() {
        let mut engine = SyntheticMediaEngine::default();
        engine
            .start(MediaSessionConfig {
                source_mode: MediaSourceMode::Synthetic,
                sink_mode: MediaSinkMode::Fake,
                ..Default::default()
            })
            .unwrap();

        engine.create_offer().unwrap();
        let events = engine.drain_events();
        assert!(matches!(
            events.as_slice(),
            [MediaEvent::LocalDescription(_)]
        ));

        engine
            .set_remote_description(MediaDescription::answer("v=0\r\n").unwrap())
            .unwrap();
        engine
            .add_remote_ice_candidate(IceCandidate::new(0, "candidate:test").unwrap())
            .unwrap();
        engine
            .apply_controls(HuddleControls {
                microphone_muted: true,
                camera_enabled: true,
                screen_share_enabled: false,
            })
            .unwrap();
        engine
            .select_device(HuddleDeviceKind::Camera, "camera:synthetic")
            .unwrap();
        let (remote, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        engine.attach_screen_share(remote.into(), 42).unwrap();
        assert!(engine.screen_share_active());
        engine.detach_screen_share().unwrap();
        assert!(!engine.screen_share_active());
        engine.request_statistics().unwrap();

        assert!(matches!(
            engine.drain_events().as_slice(),
            [MediaEvent::Statistics(_)]
        ));
        assert!(engine.is_running());
        engine.stop().unwrap();
        assert!(!engine.is_running());
    }

    #[test]
    fn media_statistics_saturate_untrusted_floating_point_values() {
        let statistics = MediaStatisticsSample {
            round_trip_seconds: Some(0.025),
            jitter_seconds: Some(0.004),
            packets_lost: Some(-7),
            packets_received: Some(u64::MAX),
            audio_bitrate_bps: Some(f64::INFINITY),
            video_bitrate_bps: Some(2_500_000.8),
        }
        .into_session_statistics();

        assert_eq!(statistics.round_trip_ms, 25);
        assert_eq!(statistics.jitter_ms, 4);
        assert_eq!(statistics.packets_lost, 0);
        assert_eq!(statistics.packets_received, u64::MAX);
        assert_eq!(statistics.audio_bitrate_bps, 0);
        assert_eq!(statistics.video_bitrate_bps, 2_500_000);
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_synthetic_pipeline_keeps_camera_off_until_explicitly_enabled() {
        let mut engine = GStreamerMediaEngine::new().unwrap();
        assert!(engine.devices().is_empty());
        engine
            .start(MediaSessionConfig {
                source_mode: MediaSourceMode::Synthetic,
                sink_mode: MediaSinkMode::Fake,
                ..Default::default()
            })
            .unwrap();

        assert!(!engine.camera_capture_active());
        engine
            .apply_controls(HuddleControls {
                microphone_muted: true,
                camera_enabled: true,
                screen_share_enabled: false,
            })
            .unwrap();
        assert!(engine.camera_capture_active());

        engine
            .apply_controls(HuddleControls {
                microphone_muted: true,
                camera_enabled: false,
                screen_share_enabled: false,
            })
            .unwrap();
        assert!(!engine.camera_capture_active());
        engine.stop().unwrap();
    }
}
