use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::SlackUser;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlackHuddleState {
    InAHuddle,
    AvailableForHuddle,
    #[default]
    #[serde(other)]
    DefaultUnset,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SlackHuddleRoom {
    pub id: Option<String>,
    pub name: Option<String>,
    pub media_server: Option<String>,
    pub created_by: Option<String>,
    pub date_start: Option<i64>,
    pub date_end: Option<i64>,
    #[serde(default)]
    pub participants: Vec<String>,
    #[serde(default)]
    pub participant_history: Vec<String>,
    #[serde(default)]
    pub participants_events: HashMap<String, HuddleParticipantEvent>,
    #[serde(default)]
    pub participants_camera_on: Vec<String>,
    #[serde(default)]
    pub participants_camera_off: Vec<String>,
    #[serde(default)]
    pub participants_screenshare_on: Vec<String>,
    #[serde(default)]
    pub participants_screenshare_off: Vec<String>,
    pub canvas_thread_ts: Option<String>,
    pub thread_root_ts: Option<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    pub is_dm_call: Option<bool>,
    pub was_rejected: Option<bool>,
    pub was_missed: Option<bool>,
    pub was_accepted: Option<bool>,
    pub has_ended: Option<bool>,
    pub background_id: Option<String>,
    pub canvas_background: Option<String>,
    pub is_prewarmed: Option<bool>,
    pub is_scheduled: Option<bool>,
    pub recording: Option<HuddleRecording>,
    pub locale: Option<String>,
    #[serde(default)]
    pub attached_file_ids: Vec<String>,
    pub media_backend_type: Option<String>,
    pub display_id: Option<String>,
    pub external_unique_id: Option<String>,
    pub app_id: Option<String>,
    pub call_family: Option<String>,
    pub pending_invitees: Option<Value>,
    pub last_invite_status_by_user: Option<Value>,
    pub knocks: Option<Value>,
    pub huddle_link: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct HuddleParticipantEvent {
    #[serde(default)]
    pub user_team: HashMap<String, Value>,
    #[serde(default)]
    pub joined: bool,
    #[serde(default)]
    pub camera_on: bool,
    #[serde(default)]
    pub camera_off: bool,
    #[serde(default)]
    pub screenshare_on: bool,
    #[serde(default)]
    pub screenshare_off: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct HuddleRecording {
    pub can_record_summary: Option<String>,
    pub note_taking: Option<bool>,
    pub summary: Option<bool>,
    pub summary_status: Option<String>,
    pub transcript: Option<bool>,
    pub recording_user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveHuddle {
    pub team_id: String,
    pub channel_id: String,
    pub call_id: String,
    pub name: Option<String>,
    pub participant_ids: Vec<String>,
    pub started_at: Option<i64>,
    pub huddle_link: Option<String>,
}

impl SlackHuddleRoom {
    pub fn participant_ids(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.participants
            .iter()
            .chain(self.participants_camera_on.iter())
            .chain(self.participants_screenshare_on.iter())
            .filter_map(|user_id| non_empty(user_id))
            .filter(|user_id| seen.insert(user_id.clone()))
            .collect()
    }

    pub fn camera_on(&self, user_id: &str) -> bool {
        contains_id(&self.participants_camera_on, user_id)
    }

    pub fn screen_share_on(&self, user_id: &str) -> bool {
        contains_id(&self.participants_screenshare_on, user_id)
    }

    pub fn has_ended(&self) -> bool {
        self.has_ended.unwrap_or(false) || self.date_end.unwrap_or_default() > 0
    }

    pub fn active_huddle(&self, team_id: &str, channel_id: &str) -> Option<ActiveHuddle> {
        if self.has_ended() {
            return None;
        }

        let team_id = non_empty(team_id)?;
        let channel_id = non_empty(channel_id)?;
        let call_id = non_empty(self.id.as_deref()?)?;
        if !self.channels.is_empty() && !contains_id(&self.channels, &channel_id) {
            return None;
        }

        Some(ActiveHuddle {
            team_id,
            channel_id,
            call_id,
            name: self.name.as_deref().and_then(non_empty),
            participant_ids: self.participant_ids(),
            started_at: self.date_start.filter(|timestamp| *timestamp > 0),
            huddle_link: self.huddle_link.as_deref().and_then(non_empty),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuddlePresence {
    pub user_id: String,
    pub call_id: String,
    pub channel_id: Option<String>,
    pub expires_at: i64,
}

impl HuddlePresence {
    pub fn from_user(user: &SlackUser) -> Option<Self> {
        let profile = user.profile.as_ref()?;
        if profile.huddle_state != SlackHuddleState::InAHuddle {
            return None;
        }

        Some(Self {
            user_id: non_empty(user.id.as_deref()?)?,
            call_id: non_empty(profile.huddle_state_call_id.as_deref()?)?,
            channel_id: profile
                .huddle_state_channel_id
                .as_deref()
                .and_then(non_empty),
            expires_at: profile.huddle_state_expiration_ts.unwrap_or_default(),
        })
    }

    pub fn is_active_at(&self, unix_seconds: i64) -> bool {
        self.expires_at <= 0 || self.expires_at > unix_seconds
    }

    pub fn matches(&self, call_id: &str, channel_id: &str) -> bool {
        self.call_id == call_id
            && self
                .channel_id
                .as_deref()
                .is_none_or(|presence_channel| presence_channel == channel_id)
    }
}

fn contains_id(ids: &[String], expected: &str) -> bool {
    ids.iter().any(|id| id.trim() == expected)
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{SlackMessage, SlackUser};

    #[test]
    fn parses_documented_huddle_room_fields_without_serializing_private_room_data() {
        let message: SlackMessage = serde_json::from_value(serde_json::json!({
            "type": "message",
            "subtype": "huddle_thread",
            "ts": "1710000000.000100",
            "no_notifications": true,
            "room": {
                "id": "R123",
                "name": "Daily sync",
                "created_by": "U123",
                "date_start": 1710000000,
                "participants": ["U123", "U456"],
                "participants_camera_on": ["U456"],
                "participants_screenshare_on": ["U123"],
                "channels": ["C123"],
                "huddle_link": "https://app.slack.com/huddle/T123/C123",
                "media_backend_type": "chime"
            }
        }))
        .unwrap();

        let room = message.room.as_ref().unwrap();
        assert_eq!(room.id.as_deref(), Some("R123"));
        assert_eq!(room.participant_ids(), vec!["U123", "U456"]);
        assert!(room.camera_on("U456"));
        assert!(room.screen_share_on("U123"));
        assert!(!room.has_ended());
        assert!(!message.is_notification_worthy());

        let serialized = serde_json::to_value(message).unwrap();
        assert!(serialized.get("room").is_none());
        assert_eq!(
            serialized.get("no_notifications"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn ended_room_is_not_an_active_discovery() {
        let room: SlackHuddleRoom = serde_json::from_value(serde_json::json!({
            "id": "R123",
            "date_end": 1710000100,
            "participants": ["U123"],
            "channels": ["C123"]
        }))
        .unwrap();

        assert!(room.has_ended());
        assert!(room.active_huddle("T123", "C123").is_none());
    }

    #[test]
    fn correlates_active_user_presence_by_call_and_channel() {
        let user: SlackUser = serde_json::from_value(serde_json::json!({
            "id": "U123",
            "profile": {
                "display_name": "Ada",
                "huddle_state": "in_a_huddle",
                "huddle_state_call_id": "R123",
                "huddle_state_channel_id": "C123",
                "huddle_state_expiration_ts": 200
            }
        }))
        .unwrap();

        let presence = HuddlePresence::from_user(&user).unwrap();
        assert_eq!(presence.user_id, "U123");
        assert_eq!(presence.call_id, "R123");
        assert_eq!(presence.channel_id.as_deref(), Some("C123"));
        assert!(presence.is_active_at(199));
        assert!(!presence.is_active_at(200));
        assert!(presence.matches("R123", "C123"));
        assert!(!presence.matches("R123", "C999"));
    }

    #[test]
    fn ignores_available_or_incomplete_user_huddle_presence() {
        let available: SlackUser = serde_json::from_value(serde_json::json!({
            "id": "U123",
            "profile": {
                "huddle_state": "available_for_huddle",
                "huddle_state_call_id": "R123"
            }
        }))
        .unwrap();
        let missing_user: SlackUser = serde_json::from_value(serde_json::json!({
            "profile": {
                "huddle_state": "in_a_huddle",
                "huddle_state_call_id": "R123"
            }
        }))
        .unwrap();

        assert!(HuddlePresence::from_user(&available).is_none());
        assert!(HuddlePresence::from_user(&missing_user).is_none());
    }

    #[test]
    fn conversation_huddle_metadata_is_detected_but_kept_opaque() {
        let conversation: crate::models::SlackConversation =
            serde_json::from_value(serde_json::json!({
                "id": "C123",
                "properties": { "huddles": { "opaque_future_field": true } }
            }))
            .unwrap();

        assert!(conversation.has_huddle_metadata());
        assert!(conversation.extra.contains_key("properties"));
    }
}
