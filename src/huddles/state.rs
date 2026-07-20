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
