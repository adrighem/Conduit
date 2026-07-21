use crate::huddles::state::{HuddlePhase, HuddleScreenShareState, HuddleSnapshot};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HuddlePrimaryAction {
    #[default]
    None,
    OpenPreflight,
    Join,
    OpenExternal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HuddlePresentation {
    pub visible: bool,
    pub title: &'static str,
    pub primary_label: Option<&'static str>,
    pub primary_action: HuddlePrimaryAction,
    pub show_external: bool,
    pub show_controls: bool,
    pub controls_sensitive: bool,
    pub show_leave: bool,
    pub show_dismiss: bool,
    pub microphone_muted: bool,
    pub camera_enabled: bool,
    pub screen_share_active: bool,
    pub screen_share_requesting: bool,
}

pub fn present_huddle(
    snapshot: &HuddleSnapshot,
    visible_channel_id: Option<&str>,
) -> HuddlePresentation {
    let matching_channel = snapshot
        .huddle
        .as_ref()
        .is_some_and(|huddle| visible_channel_id == Some(huddle.channel_id.as_str()));
    let mut presentation = HuddlePresentation {
        microphone_muted: snapshot.controls.microphone_muted,
        camera_enabled: snapshot.controls.camera_enabled,
        screen_share_active: snapshot.screen_share_state == HuddleScreenShareState::Active,
        screen_share_requesting: snapshot.screen_share_state == HuddleScreenShareState::Requesting,
        ..Default::default()
    };

    match snapshot.phase {
        HuddlePhase::Idle => {}
        HuddlePhase::Discovered if matching_channel => {
            presentation.visible = true;
            presentation.title = "Huddle is active";
            presentation.primary_label = Some("View huddle");
            presentation.primary_action = HuddlePrimaryAction::OpenPreflight;
            presentation.show_external = true;
            presentation.show_dismiss = true;
        }
        HuddlePhase::Discovered => {}
        HuddlePhase::Preflight => {
            presentation.visible = true;
            presentation.title = "Ready to join";
            presentation.primary_label = Some(if snapshot.native_join_available {
                "Join"
            } else {
                "Open in Slack"
            });
            presentation.primary_action = if snapshot.native_join_available {
                HuddlePrimaryAction::Join
            } else {
                HuddlePrimaryAction::OpenExternal
            };
            presentation.show_external = snapshot.native_join_available;
            presentation.show_dismiss = true;
        }
        HuddlePhase::Joining => {
            presentation.visible = true;
            presentation.title = "Joining huddle…";
            presentation.show_external = true;
            presentation.show_leave = true;
        }
        HuddlePhase::Connected => {
            presentation.visible = true;
            presentation.title = "In huddle";
            presentation.show_controls = true;
            presentation.controls_sensitive = true;
            presentation.show_leave = true;
        }
        HuddlePhase::Reconnecting => {
            presentation.visible = true;
            presentation.title = "Reconnecting to huddle…";
            presentation.show_controls = true;
            presentation.show_external = true;
            presentation.show_leave = true;
        }
        HuddlePhase::Leaving => {
            presentation.visible = true;
            presentation.title = "Leaving huddle…";
        }
        HuddlePhase::Failed => {
            presentation.visible = true;
            presentation.title = "Huddle connection failed";
            presentation.primary_label = Some("Review huddle");
            presentation.primary_action = HuddlePrimaryAction::OpenPreflight;
            presentation.show_external = true;
            presentation.show_dismiss = true;
        }
        HuddlePhase::ExternallyHandedOff => {
            presentation.visible = true;
            presentation.title = "Opened in Slack";
            presentation.primary_label = Some("Open again");
            presentation.primary_action = HuddlePrimaryAction::OpenExternal;
            presentation.show_dismiss = true;
        }
    }

    presentation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huddles::model::ActiveHuddle;

    fn snapshot(phase: HuddlePhase) -> HuddleSnapshot {
        HuddleSnapshot {
            phase,
            huddle: Some(ActiveHuddle {
                team_id: "T123".to_string(),
                channel_id: "C123".to_string(),
                call_id: "R123".to_string(),
                name: None,
                participant_ids: vec!["U123".to_string()],
                started_at: None,
                huddle_link: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn passive_discovery_is_visible_only_in_its_conversation() {
        let discovered = snapshot(HuddlePhase::Discovered);

        assert!(present_huddle(&discovered, Some("C123")).visible);
        assert!(!present_huddle(&discovered, Some("C999")).visible);
        assert!(!present_huddle(&discovered, None).visible);
    }

    #[test]
    fn unsupported_native_join_becomes_an_explicit_external_handoff() {
        let preflight = snapshot(HuddlePhase::Preflight);
        let presentation = present_huddle(&preflight, Some("C123"));

        assert_eq!(presentation.primary_label, Some("Open in Slack"));
        assert_eq!(
            presentation.primary_action,
            HuddlePrimaryAction::OpenExternal
        );
    }

    #[test]
    fn connected_controls_reflect_capture_state_without_enabling_it() {
        let mut connected = snapshot(HuddlePhase::Connected);
        connected.controls.microphone_muted = true;
        connected.controls.camera_enabled = true;
        connected.screen_share_state = HuddleScreenShareState::Requesting;
        let presentation = present_huddle(&connected, Some("C999"));

        assert!(presentation.visible);
        assert!(presentation.show_controls);
        assert!(presentation.controls_sensitive);
        assert!(presentation.microphone_muted);
        assert!(presentation.camera_enabled);
        assert!(presentation.screen_share_requesting);
        assert!(!presentation.screen_share_active);
    }

    #[test]
    fn every_session_phase_remains_visible_globally() {
        for phase in [
            HuddlePhase::Preflight,
            HuddlePhase::Joining,
            HuddlePhase::Connected,
            HuddlePhase::Reconnecting,
            HuddlePhase::Leaving,
            HuddlePhase::Failed,
            HuddlePhase::ExternallyHandedOff,
        ] {
            assert!(
                present_huddle(&snapshot(phase), Some("C999")).visible,
                "{phase:?}"
            );
        }
    }
}
