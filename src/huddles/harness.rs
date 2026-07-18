// This developer harness is exercised directly by tests and feature builds.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

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
use crate::huddles::state::{HuddleSessionStatistics, HuddleSnapshot};

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
}

impl SyntheticHuddleHarness {
    pub fn new() -> Self {
        let trace = Arc::new(Mutex::new(SyntheticHuddleTrace::default()));
        Self {
            coordinator: HuddleCoordinator::default(),
            gate: NativeJoinGate::new(
                SyntheticSlackBootstrap {
                    trace: Arc::clone(&trace),
                },
                SyntheticChimeBridge {
                    trace: Arc::clone(&trace),
                },
            ),
            media: SyntheticMediaEngine::default(),
            screen_cast: None,
            screen_cast_backend: Arc::new(SyntheticScreenCastBackend),
            trace,
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
        self.gate.begin_join(&huddle)?;
        self.media.start(MediaSessionConfig {
            source_mode: MediaSourceMode::Synthetic,
            sink_mode: MediaSinkMode::Fake,
            ..Default::default()
        })?;
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
        self.trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .media_starts += 1;
        self.coordinator.apply(CoordinatorInput::MediaConnected)?;
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
        let effects = self
            .coordinator
            .apply(CoordinatorInput::ScreenShareChanged(true))?;
        if !effects.contains(&HuddleEffect::StartScreenShare) {
            return Ok(());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let lease = runtime.block_on(request_screen_cast(
            Arc::clone(&self.screen_cast_backend),
            None,
            receiver,
        ))?;
        self.media
            .attach_screen_share(lease.duplicate_remote_fd()?, lease.node_id())?;
        self.screen_cast = Some(lease);
        self.coordinator
            .apply(CoordinatorInput::ScreenShareStarted)?;
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
            self.stop_screen_share_resources()?;
            self.coordinator
                .apply(CoordinatorInput::ScreenShareStopped)?;
        }
        Ok(())
    }

    pub fn reconnect(&mut self) -> Result<(), SyntheticHarnessError> {
        let effects = self.coordinator.apply(CoordinatorInput::ConnectionLost)?;
        self.apply_control_effects(&effects)?;
        if effects.contains(&HuddleEffect::StopScreenShare) {
            self.stop_screen_share_resources()?;
        }
        self.trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .reconnects += 1;
        self.coordinator.apply(CoordinatorInput::MediaReconnected)?;
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
        let effects = self.coordinator.apply(CoordinatorInput::LeaveRequested)?;
        if effects.contains(&HuddleEffect::StopSession) {
            self.stop_screen_share_resources()?;
            self.media.stop()?;
            self.trace
                .lock()
                .expect("synthetic huddle trace lock poisoned")
                .media_stops += 1;
            self.gate.stop()?;
            self.coordinator.apply(CoordinatorInput::MediaStopped)?;
        }
        Ok(())
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
        if self.media.screen_share_active() {
            self.media.detach_screen_share()?;
            self.trace
                .lock()
                .expect("synthetic huddle trace lock poisoned")
                .screen_share_stops += 1;
        }
        if let Some(lease) = self.screen_cast.take() {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(lease.close())?;
        }
        Ok(())
    }
}

impl Default for SyntheticHuddleHarness {
    fn default() -> Self {
        Self::new()
    }
}

struct SyntheticSlackBootstrap {
    trace: Arc<Mutex<SyntheticHuddleTrace>>,
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
        self.trace
            .lock()
            .expect("synthetic huddle trace lock poisoned")
            .bootstrap_leaves += 1;
        Ok(())
    }
}

struct SyntheticChimeBridge {
    trace: Arc<Mutex<SyntheticHuddleTrace>>,
}

impl ChimeMediaBridge for SyntheticChimeBridge {
    fn capability(&self) -> ChimeBridgeCapability {
        ChimeBridgeCapability::Verified {
            bridge_revision: "synthetic-chime-v1",
        }
    }

    fn connect(&mut self, session: SlackJoinSession) -> Result<(), SignalingError> {
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
