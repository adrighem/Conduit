use crate::huddles::model::{ActiveHuddle, HuddlePresence};
use crate::huddles::state::{
    HuddleControls, HuddleDeviceKind, HuddleDeviceSelection, HuddleFailure, HuddleParticipant,
    HuddlePhase, HuddleScreenShareState, HuddleSessionStatistics, HuddleSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinatorInput {
    HuddleDiscovered(ActiveHuddle),
    HuddleEnded {
        call_id: String,
    },
    PresenceChanged {
        user_id: String,
        presence: Option<HuddlePresence>,
    },
    OpenPreflight {
        call_id: String,
    },
    JoinRequested {
        call_id: String,
    },
    OpenExternally {
        call_id: String,
    },
    LeaveRequested,
    Dismissed,
    MutedChanged(bool),
    CameraChanged(bool),
    ScreenShareChanged(bool),
    ScreenShareStarted,
    ScreenShareStopped,
    #[allow(dead_code)] // Produced by the capability-gated production portal actor.
    ScreenShareFailed(HuddleFailure),
    DeviceSelected {
        kind: HuddleDeviceKind,
        id: String,
    },
    MediaConnected,
    ConnectionLost,
    MediaReconnected,
    MediaStopped,
    StatisticsUpdated(HuddleSessionStatistics),
    JoinCapabilityChanged(bool),
    Failed(HuddleFailure),
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HuddleEffect {
    Publish(HuddleSnapshot),
    BeginNativeJoin {
        huddle: ActiveHuddle,
        controls: HuddleControls,
        devices: HuddleDeviceSelection,
    },
    ApplyControls(HuddleControls),
    ApplyDeviceSelection(HuddleDeviceSelection),
    StartScreenShare,
    StopScreenShare,
    StopSession,
    OpenExternal(ActiveHuddle),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("huddle action {action} is not valid while the session is {phase:?}")]
pub struct HuddleTransitionError {
    action: &'static str,
    phase: HuddlePhase,
}

#[derive(Debug, Default)]
pub struct HuddleCoordinator {
    snapshot: HuddleSnapshot,
}

impl HuddleCoordinator {
    pub fn snapshot(&self) -> &HuddleSnapshot {
        &self.snapshot
    }

    pub fn apply(
        &mut self,
        input: CoordinatorInput,
    ) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        let previous = self.snapshot.clone();
        match self.apply_inner(input) {
            Ok(effects) => Ok(effects),
            Err(error) => {
                self.snapshot = previous;
                Err(error)
            }
        }
    }

    fn apply_inner(
        &mut self,
        input: CoordinatorInput,
    ) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        match input {
            CoordinatorInput::HuddleDiscovered(huddle) => self.discover(huddle),
            CoordinatorInput::HuddleEnded { call_id } => self.end_huddle(&call_id),
            CoordinatorInput::PresenceChanged { user_id, presence } => {
                self.update_presence(user_id, presence)
            }
            CoordinatorInput::OpenPreflight { call_id } => self.open_preflight(&call_id),
            CoordinatorInput::JoinRequested { call_id } => self.request_join(&call_id),
            CoordinatorInput::OpenExternally { call_id } => self.open_externally(&call_id),
            CoordinatorInput::LeaveRequested => self.leave(),
            CoordinatorInput::Dismissed => self.dismiss(),
            CoordinatorInput::MutedChanged(muted) => self.set_muted(muted),
            CoordinatorInput::CameraChanged(enabled) => self.set_camera(enabled),
            CoordinatorInput::ScreenShareChanged(enabled) => self.set_screen_share(enabled),
            CoordinatorInput::ScreenShareStarted => self.screen_share_started(),
            CoordinatorInput::ScreenShareStopped => self.screen_share_stopped(),
            CoordinatorInput::ScreenShareFailed(failure) => self.screen_share_failed(failure),
            CoordinatorInput::DeviceSelected { kind, id } => self.select_device(kind, id),
            CoordinatorInput::MediaConnected => self.media_connected(false),
            CoordinatorInput::ConnectionLost => self.connection_lost(),
            CoordinatorInput::MediaReconnected => self.media_connected(true),
            CoordinatorInput::MediaStopped => self.media_stopped(),
            CoordinatorInput::StatisticsUpdated(statistics) => self.update_statistics(statistics),
            CoordinatorInput::JoinCapabilityChanged(available) => {
                Ok(self.update_join_capability(available))
            }
            CoordinatorInput::Failed(failure) => self.fail(failure),
            CoordinatorInput::Reset => self.reset(),
        }
    }

    fn discover(
        &mut self,
        huddle: ActiveHuddle,
    ) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        if self.snapshot.call_id() == Some(huddle.call_id.as_str()) {
            let changed = self.snapshot.huddle.as_ref() != Some(&huddle);
            self.snapshot.huddle = Some(huddle.clone());
            let participants_changed = merge_participants(
                &mut self.snapshot.participants,
                huddle.participant_ids.iter().map(String::as_str),
            );
            return Ok(if changed || participants_changed {
                self.publish()
            } else {
                Vec::new()
            });
        }

        if matches!(
            self.snapshot.phase,
            HuddlePhase::Joining
                | HuddlePhase::Connected
                | HuddlePhase::Reconnecting
                | HuddlePhase::Leaving
        ) {
            return Ok(Vec::new());
        }

        let native_join_available = self.snapshot.native_join_available;
        self.snapshot = HuddleSnapshot {
            phase: HuddlePhase::Discovered,
            participants: huddle
                .participant_ids
                .iter()
                .cloned()
                .map(HuddleParticipant::from_user_id)
                .collect(),
            huddle: Some(huddle),
            native_join_available,
            ..Default::default()
        };
        Ok(self.publish())
    }

    fn end_huddle(&mut self, call_id: &str) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_call(call_id, "end")?;
        if self.media_may_be_active() {
            self.snapshot.phase = HuddlePhase::Leaving;
            self.disable_visual_capture();
            Ok(vec![
                HuddleEffect::Publish(self.snapshot.clone()),
                HuddleEffect::StopSession,
            ])
        } else {
            self.snapshot = HuddleSnapshot::default();
            Ok(self.publish())
        }
    }

    fn update_presence(
        &mut self,
        user_id: String,
        presence: Option<HuddlePresence>,
    ) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        let Some(huddle) = self.snapshot.huddle.as_ref() else {
            return Ok(Vec::new());
        };
        let is_present = presence
            .as_ref()
            .is_some_and(|presence| presence.matches(&huddle.call_id, &huddle.channel_id));
        let previous_len = self.snapshot.participants.len();
        let already_present = self
            .snapshot
            .participants
            .iter()
            .any(|participant| participant.user_id == user_id);
        if is_present && !already_present {
            self.snapshot
                .participants
                .push(HuddleParticipant::from_user_id(user_id));
            self.snapshot
                .participants
                .sort_by(|left, right| left.user_id.cmp(&right.user_id));
        } else if !is_present && already_present {
            self.snapshot
                .participants
                .retain(|participant| participant.user_id != user_id);
        }
        Ok(if self.snapshot.participants.len() != previous_len {
            self.publish()
        } else {
            Vec::new()
        })
    }

    fn open_preflight(
        &mut self,
        call_id: &str,
    ) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_call(call_id, "open preflight")?;
        self.require_phase(
            &[
                HuddlePhase::Discovered,
                HuddlePhase::Failed,
                HuddlePhase::ExternallyHandedOff,
            ],
            "open preflight",
        )?;
        self.snapshot.phase = HuddlePhase::Preflight;
        self.snapshot.failure = None;
        self.snapshot.controls.camera_enabled = false;
        self.snapshot.controls.screen_share_enabled = false;
        Ok(self.publish())
    }

    fn request_join(&mut self, call_id: &str) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_call(call_id, "join")?;
        self.require_phase(&[HuddlePhase::Preflight], "join")?;
        self.snapshot.phase = HuddlePhase::Joining;
        let huddle = self.snapshot.huddle.clone().expect("verified huddle");
        Ok(vec![
            HuddleEffect::Publish(self.snapshot.clone()),
            HuddleEffect::BeginNativeJoin {
                huddle,
                controls: self.snapshot.controls.clone(),
                devices: self.snapshot.devices.clone(),
            },
        ])
    }

    fn open_externally(
        &mut self,
        call_id: &str,
    ) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_call(call_id, "open externally")?;
        self.require_not_phase(
            &[HuddlePhase::Idle, HuddlePhase::Leaving],
            "open externally",
        )?;
        let huddle = self.snapshot.huddle.clone().expect("verified huddle");
        let stop_session = self.media_may_be_active();
        self.snapshot.phase = HuddlePhase::ExternallyHandedOff;
        self.snapshot.failure = None;
        self.disable_visual_capture();
        let mut effects = vec![HuddleEffect::Publish(self.snapshot.clone())];
        if stop_session {
            effects.push(HuddleEffect::StopSession);
        }
        effects.push(HuddleEffect::OpenExternal(huddle));
        Ok(effects)
    }

    fn leave(&mut self) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_not_phase(&[HuddlePhase::Idle, HuddlePhase::Leaving], "leave")?;
        self.snapshot.phase = HuddlePhase::Leaving;
        self.snapshot.failure = None;
        self.disable_visual_capture();
        Ok(vec![
            HuddleEffect::Publish(self.snapshot.clone()),
            HuddleEffect::StopSession,
        ])
    }

    fn dismiss(&mut self) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_phase(
            &[
                HuddlePhase::Discovered,
                HuddlePhase::Preflight,
                HuddlePhase::Failed,
                HuddlePhase::ExternallyHandedOff,
            ],
            "dismiss",
        )?;
        self.snapshot = HuddleSnapshot::default();
        Ok(self.publish())
    }

    fn set_muted(&mut self, muted: bool) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_controls_phase("change mute")?;
        if self.snapshot.controls.microphone_muted == muted {
            return Ok(Vec::new());
        }
        self.snapshot.controls.microphone_muted = muted;
        Ok(self.control_effects())
    }

    fn set_camera(&mut self, enabled: bool) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_controls_phase("change camera")?;
        if self.snapshot.controls.camera_enabled == enabled {
            return Ok(Vec::new());
        }
        self.snapshot.controls.camera_enabled = enabled;
        Ok(self.control_effects())
    }

    fn set_screen_share(
        &mut self,
        enabled: bool,
    ) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_phase(&[HuddlePhase::Connected], "change screen share")?;
        if enabled {
            if matches!(
                self.snapshot.screen_share_state,
                HuddleScreenShareState::Requesting | HuddleScreenShareState::Active
            ) {
                return Ok(Vec::new());
            }
            self.snapshot.screen_share_state = HuddleScreenShareState::Requesting;
            self.snapshot.screen_share_failure = None;
            self.snapshot.controls.screen_share_enabled = false;
            Ok(vec![
                HuddleEffect::Publish(self.snapshot.clone()),
                HuddleEffect::StartScreenShare,
            ])
        } else {
            if self.snapshot.screen_share_state == HuddleScreenShareState::Off {
                return Ok(Vec::new());
            }
            self.snapshot.screen_share_state = HuddleScreenShareState::Off;
            self.snapshot.screen_share_failure = None;
            self.snapshot.controls.screen_share_enabled = false;
            Ok(vec![
                HuddleEffect::Publish(self.snapshot.clone()),
                HuddleEffect::StopScreenShare,
            ])
        }
    }

    fn screen_share_started(&mut self) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_phase(&[HuddlePhase::Connected], "start screen share")?;
        if self.snapshot.screen_share_state != HuddleScreenShareState::Requesting {
            return Err(self.invalid("start unrequested screen share"));
        }
        self.snapshot.screen_share_state = HuddleScreenShareState::Active;
        self.snapshot.screen_share_failure = None;
        self.snapshot.controls.screen_share_enabled = true;
        Ok(self.publish())
    }

    fn screen_share_stopped(&mut self) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_phase(
            &[HuddlePhase::Connected, HuddlePhase::Reconnecting],
            "stop screen share",
        )?;
        let changed = self.snapshot.screen_share_state != HuddleScreenShareState::Off
            || self.snapshot.controls.screen_share_enabled
            || self.snapshot.screen_share_failure.is_some();
        self.snapshot.screen_share_state = HuddleScreenShareState::Off;
        self.snapshot.screen_share_failure = None;
        self.snapshot.controls.screen_share_enabled = false;
        Ok(if changed { self.publish() } else { Vec::new() })
    }

    fn screen_share_failed(
        &mut self,
        failure: HuddleFailure,
    ) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_phase(&[HuddlePhase::Connected], "fail screen share")?;
        self.snapshot.screen_share_state = HuddleScreenShareState::Failed;
        self.snapshot.screen_share_failure = Some(failure);
        self.snapshot.controls.screen_share_enabled = false;
        Ok(vec![
            HuddleEffect::Publish(self.snapshot.clone()),
            HuddleEffect::StopScreenShare,
        ])
    }

    fn select_device(
        &mut self,
        kind: HuddleDeviceKind,
        id: String,
    ) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_controls_phase("select device")?;
        let id = id.trim();
        if id.is_empty() {
            return Err(self.invalid("select empty device"));
        }
        if self.snapshot.devices.selected(kind) == Some(id) {
            return Ok(Vec::new());
        }
        self.snapshot.devices.select(kind, id.to_string());
        let mut effects = self.publish();
        if self.media_may_be_active() {
            effects.push(HuddleEffect::ApplyDeviceSelection(
                self.snapshot.devices.clone(),
            ));
        }
        Ok(effects)
    }

    fn media_connected(
        &mut self,
        reconnect: bool,
    ) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_phase(
            &[if reconnect {
                HuddlePhase::Reconnecting
            } else {
                HuddlePhase::Joining
            }],
            if reconnect { "reconnect" } else { "connect" },
        )?;
        self.snapshot.phase = HuddlePhase::Connected;
        self.snapshot.failure = None;
        Ok(self.publish())
    }

    fn connection_lost(&mut self) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_phase(&[HuddlePhase::Connected], "lose connection")?;
        let was_sharing = self.snapshot.screen_share_state != HuddleScreenShareState::Off;
        self.snapshot.phase = HuddlePhase::Reconnecting;
        self.disable_visual_capture();
        let mut effects = vec![
            HuddleEffect::Publish(self.snapshot.clone()),
            HuddleEffect::ApplyControls(self.snapshot.controls.clone()),
        ];
        if was_sharing {
            effects.push(HuddleEffect::StopScreenShare);
        }
        Ok(effects)
    }

    fn media_stopped(&mut self) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_phase(&[HuddlePhase::Leaving], "finish leave")?;
        self.snapshot = HuddleSnapshot::default();
        Ok(self.publish())
    }

    fn update_statistics(
        &mut self,
        statistics: HuddleSessionStatistics,
    ) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_phase(
            &[HuddlePhase::Connected, HuddlePhase::Reconnecting],
            "update statistics",
        )?;
        if self.snapshot.statistics.as_ref() == Some(&statistics) {
            return Ok(Vec::new());
        }
        self.snapshot.statistics = Some(statistics);
        Ok(self.publish())
    }

    fn update_join_capability(&mut self, available: bool) -> Vec<HuddleEffect> {
        if self.snapshot.native_join_available == available {
            return Vec::new();
        }
        self.snapshot.native_join_available = available;
        if self.snapshot.huddle.is_some() {
            self.publish()
        } else {
            Vec::new()
        }
    }

    fn fail(&mut self, failure: HuddleFailure) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        self.require_not_phase(&[HuddlePhase::Idle, HuddlePhase::Leaving], "fail")?;
        let stop_session = self.media_may_be_active();
        self.snapshot.phase = HuddlePhase::Failed;
        self.snapshot.failure = Some(failure);
        self.disable_visual_capture();
        let mut effects = vec![HuddleEffect::Publish(self.snapshot.clone())];
        if stop_session {
            effects.push(HuddleEffect::StopSession);
        }
        Ok(effects)
    }

    fn reset(&mut self) -> Result<Vec<HuddleEffect>, HuddleTransitionError> {
        let stop_session = self.snapshot.phase != HuddlePhase::Idle;
        self.snapshot = HuddleSnapshot::default();
        let mut effects = self.publish();
        if stop_session {
            effects.push(HuddleEffect::StopSession);
        }
        Ok(effects)
    }

    fn control_effects(&self) -> Vec<HuddleEffect> {
        let mut effects = self.publish();
        if self.media_may_be_active() {
            effects.push(HuddleEffect::ApplyControls(self.snapshot.controls.clone()));
        }
        effects
    }

    fn publish(&self) -> Vec<HuddleEffect> {
        vec![HuddleEffect::Publish(self.snapshot.clone())]
    }

    fn disable_visual_capture(&mut self) {
        self.snapshot.controls.camera_enabled = false;
        self.snapshot.controls.screen_share_enabled = false;
        self.snapshot.screen_share_state = HuddleScreenShareState::Off;
        self.snapshot.screen_share_failure = None;
        self.snapshot.statistics = None;
    }

    fn media_may_be_active(&self) -> bool {
        matches!(
            self.snapshot.phase,
            HuddlePhase::Joining | HuddlePhase::Connected | HuddlePhase::Reconnecting
        )
    }

    fn require_controls_phase(&self, action: &'static str) -> Result<(), HuddleTransitionError> {
        self.require_phase(
            &[
                HuddlePhase::Preflight,
                HuddlePhase::Joining,
                HuddlePhase::Connected,
                HuddlePhase::Reconnecting,
            ],
            action,
        )
    }

    fn require_call(
        &self,
        call_id: &str,
        action: &'static str,
    ) -> Result<(), HuddleTransitionError> {
        if self.snapshot.call_id() == Some(call_id) {
            Ok(())
        } else {
            Err(self.invalid(action))
        }
    }

    fn require_phase(
        &self,
        phases: &[HuddlePhase],
        action: &'static str,
    ) -> Result<(), HuddleTransitionError> {
        if phases.contains(&self.snapshot.phase) {
            Ok(())
        } else {
            Err(self.invalid(action))
        }
    }

    fn require_not_phase(
        &self,
        phases: &[HuddlePhase],
        action: &'static str,
    ) -> Result<(), HuddleTransitionError> {
        if phases.contains(&self.snapshot.phase) {
            Err(self.invalid(action))
        } else {
            Ok(())
        }
    }

    fn invalid(&self, action: &'static str) -> HuddleTransitionError {
        HuddleTransitionError {
            action,
            phase: self.snapshot.phase,
        }
    }
}

fn merge_participants<'a>(
    participants: &mut Vec<HuddleParticipant>,
    user_ids: impl Iterator<Item = &'a str>,
) -> bool {
    let previous = participants.clone();
    for user_id in user_ids {
        let user_id = user_id.trim();
        if !user_id.is_empty()
            && !participants
                .iter()
                .any(|participant| participant.user_id == user_id)
        {
            participants.push(HuddleParticipant::from_user_id(user_id.to_string()));
        }
    }
    participants.sort_by(|left, right| left.user_id.cmp(&right.user_id));
    *participants != previous
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huddles::model::{ActiveHuddle, HuddlePresence};
    use crate::huddles::state::{
        HuddleDeviceKind, HuddleFailure, HuddlePhase, HuddleSessionStatistics,
    };

    fn huddle() -> ActiveHuddle {
        ActiveHuddle {
            team_id: "T123".to_string(),
            channel_id: "C123".to_string(),
            call_id: "R123".to_string(),
            name: Some("Daily sync".to_string()),
            participant_ids: vec!["U100".to_string()],
            started_at: Some(100),
            huddle_link: Some("https://app.slack.com/huddle/T123/C123".to_string()),
        }
    }

    #[test]
    fn runs_discovery_preflight_join_connected_and_leave_lifecycle() {
        let mut coordinator = HuddleCoordinator::default();

        coordinator
            .apply(CoordinatorInput::HuddleDiscovered(huddle()))
            .unwrap();
        assert_eq!(coordinator.snapshot().phase, HuddlePhase::Discovered);

        coordinator
            .apply(CoordinatorInput::OpenPreflight {
                call_id: "R123".to_string(),
            })
            .unwrap();
        assert_eq!(coordinator.snapshot().phase, HuddlePhase::Preflight);
        assert!(!coordinator.snapshot().controls.camera_enabled);
        assert!(!coordinator.snapshot().controls.screen_share_enabled);
        assert!(!coordinator.snapshot().capture_active());

        let effects = coordinator
            .apply(CoordinatorInput::JoinRequested {
                call_id: "R123".to_string(),
            })
            .unwrap();
        assert_eq!(coordinator.snapshot().phase, HuddlePhase::Joining);
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, HuddleEffect::BeginNativeJoin { .. })));

        coordinator.apply(CoordinatorInput::MediaConnected).unwrap();
        assert_eq!(coordinator.snapshot().phase, HuddlePhase::Connected);

        let effects = coordinator.apply(CoordinatorInput::LeaveRequested).unwrap();
        assert_eq!(coordinator.snapshot().phase, HuddlePhase::Leaving);
        assert!(effects.contains(&HuddleEffect::StopSession));

        coordinator.apply(CoordinatorInput::MediaStopped).unwrap();
        assert_eq!(coordinator.snapshot().phase, HuddlePhase::Idle);
        assert!(coordinator.snapshot().huddle.is_none());
    }

    #[test]
    fn invalid_transition_keeps_the_previous_state() {
        let mut coordinator = HuddleCoordinator::default();
        coordinator
            .apply(CoordinatorInput::HuddleDiscovered(huddle()))
            .unwrap();
        let before = coordinator.snapshot().clone();

        assert!(coordinator
            .apply(CoordinatorInput::JoinRequested {
                call_id: "R123".to_string(),
            })
            .is_err());
        assert_eq!(coordinator.snapshot(), &before);
    }

    #[test]
    fn reconnect_preserves_mute_but_requires_camera_and_share_to_be_enabled_again() {
        let mut coordinator = connected_coordinator();
        coordinator
            .apply(CoordinatorInput::MutedChanged(true))
            .unwrap();
        coordinator
            .apply(CoordinatorInput::CameraChanged(true))
            .unwrap();
        coordinator
            .apply(CoordinatorInput::ScreenShareChanged(true))
            .unwrap();
        assert!(coordinator.snapshot().capture_active());

        coordinator.apply(CoordinatorInput::ConnectionLost).unwrap();
        assert_eq!(coordinator.snapshot().phase, HuddlePhase::Reconnecting);
        assert!(coordinator.snapshot().controls.microphone_muted);
        assert!(!coordinator.snapshot().controls.camera_enabled);
        assert!(!coordinator.snapshot().controls.screen_share_enabled);
        assert!(!coordinator.snapshot().capture_active());

        coordinator
            .apply(CoordinatorInput::MediaReconnected)
            .unwrap();
        assert_eq!(coordinator.snapshot().phase, HuddlePhase::Connected);
        assert!(!coordinator.snapshot().controls.camera_enabled);
        assert!(!coordinator.snapshot().controls.screen_share_enabled);
    }

    #[test]
    fn screen_share_is_only_active_after_portal_media_attachment() {
        let mut coordinator = connected_coordinator();
        let effects = coordinator
            .apply(CoordinatorInput::ScreenShareChanged(true))
            .unwrap();
        assert_eq!(
            coordinator.snapshot().screen_share_state,
            crate::huddles::state::HuddleScreenShareState::Requesting
        );
        assert!(!coordinator.snapshot().controls.screen_share_enabled);
        assert!(effects.contains(&HuddleEffect::StartScreenShare));

        coordinator
            .apply(CoordinatorInput::ScreenShareStarted)
            .unwrap();
        assert_eq!(
            coordinator.snapshot().screen_share_state,
            crate::huddles::state::HuddleScreenShareState::Active
        );
        assert!(coordinator.snapshot().controls.screen_share_enabled);

        coordinator
            .apply(CoordinatorInput::ScreenShareStopped)
            .unwrap();
        assert_eq!(
            coordinator.snapshot().screen_share_state,
            crate::huddles::state::HuddleScreenShareState::Off
        );
        assert!(!coordinator.snapshot().controls.screen_share_enabled);
    }

    #[test]
    fn screen_share_failure_rolls_back_capture_without_ending_the_call() {
        let mut coordinator = connected_coordinator();
        coordinator
            .apply(CoordinatorInput::ScreenShareChanged(true))
            .unwrap();

        let effects = coordinator
            .apply(CoordinatorInput::ScreenShareFailed(
                HuddleFailure::permission_denied(),
            ))
            .unwrap();
        assert_eq!(coordinator.snapshot().phase, HuddlePhase::Connected);
        assert_eq!(
            coordinator.snapshot().screen_share_state,
            crate::huddles::state::HuddleScreenShareState::Failed
        );
        assert!(!coordinator.snapshot().controls.screen_share_enabled);
        assert_eq!(
            coordinator
                .snapshot()
                .screen_share_failure
                .as_ref()
                .map(|failure| failure.kind),
            Some(crate::huddles::state::HuddleFailureKind::PermissionDenied)
        );
        assert!(effects.contains(&HuddleEffect::StopScreenShare));
    }

    #[test]
    fn device_roster_and_statistics_changes_are_redacted_domain_data() {
        let mut coordinator = connected_coordinator();
        coordinator
            .apply(CoordinatorInput::DeviceSelected {
                kind: HuddleDeviceKind::Microphone,
                id: "mic-1".to_string(),
            })
            .unwrap();
        assert_eq!(
            coordinator.snapshot().devices.microphone_id.as_deref(),
            Some("mic-1")
        );

        let presence = HuddlePresence {
            user_id: "U200".to_string(),
            call_id: "R123".to_string(),
            channel_id: Some("C123".to_string()),
            expires_at: 0,
        };
        coordinator
            .apply(CoordinatorInput::PresenceChanged {
                user_id: "U200".to_string(),
                presence: Some(presence.clone()),
            })
            .unwrap();
        let duplicate = coordinator
            .apply(CoordinatorInput::PresenceChanged {
                user_id: "U200".to_string(),
                presence: Some(presence),
            })
            .unwrap();
        assert!(duplicate.is_empty());
        assert!(coordinator
            .snapshot()
            .participants
            .iter()
            .any(|participant| participant.user_id == "U200"));

        let statistics = HuddleSessionStatistics {
            round_trip_ms: 28,
            jitter_ms: 4,
            packets_lost: 2,
            packets_received: 1_000,
            audio_bitrate_bps: 32_000,
            video_bitrate_bps: 0,
        };
        coordinator
            .apply(CoordinatorInput::StatisticsUpdated(statistics.clone()))
            .unwrap();
        assert_eq!(coordinator.snapshot().statistics, Some(statistics));
    }

    #[test]
    fn failures_and_external_handoff_never_leave_capture_enabled() {
        let mut failed = connected_coordinator();
        failed.apply(CoordinatorInput::CameraChanged(true)).unwrap();
        failed
            .apply(CoordinatorInput::Failed(HuddleFailure::network()))
            .unwrap();
        assert_eq!(failed.snapshot().phase, HuddlePhase::Failed);
        assert!(!failed.snapshot().capture_active());

        let mut external = HuddleCoordinator::default();
        external
            .apply(CoordinatorInput::HuddleDiscovered(huddle()))
            .unwrap();
        let effects = external
            .apply(CoordinatorInput::OpenExternally {
                call_id: "R123".to_string(),
            })
            .unwrap();
        assert_eq!(external.snapshot().phase, HuddlePhase::ExternallyHandedOff);
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, HuddleEffect::OpenExternal(_))));
        assert!(!external.snapshot().capture_active());
    }

    fn connected_coordinator() -> HuddleCoordinator {
        let mut coordinator = HuddleCoordinator::default();
        coordinator
            .apply(CoordinatorInput::HuddleDiscovered(huddle()))
            .unwrap();
        coordinator
            .apply(CoordinatorInput::OpenPreflight {
                call_id: "R123".to_string(),
            })
            .unwrap();
        coordinator
            .apply(CoordinatorInput::JoinRequested {
                call_id: "R123".to_string(),
            })
            .unwrap();
        coordinator.apply(CoordinatorInput::MediaConnected).unwrap();
        coordinator
    }
}
