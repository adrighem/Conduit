// This developer harness is exercised directly by tests and feature builds.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use crate::huddles::coordinator::{
    CoordinatorInput, HuddleCoordinator, HuddleEffect, HuddleTransitionError,
};
use crate::huddles::media::{
    IceCandidate, MediaDescription, MediaEngine, MediaError, MediaSessionConfig, MediaSinkMode,
    MediaSourceMode, SyntheticMediaEngine,
};
use crate::huddles::model::ActiveHuddle;
use crate::huddles::portal::{
    request_screen_cast, PortalError, ScreenCastLease, SyntheticScreenCastBackend,
};
use crate::huddles::signaling::{
    ChimeBridgeCapability, ChimeMediaBridge, NativeJoinGate, SignalingError,
    SlackBootstrapCapability, SlackHuddleBootstrap, SlackJoinSession,
};
use crate::huddles::state::{HuddleFailure, HuddlePhase, HuddleSessionStatistics, HuddleSnapshot};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyntheticHuddleTrace {
    pub bootstrap_joins: usize,
    pub bootstrap_leaves: usize,
    pub bridge_connects: usize,
    pub bridge_disconnects: usize,
    pub control_updates: usize,
    pub reconnects: usize,
    pub media_starts: usize,
    pub media_stops: usize,
    pub screen_share_starts: usize,
    pub screen_share_stops: usize,
    #[cfg(test)]
    portal_close_attempts: usize,
    #[cfg(test)]
    lifecycle: Vec<SyntheticLifecycleStep>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyntheticLifecycleStep {
    ScreenShareDetached,
    PortalClosed,
    MediaStopped,
    BridgeDisconnected,
    BootstrapReleased,
    CoordinatorFinalized,
}

#[derive(Debug, thiserror::Error)]
pub enum SyntheticHarnessError {
    #[error(transparent)]
    Transition(#[from] HuddleTransitionError),
    #[error(transparent)]
    Signaling(#[from] SignalingError),
    #[error(transparent)]
    Media(#[from] MediaError),
    #[error(transparent)]
    Portal(#[from] PortalError),
    #[error(transparent)]
    Runtime(#[from] std::io::Error),
    #[error("the synthetic coordinator did not request a native join")]
    JoinNotRequested,
}

pub struct SyntheticHuddleHarness {
    coordinator: HuddleCoordinator,
    gate: NativeJoinGate<SyntheticSlackBootstrap, SyntheticChimeBridge>,
    media: SyntheticMediaEngine,
    screen_cast: Option<ScreenCastLease<SyntheticScreenCastBackend>>,
    screen_cast_backend: Arc<SyntheticScreenCastBackend>,
    trace: Arc<Mutex<SyntheticHuddleTrace>>,
    #[cfg(test)]
    fail_join_after_media_start: bool,
    #[cfg(test)]
    fail_detach_once: bool,
    #[cfg(test)]
    fail_media_stop_once: bool,
    #[cfg(test)]
    fail_portal_close_attempts: usize,
    #[cfg(test)]
    fail_bridge_disconnect_once: Arc<AtomicBool>,
    #[cfg(test)]
    fail_bootstrap_leave_once: Arc<AtomicBool>,
}

impl SyntheticHuddleHarness {
    pub fn new() -> Self {
        let trace = Arc::new(Mutex::new(SyntheticHuddleTrace::default()));
        #[cfg(test)]
        let fail_bridge_disconnect_once = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let fail_bootstrap_leave_once = Arc::new(AtomicBool::new(false));
        Self {
            coordinator: HuddleCoordinator::default(),
            gate: NativeJoinGate::new(
                SyntheticSlackBootstrap {
                    trace: Arc::clone(&trace),
                    #[cfg(test)]
                    fail_leave_once: Arc::clone(&fail_bootstrap_leave_once),
                },
                SyntheticChimeBridge {
                    trace: Arc::clone(&trace),
                    #[cfg(test)]
                    fail_disconnect_once: Arc::clone(&fail_bridge_disconnect_once),
                },
            ),
            media: SyntheticMediaEngine::default(),
            screen_cast: None,
            screen_cast_backend: Arc::new(SyntheticScreenCastBackend),
            trace,
            #[cfg(test)]
            fail_join_after_media_start: false,
            #[cfg(test)]
            fail_detach_once: false,
            #[cfg(test)]
            fail_media_stop_once: false,
            #[cfg(test)]
            fail_portal_close_attempts: 0,
            #[cfg(test)]
            fail_bridge_disconnect_once,
            #[cfg(test)]
            fail_bootstrap_leave_once,
        }
    }

    pub fn snapshot(&self) -> &HuddleSnapshot {
        self.coordinator.snapshot()
    }

    pub fn trace(&self) -> SyntheticHuddleTrace {
        self.trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .clone()
    }

    pub fn join(&mut self, huddle: ActiveHuddle) -> Result<(), SyntheticHarnessError> {
        let call_id = huddle.call_id.clone();
        self.coordinator
            .apply(CoordinatorInput::HuddleDiscovered(huddle))?;
        self.coordinator.apply(CoordinatorInput::OpenPreflight {
            call_id: call_id.clone(),
        })?;
        let effects = self
            .coordinator
            .apply(CoordinatorInput::JoinRequested { call_id })?;
        let huddle = effects.into_iter().find_map(|effect| match effect {
            HuddleEffect::BeginNativeJoin { huddle, .. } => Some(huddle),
            _ => None,
        });
        let huddle = huddle.ok_or(SyntheticHarnessError::JoinNotRequested)?;
        if let Err(error) = self.gate.begin_join(&huddle) {
            if error != SignalingError::AlreadyConnected {
                let _ = self.gate.stop();
            }
            let _ = self
                .coordinator
                .apply(CoordinatorInput::Failed(HuddleFailure::media()));
            return Err(error.into());
        }
        if let Err(error) = self.media.start(MediaSessionConfig {
            source_mode: MediaSourceMode::Synthetic,
            sink_mode: MediaSinkMode::Fake,
            ..Default::default()
        }) {
            self.rollback_join(false, true);
            return Err(error.into());
        }
        self.trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .media_starts += 1;

        let setup = (|| -> Result<(), SyntheticHarnessError> {
            #[cfg(test)]
            if std::mem::take(&mut self.fail_join_after_media_start) {
                return Err(SyntheticHarnessError::JoinNotRequested);
            }
            self.media.create_offer()?;
            if !self.media.drain_events().iter().any(|event| {
                matches!(
                    event,
                    crate::huddles::media::MediaEvent::LocalDescription(_)
                )
            }) {
                return Err(SyntheticHarnessError::JoinNotRequested);
            }
            self.media
                .set_remote_description(MediaDescription::answer("v=0\r\n")?)?;
            self.media
                .add_remote_ice_candidate(IceCandidate::new(0, "candidate:synthetic")?)?;
            Ok(())
        })();
        if let Err(error) = setup {
            self.rollback_join(true, true);
            return Err(error);
        }
        if let Err(error) = self.coordinator.apply(CoordinatorInput::MediaConnected) {
            self.rollback_join(true, true);
            return Err(error.into());
        }
        Ok(())
    }

    pub fn set_muted(&mut self, muted: bool) -> Result<(), SyntheticHarnessError> {
        let effects = self
            .coordinator
            .apply(CoordinatorInput::MutedChanged(muted))?;
        self.apply_control_effects(&effects)
    }

    pub fn set_camera_enabled(&mut self, enabled: bool) -> Result<(), SyntheticHarnessError> {
        let effects = self
            .coordinator
            .apply(CoordinatorInput::CameraChanged(enabled))?;
        self.apply_control_effects(&effects)
    }

    pub fn start_screen_share(&mut self) -> Result<(), SyntheticHarnessError> {
        if self.screen_cast.is_some() && !self.media.screen_share_active() {
            self.stop_screen_share_resources()?;
        }
        let effects = self
            .coordinator
            .apply(CoordinatorInput::ScreenShareChanged(true))?;
        if !effects.contains(&HuddleEffect::StartScreenShare) {
            return Ok(());
        }
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = self.coordinator.apply(CoordinatorInput::ScreenShareFailed(
                    HuddleFailure::internal(),
                ));
                return Err(error.into());
            }
        };
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let lease = match runtime.block_on(request_screen_cast(
            Arc::clone(&self.screen_cast_backend),
            None,
            receiver,
        )) {
            Ok(lease) => lease,
            Err(error) => {
                let (primary, _cleanup, pending_lease) = error.into_parts();
                if let Some(pending_lease) = pending_lease {
                    debug_assert!(self.screen_cast.is_none());
                    self.screen_cast = Some(pending_lease);
                }
                let _ = self
                    .coordinator
                    .apply(CoordinatorInput::ScreenShareFailed(HuddleFailure::media()));
                return Err(primary.into());
            }
        };
        self.screen_cast = Some(lease);
        let remote = self
            .screen_cast
            .as_mut()
            .expect("stored screen cast lease")
            .take_remote();
        let (remote_fd, node_id) = match remote {
            Ok(remote) => remote,
            Err(error) => {
                let _ = self.stop_screen_share_resources();
                let _ = self
                    .coordinator
                    .apply(CoordinatorInput::ScreenShareFailed(HuddleFailure::media()));
                return Err(error.into());
            }
        };
        if let Err(error) = self.media.attach_screen_share(remote_fd, node_id) {
            let _ = self.stop_screen_share_resources();
            let _ = self
                .coordinator
                .apply(CoordinatorInput::ScreenShareFailed(HuddleFailure::media()));
            return Err(error.into());
        }
        if let Err(error) = self.coordinator.apply(CoordinatorInput::ScreenShareStarted) {
            let _ = self.stop_screen_share_resources();
            let _ = self
                .coordinator
                .apply(CoordinatorInput::ScreenShareFailed(HuddleFailure::media()));
            return Err(error.into());
        }
        self.trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .screen_share_starts += 1;
        Ok(())
    }

    pub fn stop_screen_share(&mut self) -> Result<(), SyntheticHarnessError> {
        let effects = self
            .coordinator
            .apply(CoordinatorInput::ScreenShareChanged(false))?;
        if effects.contains(&HuddleEffect::StopScreenShare) {
            if let Err(error) = self.stop_screen_share_resources() {
                let _ = self
                    .coordinator
                    .apply(CoordinatorInput::Failed(HuddleFailure::media()));
                let _ = self.teardown_session();
                return Err(error);
            }
            self.coordinator
                .apply(CoordinatorInput::ScreenShareStopped)?;
        }
        Ok(())
    }

    pub fn reconnect(&mut self) -> Result<(), SyntheticHarnessError> {
        let effects = self.coordinator.apply(CoordinatorInput::ConnectionLost)?;
        let mut first_error = None;
        record_first_error(&mut first_error, self.apply_control_effects(&effects));
        if effects.contains(&HuddleEffect::StopScreenShare) {
            record_first_error(&mut first_error, self.stop_screen_share_resources());
        }
        if let Some(error) = first_error {
            let _ = self
                .coordinator
                .apply(CoordinatorInput::Failed(HuddleFailure::media()));
            let _ = self.teardown_session();
            return Err(error);
        }
        self.trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .reconnects += 1;
        if let Err(error) = self.coordinator.apply(CoordinatorInput::MediaReconnected) {
            let original = SyntheticHarnessError::Transition(error);
            let _ = self
                .coordinator
                .apply(CoordinatorInput::Failed(HuddleFailure::internal()));
            let _ = self.teardown_session();
            return Err(original);
        }
        Ok(())
    }

    pub fn update_statistics(
        &mut self,
        statistics: HuddleSessionStatistics,
    ) -> Result<(), SyntheticHarnessError> {
        self.coordinator
            .apply(CoordinatorInput::StatisticsUpdated(statistics))?;
        Ok(())
    }

    pub fn leave(&mut self) -> Result<(), SyntheticHarnessError> {
        if !matches!(
            self.snapshot().phase,
            HuddlePhase::Idle | HuddlePhase::Leaving
        ) {
            let effects = self.coordinator.apply(CoordinatorInput::LeaveRequested)?;
            if !effects.contains(&HuddleEffect::StopSession) {
                return Ok(());
            }
        }
        self.teardown_session()
    }

    fn rollback_join(&mut self, media_started: bool, gate_joined: bool) {
        if media_started {
            let was_running = self.media.is_running();
            if self.stop_media().is_ok() && was_running {
                self.trace
                    .lock()
                    .expect("synthetic huddle trace lock poisoned")
                    .media_stops += 1;
            }
        }
        if gate_joined {
            let _ = self.gate.stop();
        }
        let _ = self
            .coordinator
            .apply(CoordinatorInput::Failed(HuddleFailure::media()));
    }

    fn teardown_session(&mut self) -> Result<(), SyntheticHarnessError> {
        let mut first_error = None;
        record_first_error(&mut first_error, self.stop_screen_share_resources());

        let media_was_running = self.media.is_running();
        let media_stop = self.stop_media().map_err(SyntheticHarnessError::from);
        let media_stopped = !self.media.is_running();
        if media_stopped && media_was_running {
            let mut trace = self
                .trace
                .lock()
                .expect("synthetic huddle trace lock poisoned");
            trace.media_stops += 1;
            #[cfg(test)]
            trace.lifecycle.push(SyntheticLifecycleStep::MediaStopped);
        }
        record_first_error(&mut first_error, media_stop);
        if !media_stopped {
            record_first_error(
                &mut first_error,
                Err(SyntheticHarnessError::Media(MediaError::OperationFailed)),
            );
        } else {
            record_first_error(&mut first_error, self.close_screen_cast_portal());
        }
        record_first_error(
            &mut first_error,
            self.gate.stop().map_err(SyntheticHarnessError::from),
        );

        if media_stopped {
            #[cfg(test)]
            let coordinator_was_pending = self.snapshot().phase != HuddlePhase::Idle;
            let coordinator_result = match self.snapshot().phase {
                HuddlePhase::Leaving => self.coordinator.apply(CoordinatorInput::MediaStopped),
                HuddlePhase::Idle => Ok(Vec::new()),
                _ => self.coordinator.apply(CoordinatorInput::Reset),
            }
            .map(|_| ())
            .map_err(SyntheticHarnessError::from);
            #[cfg(test)]
            if coordinator_was_pending
                && coordinator_result.is_ok()
                && self.snapshot().phase == HuddlePhase::Idle
            {
                self.trace
                    .lock()
                    .expect("synthetic huddle trace lock poisoned")
                    .lifecycle
                    .push(SyntheticLifecycleStep::CoordinatorFinalized);
            }
            record_first_error(&mut first_error, coordinator_result);
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn apply_control_effects(
        &mut self,
        effects: &[HuddleEffect],
    ) -> Result<(), SyntheticHarnessError> {
        for effect in effects {
            if let HuddleEffect::ApplyControls(controls) = effect {
                self.media.apply_controls(controls.clone())?;
                self.trace
                    .lock()
                    .expect("synthetic huddle trace lock poisoned")
                    .control_updates += 1;
            }
        }
        Ok(())
    }

    fn stop_screen_share_resources(&mut self) -> Result<(), SyntheticHarnessError> {
        let mut first_error = None;
        let mut detached = true;
        if self.media.screen_share_active() {
            let detach = self
                .detach_screen_share()
                .map_err(SyntheticHarnessError::from);
            detached = detach.is_ok();
            if detach.is_ok() {
                let mut trace = self
                    .trace
                    .lock()
                    .expect("synthetic huddle trace lock poisoned");
                trace.screen_share_stops += 1;
                #[cfg(test)]
                trace
                    .lifecycle
                    .push(SyntheticLifecycleStep::ScreenShareDetached);
            }
            record_first_error(&mut first_error, detach);
        }

        if detached {
            record_first_error(&mut first_error, self.close_screen_cast_portal());
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn close_screen_cast_portal(&mut self) -> Result<(), SyntheticHarnessError> {
        #[cfg(test)]
        if self.screen_cast.is_some() {
            self.trace
                .lock()
                .expect("synthetic huddle trace lock poisoned")
                .portal_close_attempts += 1;
            if self.fail_portal_close_attempts > 0 {
                self.fail_portal_close_attempts -= 1;
                return Err(PortalError::OperationFailed.into());
            }
        }

        let mut portal_closed = false;
        if let Some(lease) = self.screen_cast.as_mut() {
            let close = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime
                    .block_on(lease.close())
                    .map_err(SyntheticHarnessError::from),
                Err(error) => Err(SyntheticHarnessError::Runtime(error)),
            };
            portal_closed = close.is_ok();
            close?;
        }
        if portal_closed {
            self.screen_cast = None;
            #[cfg(test)]
            self.trace
                .lock()
                .expect("synthetic huddle trace lock poisoned")
                .lifecycle
                .push(SyntheticLifecycleStep::PortalClosed);
        }
        Ok(())
    }

    fn detach_screen_share(&mut self) -> Result<(), MediaError> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_detach_once) {
            return Err(MediaError::OperationFailed);
        }
        self.media.detach_screen_share()
    }

    fn stop_media(&mut self) -> Result<(), MediaError> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_media_stop_once) {
            return Err(MediaError::OperationFailed);
        }
        self.media.stop()
    }
}

fn record_first_error(
    first_error: &mut Option<SyntheticHarnessError>,
    result: Result<(), SyntheticHarnessError>,
) {
    if let Err(error) = result {
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

impl Default for SyntheticHuddleHarness {
    fn default() -> Self {
        Self::new()
    }
}

struct SyntheticSlackBootstrap {
    trace: Arc<Mutex<SyntheticHuddleTrace>>,
    #[cfg(test)]
    fail_leave_once: Arc<AtomicBool>,
}

impl SlackHuddleBootstrap for SyntheticSlackBootstrap {
    fn capability(&self) -> SlackBootstrapCapability {
        SlackBootstrapCapability::Verified {
            contract_revision: "synthetic-slack-v1",
        }
    }

    fn bootstrap(&mut self, _huddle: &ActiveHuddle) -> Result<SlackJoinSession, SignalingError> {
        self.trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .bootstrap_joins += 1;
        SlackJoinSession::new_for_adapter(
            "synthetic-meeting",
            "synthetic-attendee",
            "wss://synthetic.invalid/signaling",
            "synthetic-join-token",
            vec!["turn:synthetic.invalid".to_string()],
        )
    }

    fn leave(&mut self, _call_id: &str) -> Result<(), SignalingError> {
        let mut trace = self
            .trace
            .lock()
            .expect("synthetic huddle trace lock poisoned");
        trace.bootstrap_leaves += 1;
        #[cfg(test)]
        if self.fail_leave_once.swap(false, Ordering::SeqCst) {
            return Err(SignalingError::BootstrapFailed);
        }
        #[cfg(test)]
        trace
            .lifecycle
            .push(SyntheticLifecycleStep::BootstrapReleased);
        Ok(())
    }
}

struct SyntheticChimeBridge {
    trace: Arc<Mutex<SyntheticHuddleTrace>>,
    #[cfg(test)]
    fail_disconnect_once: Arc<AtomicBool>,
}

impl ChimeMediaBridge for SyntheticChimeBridge {
    fn capability(&self) -> ChimeBridgeCapability {
        ChimeBridgeCapability::Verified {
            bridge_revision: "synthetic-chime-v1",
        }
    }

    fn connect(&mut self, session: &SlackJoinSession) -> Result<(), SignalingError> {
        let _ = (
            session.meeting_id(),
            session.attendee_id(),
            session.signaling_url()?,
            session.join_token()?,
            session.turn_uris()?,
        );
        self.trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .bridge_connects += 1;
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), SignalingError> {
        self.trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .bridge_disconnects += 1;
        #[cfg(test)]
        if self.fail_disconnect_once.swap(false, Ordering::SeqCst) {
            return Err(SignalingError::ChimeBridgeFailed);
        }
        #[cfg(test)]
        self.trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .lifecycle
            .push(SyntheticLifecycleStep::BridgeDisconnected);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huddles::model::ActiveHuddle;
    use crate::huddles::state::{HuddlePhase, HuddleSessionStatistics};

    #[test]
    fn synthetic_session_exercises_join_controls_reconnect_statistics_and_teardown() {
        let mut harness = SyntheticHuddleHarness::new();
        harness.join(huddle()).unwrap();
        assert_eq!(harness.snapshot().phase, HuddlePhase::Connected);

        harness.set_muted(true).unwrap();
        harness.set_camera_enabled(true).unwrap();
        harness.start_screen_share().unwrap();
        assert!(harness.snapshot().controls.screen_share_enabled);
        harness.stop_screen_share().unwrap();
        assert!(!harness.snapshot().controls.screen_share_enabled);
        harness.reconnect().unwrap();
        assert_eq!(harness.snapshot().phase, HuddlePhase::Connected);
        assert!(harness.snapshot().controls.microphone_muted);
        assert!(!harness.snapshot().controls.camera_enabled);

        let statistics = HuddleSessionStatistics {
            round_trip_ms: 25,
            jitter_ms: 3,
            packets_lost: 1,
            packets_received: 500,
            audio_bitrate_bps: 32_000,
            video_bitrate_bps: 0,
        };
        harness.update_statistics(statistics.clone()).unwrap();
        assert_eq!(harness.snapshot().statistics, Some(statistics));

        harness.leave().unwrap();
        assert_eq!(harness.snapshot().phase, HuddlePhase::Idle);
        let trace = harness.trace();
        assert_eq!(trace.bootstrap_joins, 1);
        assert_eq!(trace.bridge_connects, 1);
        assert_eq!(trace.bridge_disconnects, 1);
        assert_eq!(trace.bootstrap_leaves, 1);
        assert_eq!(trace.reconnects, 1);
        assert_eq!(trace.media_starts, 1);
        assert_eq!(trace.media_stops, 1);
        assert_eq!(trace.screen_share_starts, 1);
        assert_eq!(trace.screen_share_stops, 1);
    }

    #[test]
    fn failed_join_after_media_start_rolls_back_media_and_signaling() {
        let mut harness = SyntheticHuddleHarness::new();
        harness.fail_join_after_media_start = true;

        assert!(matches!(
            harness.join(huddle()),
            Err(SyntheticHarnessError::JoinNotRequested)
        ));
        assert_eq!(harness.snapshot().phase, HuddlePhase::Failed);
        assert!(!harness.media.is_running());
        let trace = harness.trace();
        assert_eq!(trace.bootstrap_joins, 1);
        assert_eq!(trace.bridge_connects, 1);
        assert_eq!(trace.media_starts, 1);
        assert_eq!(trace.media_stops, 1);
        assert_eq!(trace.bridge_disconnects, 1);
        assert_eq!(trace.bootstrap_leaves, 1);
    }

    #[test]
    fn repeated_leave_after_complete_teardown_is_a_noop() {
        let mut harness = SyntheticHuddleHarness::new();
        harness.join(huddle()).unwrap();
        harness.leave().unwrap();
        let trace = harness.trace();

        harness.leave().unwrap();

        assert_eq!(harness.snapshot().phase, HuddlePhase::Idle);
        assert_eq!(harness.trace(), trace);
    }

    #[test]
    fn leave_detaches_active_share_before_portal_and_session_teardown() {
        let mut harness = SyntheticHuddleHarness::new();
        harness.join(huddle()).unwrap();
        harness.start_screen_share().unwrap();
        harness
            .trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .lifecycle
            .clear();

        harness.leave().unwrap();

        assert_eq!(harness.snapshot().phase, HuddlePhase::Idle);
        assert!(!harness.media.screen_share_active());
        assert!(harness.screen_cast.is_none());
        assert_eq!(
            harness.trace().lifecycle,
            vec![
                SyntheticLifecycleStep::ScreenShareDetached,
                SyntheticLifecycleStep::PortalClosed,
                SyntheticLifecycleStep::MediaStopped,
                SyntheticLifecycleStep::BridgeDisconnected,
                SyntheticLifecycleStep::BootstrapReleased,
                SyntheticLifecycleStep::CoordinatorFinalized,
            ]
        );
    }

    #[test]
    fn reconnect_detaches_active_share_before_portal_close() {
        let mut harness = SyntheticHuddleHarness::new();
        harness.join(huddle()).unwrap();
        harness.start_screen_share().unwrap();
        harness
            .trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .lifecycle
            .clear();

        harness.reconnect().unwrap();

        assert_eq!(harness.snapshot().phase, HuddlePhase::Connected);
        assert!(!harness.media.screen_share_active());
        assert!(harness.screen_cast.is_none());
        assert_eq!(
            harness.trace().lifecycle,
            vec![
                SyntheticLifecycleStep::ScreenShareDetached,
                SyntheticLifecycleStep::PortalClosed,
            ]
        );
    }

    #[test]
    fn leave_finishes_other_stages_and_retries_only_failed_signaling_cleanup() {
        let mut harness = SyntheticHuddleHarness::new();
        harness.join(huddle()).unwrap();
        harness
            .fail_bridge_disconnect_once
            .store(true, Ordering::SeqCst);

        assert!(matches!(
            harness.leave(),
            Err(SyntheticHarnessError::Signaling(
                SignalingError::ChimeBridgeFailed
            ))
        ));
        assert_eq!(harness.snapshot().phase, HuddlePhase::Idle);
        let first = harness.trace();
        assert_eq!(first.media_stops, 1);
        assert_eq!(first.bridge_disconnects, 1);
        assert_eq!(first.bootstrap_leaves, 1);

        harness.leave().unwrap();
        let retried = harness.trace();
        assert_eq!(retried.media_stops, 1);
        assert_eq!(retried.bridge_disconnects, 2);
        assert_eq!(retried.bootstrap_leaves, 1);

        harness.leave().unwrap();
        assert_eq!(harness.trace(), retried);
    }

    #[test]
    fn detach_failure_stops_media_before_closing_portal() {
        let mut harness = SyntheticHuddleHarness::new();
        harness.join(huddle()).unwrap();
        harness.start_screen_share().unwrap();
        harness
            .trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .lifecycle
            .clear();
        harness.fail_detach_once = true;

        assert!(matches!(
            harness.leave(),
            Err(SyntheticHarnessError::Media(MediaError::OperationFailed))
        ));

        assert_eq!(harness.snapshot().phase, HuddlePhase::Idle);
        assert!(!harness.media.is_running());
        assert!(!harness.media.screen_share_active());
        assert!(harness.screen_cast.is_none());
        let trace = harness.trace();
        assert_eq!(trace.screen_share_stops, 0);
        assert_eq!(trace.portal_close_attempts, 1);
        assert_eq!(
            trace.lifecycle,
            vec![
                SyntheticLifecycleStep::MediaStopped,
                SyntheticLifecycleStep::PortalClosed,
                SyntheticLifecycleStep::BridgeDisconnected,
                SyntheticLifecycleStep::BootstrapReleased,
                SyntheticLifecycleStep::CoordinatorFinalized,
            ]
        );
    }

    #[test]
    fn portal_close_failure_survives_both_attempts_then_retries_on_leave() {
        let mut harness = SyntheticHuddleHarness::new();
        harness.join(huddle()).unwrap();
        harness.start_screen_share().unwrap();
        harness
            .trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .lifecycle
            .clear();
        harness.fail_portal_close_attempts = 2;

        assert!(matches!(
            harness.leave(),
            Err(SyntheticHarnessError::Portal(PortalError::OperationFailed))
        ));

        assert_eq!(harness.snapshot().phase, HuddlePhase::Idle);
        assert!(!harness.media.is_running());
        assert!(harness.screen_cast.is_some());
        let first = harness.trace();
        assert_eq!(first.portal_close_attempts, 2);
        assert_eq!(
            first.lifecycle,
            vec![
                SyntheticLifecycleStep::ScreenShareDetached,
                SyntheticLifecycleStep::MediaStopped,
                SyntheticLifecycleStep::BridgeDisconnected,
                SyntheticLifecycleStep::BootstrapReleased,
                SyntheticLifecycleStep::CoordinatorFinalized,
            ]
        );

        harness.leave().unwrap();

        assert!(harness.screen_cast.is_none());
        let retried = harness.trace();
        assert_eq!(retried.portal_close_attempts, 3);
        assert_eq!(retried.media_stops, first.media_stops);
        assert_eq!(retried.bridge_disconnects, first.bridge_disconnects);
        assert_eq!(retried.bootstrap_leaves, first.bootstrap_leaves);
        assert_eq!(
            retried.lifecycle,
            vec![
                SyntheticLifecycleStep::ScreenShareDetached,
                SyntheticLifecycleStep::MediaStopped,
                SyntheticLifecycleStep::BridgeDisconnected,
                SyntheticLifecycleStep::BootstrapReleased,
                SyntheticLifecycleStep::CoordinatorFinalized,
                SyntheticLifecycleStep::PortalClosed,
            ]
        );

        harness.leave().unwrap();
        assert_eq!(harness.trace(), retried);
    }

    #[test]
    fn media_stop_failure_keeps_coordinator_pending_until_retry() {
        let mut harness = SyntheticHuddleHarness::new();
        harness.join(huddle()).unwrap();
        harness.fail_media_stop_once = true;

        assert!(matches!(
            harness.leave(),
            Err(SyntheticHarnessError::Media(MediaError::OperationFailed))
        ));

        assert_eq!(harness.snapshot().phase, HuddlePhase::Leaving);
        assert!(harness.media.is_running());
        let first = harness.trace();
        assert_eq!(first.media_stops, 0);
        assert_eq!(first.bridge_disconnects, 1);
        assert_eq!(first.bootstrap_leaves, 1);
        assert!(!first
            .lifecycle
            .contains(&SyntheticLifecycleStep::CoordinatorFinalized));

        harness.leave().unwrap();

        assert_eq!(harness.snapshot().phase, HuddlePhase::Idle);
        assert!(!harness.media.is_running());
        let retried = harness.trace();
        assert_eq!(retried.media_stops, 1);
        assert_eq!(retried.bridge_disconnects, 1);
        assert_eq!(retried.bootstrap_leaves, 1);
        assert_eq!(
            retried.lifecycle,
            vec![
                SyntheticLifecycleStep::BridgeDisconnected,
                SyntheticLifecycleStep::BootstrapReleased,
                SyntheticLifecycleStep::MediaStopped,
                SyntheticLifecycleStep::CoordinatorFinalized,
            ]
        );
    }

    #[test]
    fn bootstrap_leave_failure_retries_only_pending_signaling_stage() {
        let mut harness = SyntheticHuddleHarness::new();
        harness.join(huddle()).unwrap();
        harness
            .fail_bootstrap_leave_once
            .store(true, Ordering::SeqCst);

        assert!(matches!(
            harness.leave(),
            Err(SyntheticHarnessError::Signaling(
                SignalingError::BootstrapFailed
            ))
        ));

        assert_eq!(harness.snapshot().phase, HuddlePhase::Idle);
        let first = harness.trace();
        assert_eq!(first.media_stops, 1);
        assert_eq!(first.bridge_disconnects, 1);
        assert_eq!(first.bootstrap_leaves, 1);

        harness.leave().unwrap();

        let retried = harness.trace();
        assert_eq!(retried.media_stops, 1);
        assert_eq!(retried.bridge_disconnects, 1);
        assert_eq!(retried.bootstrap_leaves, 2);
        assert_eq!(
            retried.lifecycle,
            vec![
                SyntheticLifecycleStep::MediaStopped,
                SyntheticLifecycleStep::BridgeDisconnected,
                SyntheticLifecycleStep::CoordinatorFinalized,
                SyntheticLifecycleStep::BootstrapReleased,
            ]
        );

        harness.leave().unwrap();
        assert_eq!(harness.trace(), retried);
    }

    #[test]
    fn setup_error_survives_failed_rollback_then_leave_retries_cleanup() {
        let mut harness = SyntheticHuddleHarness::new();
        harness.fail_join_after_media_start = true;
        harness.fail_media_stop_once = true;
        harness
            .fail_bridge_disconnect_once
            .store(true, Ordering::SeqCst);
        harness
            .fail_bootstrap_leave_once
            .store(true, Ordering::SeqCst);

        assert!(matches!(
            harness.join(huddle()),
            Err(SyntheticHarnessError::JoinNotRequested)
        ));

        assert_eq!(harness.snapshot().phase, HuddlePhase::Failed);
        assert!(harness.media.is_running());
        let rollback = harness.trace();
        assert_eq!(rollback.media_stops, 0);
        assert_eq!(rollback.bridge_disconnects, 1);
        assert_eq!(rollback.bootstrap_leaves, 1);

        harness.leave().unwrap();

        assert_eq!(harness.snapshot().phase, HuddlePhase::Idle);
        assert!(!harness.media.is_running());
        let cleaned = harness.trace();
        assert_eq!(cleaned.media_stops, 1);
        assert_eq!(cleaned.bridge_disconnects, 2);
        assert_eq!(cleaned.bootstrap_leaves, 2);
        assert_eq!(
            cleaned.lifecycle,
            vec![
                SyntheticLifecycleStep::MediaStopped,
                SyntheticLifecycleStep::BridgeDisconnected,
                SyntheticLifecycleStep::BootstrapReleased,
                SyntheticLifecycleStep::CoordinatorFinalized,
            ]
        );
    }

    fn huddle() -> ActiveHuddle {
        ActiveHuddle {
            team_id: "T123".to_string(),
            channel_id: "C123".to_string(),
            call_id: "R123".to_string(),
            name: Some("Synthetic huddle".to_string()),
            participant_ids: vec!["U123".to_string()],
            started_at: Some(100),
            huddle_link: None,
        }
    }
}
