use crate::huddles::model::ActiveHuddle;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HuddlePhase {
    #[default]
    Idle,
    Discovered,
    Preflight,
    Joining,
    Connected,
    Reconnecting,
    Leaving,
    Failed,
    ExternallyHandedOff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HuddleDeviceKind {
    Microphone,
    Speaker,
    Camera,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuddleDevice {
    pub id: String,
    pub label: String,
    pub kind: HuddleDeviceKind,
    pub is_default: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HuddleDeviceSelection {
    pub microphone_id: Option<String>,
    pub speaker_id: Option<String>,
    pub camera_id: Option<String>,
}

impl HuddleDeviceSelection {
    pub fn select(&mut self, kind: HuddleDeviceKind, id: String) {
        match kind {
            HuddleDeviceKind::Microphone => self.microphone_id = Some(id),
            HuddleDeviceKind::Speaker => self.speaker_id = Some(id),
            HuddleDeviceKind::Camera => self.camera_id = Some(id),
        }
    }

    pub fn selected(&self, kind: HuddleDeviceKind) -> Option<&str> {
        match kind {
            HuddleDeviceKind::Microphone => self.microphone_id.as_deref(),
            HuddleDeviceKind::Speaker => self.speaker_id.as_deref(),
            HuddleDeviceKind::Camera => self.camera_id.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HuddleControls {
    pub microphone_muted: bool,
    pub camera_enabled: bool,
    pub screen_share_enabled: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HuddleScreenShareState {
    #[default]
    Off,
    Requesting,
    Active,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuddleParticipant {
    pub user_id: String,
    pub display_name: Option<String>,
    pub microphone_muted: Option<bool>,
    pub camera_enabled: bool,
    pub screen_share_enabled: bool,
}

impl HuddleParticipant {
    pub fn from_user_id(user_id: String) -> Self {
        Self {
            user_id,
            display_name: None,
            microphone_muted: None,
            camera_enabled: false,
            screen_share_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HuddleSessionStatistics {
    pub round_trip_ms: u32,
    pub jitter_ms: u32,
    pub packets_lost: u64,
    pub packets_received: u64,
    pub audio_bitrate_bps: u64,
    pub video_bitrate_bps: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HuddleFailureKind {
    Unsupported,
    PermissionDenied,
    DeviceUnavailable,
    Network,
    ProtocolChanged,
    Media,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuddleFailure {
    pub kind: HuddleFailureKind,
    pub message: String,
    pub recoverable: bool,
}

impl HuddleFailure {
    pub fn unsupported() -> Self {
        Self::safe(
            HuddleFailureKind::Unsupported,
            "Native Slack huddle joining is unavailable for this session.",
            false,
        )
    }

    pub fn permission_denied() -> Self {
        Self::safe(
            HuddleFailureKind::PermissionDenied,
            "Permission to use the requested capture source was not granted.",
            true,
        )
    }

    pub fn device_unavailable() -> Self {
        Self::safe(
            HuddleFailureKind::DeviceUnavailable,
            "A selected media device is no longer available.",
            true,
        )
    }

    pub fn network() -> Self {
        Self::safe(
            HuddleFailureKind::Network,
            "The huddle connection was interrupted.",
            true,
        )
    }

    pub fn protocol_changed() -> Self {
        Self::safe(
            HuddleFailureKind::ProtocolChanged,
            "Slack huddle compatibility could not be verified.",
            false,
        )
    }

    pub fn media() -> Self {
        Self::safe(
            HuddleFailureKind::Media,
            "The media session could not be started.",
            true,
        )
    }

    pub fn internal() -> Self {
        Self::safe(
            HuddleFailureKind::Internal,
            "The huddle session stopped unexpectedly.",
            true,
        )
    }

    fn safe(kind: HuddleFailureKind, message: &str, recoverable: bool) -> Self {
        Self {
            kind,
            message: message.to_string(),
            recoverable,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HuddleSnapshot {
    pub phase: HuddlePhase,
    pub huddle: Option<ActiveHuddle>,
    pub controls: HuddleControls,
    pub devices: HuddleDeviceSelection,
    pub participants: Vec<HuddleParticipant>,
    pub statistics: Option<HuddleSessionStatistics>,
    pub failure: Option<HuddleFailure>,
    pub screen_share_state: HuddleScreenShareState,
    pub screen_share_failure: Option<HuddleFailure>,
    pub native_join_available: bool,
}

impl HuddleSnapshot {
    pub fn capture_active(&self) -> bool {
        self.phase == HuddlePhase::Connected
            && (!self.controls.microphone_muted
                || self.controls.camera_enabled
                || self.controls.screen_share_enabled)
    }

    pub fn call_id(&self) -> Option<&str> {
        self.huddle.as_ref().map(|huddle| huddle.call_id.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HuddleCommand {
    OpenPreflight { call_id: String },
    Join { call_id: String },
    OpenExternally { call_id: String },
    Leave,
    Dismiss,
    SetMuted(bool),
    SetCameraEnabled(bool),
    SetScreenShareEnabled(bool),
    SelectDevice { kind: HuddleDeviceKind, id: String },
}

impl HuddleCommand {
    pub fn call_id(&self) -> Option<&str> {
        match self {
            Self::OpenPreflight { call_id }
            | Self::Join { call_id }
            | Self::OpenExternally { call_id } => Some(call_id),
            Self::Leave
            | Self::Dismiss
            | Self::SetMuted(_)
            | Self::SetCameraEnabled(_)
            | Self::SetScreenShareEnabled(_)
            | Self::SelectDevice { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HuddleEvent {
    Snapshot(Box<HuddleSnapshot>),
    DevicesAvailable(Vec<HuddleDevice>),
    OpenExternalRequested(ActiveHuddle),
}

impl HuddleEvent {
    pub fn call_id(&self) -> Option<&str> {
        match self {
            Self::Snapshot(snapshot) => snapshot.call_id(),
            Self::OpenExternalRequested(huddle) => Some(&huddle.call_id),
            Self::DevicesAvailable(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_active_huddle(call_id: &str) -> ActiveHuddle {
        ActiveHuddle {
            team_id: "T123".to_string(),
            channel_id: "C123".to_string(),
            call_id: call_id.to_string(),
            name: Some("Sync".to_string()),
            participant_ids: vec!["U123".to_string()],
            started_at: Some(1710000000),
            huddle_link: None,
        }
    }

    #[test]
    fn default_snapshot_and_phase_transitions_gate_capture_activity() {
        let mut snapshot = HuddleSnapshot::default();
        assert_eq!(snapshot.phase, HuddlePhase::Idle);
        assert_eq!(snapshot.screen_share_state, HuddleScreenShareState::Off);
        assert!(snapshot.call_id().is_none());
        assert!(!snapshot.capture_active());

        snapshot.huddle = Some(sample_active_huddle("R123"));
        assert_eq!(snapshot.call_id(), Some("R123"));

        for phase in [
            HuddlePhase::Discovered,
            HuddlePhase::Preflight,
            HuddlePhase::Joining,
            HuddlePhase::Reconnecting,
            HuddlePhase::Leaving,
            HuddlePhase::Failed,
            HuddlePhase::ExternallyHandedOff,
        ] {
            snapshot.phase = phase;
            assert!(!snapshot.capture_active());
        }

        snapshot.phase = HuddlePhase::Connected;
        assert!(snapshot.capture_active());

        snapshot.controls.microphone_muted = true;
        assert!(!snapshot.capture_active());

        snapshot.controls.camera_enabled = true;
        assert!(snapshot.capture_active());

        snapshot.controls.camera_enabled = false;
        snapshot.controls.screen_share_enabled = true;
        assert!(snapshot.capture_active());
    }

    #[test]
    fn device_selection_tracks_each_device_kind_independently() {
        let mut selection = HuddleDeviceSelection::default();
        assert!(selection.selected(HuddleDeviceKind::Microphone).is_none());
        assert!(selection.selected(HuddleDeviceKind::Speaker).is_none());
        assert!(selection.selected(HuddleDeviceKind::Camera).is_none());

        selection.select(HuddleDeviceKind::Microphone, "mic-1".to_string());
        selection.select(HuddleDeviceKind::Speaker, "spk-1".to_string());
        selection.select(HuddleDeviceKind::Camera, "cam-1".to_string());

        assert_eq!(
            selection.selected(HuddleDeviceKind::Microphone),
            Some("mic-1")
        );
        assert_eq!(selection.selected(HuddleDeviceKind::Speaker), Some("spk-1"));
        assert_eq!(selection.selected(HuddleDeviceKind::Camera), Some("cam-1"));

        selection.select(HuddleDeviceKind::Microphone, "mic-2".to_string());
        assert_eq!(
            selection.selected(HuddleDeviceKind::Microphone),
            Some("mic-2")
        );
    }

    #[test]
    fn screen_share_state_and_failure_helpers_expose_expected_recoverability() {
        let mut snapshot = HuddleSnapshot {
            phase: HuddlePhase::Connected,
            controls: HuddleControls {
                microphone_muted: true,
                camera_enabled: false,
                screen_share_enabled: false,
            },
            ..Default::default()
        };
        assert_eq!(snapshot.screen_share_state, HuddleScreenShareState::Off);

        snapshot.screen_share_state = HuddleScreenShareState::Requesting;
        assert!(!snapshot.capture_active());

        snapshot.screen_share_state = HuddleScreenShareState::Active;
        snapshot.controls.screen_share_enabled = true;
        assert!(snapshot.capture_active());

        snapshot.screen_share_state = HuddleScreenShareState::Failed;
        snapshot.controls.screen_share_enabled = false;
        snapshot.screen_share_failure = Some(HuddleFailure::permission_denied());
        assert!(!snapshot.capture_active());

        assert!(!HuddleFailure::unsupported().recoverable);
        assert!(!HuddleFailure::protocol_changed().recoverable);
        assert!(HuddleFailure::permission_denied().recoverable);
        assert!(HuddleFailure::device_unavailable().recoverable);
        assert!(HuddleFailure::network().recoverable);
        assert!(HuddleFailure::media().recoverable);
        assert!(HuddleFailure::internal().recoverable);
    }

    #[test]
    fn participant_commands_and_events_expose_call_ids_and_defaults() {
        let participant = HuddleParticipant::from_user_id("U456".to_string());
        assert_eq!(participant.user_id, "U456");
        assert!(participant.display_name.is_none());
        assert!(participant.microphone_muted.is_none());
        assert!(!participant.camera_enabled);
        assert!(!participant.screen_share_enabled);

        assert_eq!(
            HuddleCommand::OpenPreflight {
                call_id: "R1".to_string()
            }
            .call_id(),
            Some("R1")
        );
        assert_eq!(
            HuddleCommand::Join {
                call_id: "R2".to_string()
            }
            .call_id(),
            Some("R2")
        );
        assert_eq!(
            HuddleCommand::OpenExternally {
                call_id: "R3".to_string()
            }
            .call_id(),
            Some("R3")
        );
        assert!(HuddleCommand::Leave.call_id().is_none());
        assert!(HuddleCommand::SetScreenShareEnabled(true)
            .call_id()
            .is_none());

        let huddle = sample_active_huddle("R9");
        let snapshot_event = HuddleEvent::Snapshot(Box::new(HuddleSnapshot {
            huddle: Some(huddle.clone()),
            ..Default::default()
        }));
        assert_eq!(snapshot_event.call_id(), Some("R9"));
        assert_eq!(
            HuddleEvent::OpenExternalRequested(huddle).call_id(),
            Some("R9")
        );
        assert!(HuddleEvent::DevicesAvailable(Vec::new())
            .call_id()
            .is_none());
    }
}
