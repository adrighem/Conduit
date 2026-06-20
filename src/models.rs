use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub token_type: Option<String>,
    pub scope: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    pub expires_at: Option<u64>,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub user_id: Option<String>,
    pub client_id: Option<String>,
}

impl StoredToken {
    pub fn expires_at_from_now(expires_in: u64) -> u64 {
        now_unix().saturating_add(expires_in)
    }

    pub fn should_refresh(&self) -> bool {
        let Some(expires_at) = self.expires_at else {
            return false;
        };
        expires_at <= now_unix().saturating_add(300)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthInfo {
    pub team: Option<String>,
    pub team_id: Option<String>,
    pub user: Option<String>,
    pub user_id: Option<String>,
    pub url: Option<String>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackConversation {
    pub id: String,
    pub name: Option<String>,
    pub user: Option<String>,
    pub is_channel: Option<bool>,
    pub is_group: Option<bool>,
    pub is_im: Option<bool>,
    pub is_mpim: Option<bool>,
    pub is_private: Option<bool>,
    pub is_archived: Option<bool>,
    pub unread_count: Option<u64>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl SlackConversation {
    pub fn display_name(&self) -> String {
        self.display_name_with_users(&HashMap::new())
    }

    pub fn unread_activity_count(&self) -> u64 {
        let extra_unread_count = self
            .extra
            .iter()
            .filter(|(key, _)| key.to_lowercase().contains("unread"))
            .filter_map(|(_, value)| unread_value_count(value))
            .max()
            .unwrap_or_default();

        self.unread_count
            .unwrap_or_default()
            .max(extra_unread_count)
    }

    pub fn has_unread_activity(&self) -> bool {
        self.unread_activity_count() > 0
    }

    pub fn clear_unread_activity(&mut self) {
        self.unread_count = Some(0);

        for (key, value) in &mut self.extra {
            if key.to_lowercase().contains("unread") {
                *value = cleared_unread_value(value);
            }
        }
    }

    pub fn is_muted_conversation(&self) -> bool {
        self.extra_bool("is_muted") || self.extra_bool("muted")
    }

    pub fn is_external_conversation(&self) -> bool {
        self.extra_bool("is_ext_shared")
            || self.extra_bool("is_org_shared")
            || self.extra_bool("is_pending_ext_shared")
            || self.extra_bool("is_shared")
    }

    pub fn display_name_with_users(&self, user_names: &HashMap<String, String>) -> String {
        if self.is_im.unwrap_or(false) {
            if let Some(user) = &self.user {
                if let Some(name) = user_names.get(user) {
                    return name.clone();
                }
                return format!("DM {user}");
            }
        }

        if let Some(name) = &self.name {
            if self.is_channel.unwrap_or(false) || self.is_group.unwrap_or(false) {
                return format!("#{name}");
            }
            return name.clone();
        }

        if let Some(user) = &self.user {
            return format!("DM {user}");
        }

        self.id.clone()
    }

    fn extra_bool(&self, key: &str) -> bool {
        self.extra
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

fn unread_value_count(value: &Value) -> Option<u64> {
    match value {
        Value::Bool(true) => Some(1),
        Value::Bool(false) => Some(0),
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_i64()
                .filter(|value| *value > 0)
                .map(|value| value as u64)
        }),
        Value::String(value) => value.parse::<u64>().ok(),
        _ => None,
    }
}

fn cleared_unread_value(value: &Value) -> Value {
    match value {
        Value::Bool(_) => Value::Bool(false),
        Value::Number(_) => serde_json::json!(0),
        Value::String(_) => Value::String("0".to_string()),
        _ => value.clone(),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackFile {
    pub id: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub user: Option<String>,
    pub created: Option<u64>,
    pub timestamp: Option<u64>,
    pub mimetype: Option<String>,
    pub filetype: Option<String>,
    pub pretty_type: Option<String>,
    pub size: Option<u64>,
    pub url_private: Option<String>,
    pub url_private_download: Option<String>,
    pub thumb_64: Option<String>,
    pub thumb_80: Option<String>,
    pub thumb_160: Option<String>,
    pub thumb_360: Option<String>,
    pub thumb_480: Option<String>,
    pub thumb_720: Option<String>,
    pub thumb_1024: Option<String>,
    pub permalink: Option<String>,
    pub channels: Option<Vec<String>>,
    pub groups: Option<Vec<String>>,
    pub ims: Option<Vec<String>>,
}

impl SlackFile {
    pub fn display_title(&self) -> &str {
        self.title
            .as_deref()
            .or(self.name.as_deref())
            .or(self.id.as_deref())
            .unwrap_or("File")
    }

    pub fn is_image(&self) -> bool {
        self.mimetype
            .as_deref()
            .is_some_and(|mimetype| mimetype.starts_with("image/"))
            || self.preview_url().is_some()
    }

    pub fn preview_url(&self) -> Option<&str> {
        self.thumb_480
            .as_deref()
            .or(self.thumb_360.as_deref())
            .or(self.thumb_720.as_deref())
            .or(self.thumb_1024.as_deref())
            .or(self.thumb_160.as_deref())
            .or(self.thumb_80.as_deref())
            .or(self.thumb_64.as_deref())
            .or_else(|| {
                self.is_declared_image()
                    .then_some(self.url_private.as_deref())
                    .flatten()
            })
    }

    fn is_declared_image(&self) -> bool {
        self.mimetype
            .as_deref()
            .is_some_and(|mimetype| mimetype.starts_with("image/"))
    }

    pub fn detail_label(&self) -> String {
        let mut parts = Vec::new();

        if let Some(kind) = self
            .pretty_type
            .as_deref()
            .or(self.filetype.as_deref())
            .filter(|kind| !kind.trim().is_empty())
        {
            parts.push(kind.to_string());
        }

        if let Some(size) = self.size {
            parts.push(file_size_label(size));
        }

        parts.join(" - ")
    }

    pub fn link_url(&self) -> Option<&str> {
        self.permalink
            .as_deref()
            .or(self.url_private.as_deref())
            .or(self.url_private_download.as_deref())
    }
}

fn file_size_label(size: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let size = size as f64;
    if size >= GIB {
        format!("{:.1} GB", size / GIB)
    } else if size >= MIB {
        format!("{:.1} MB", size / MIB)
    } else if size >= KIB {
        format!("{:.1} KB", size / KIB)
    } else {
        format!("{} B", size as u64)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackMessage {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub subtype: Option<String>,
    pub user: Option<String>,
    pub username: Option<String>,
    pub text: Option<String>,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub reply_count: Option<u64>,
    pub is_starred: Option<bool>,
    pub edited: Option<SlackMessageEdit>,
    pub reactions: Option<Vec<SlackReaction>>,
    pub files: Option<Vec<SlackFile>>,
    pub blocks: Option<Value>,
}

impl SlackMessage {
    pub fn author_label(&self) -> String {
        self.username
            .clone()
            .or_else(|| self.user.clone())
            .unwrap_or_else(|| "Slack".to_string())
    }

    pub fn body_text(&self) -> String {
        self.text.clone().unwrap_or_default()
    }

    pub fn has_thread(&self) -> bool {
        self.reply_count.unwrap_or_default() > 0
    }

    pub fn latest_ts<'a>(messages: impl Iterator<Item = &'a SlackMessage>) -> Option<String> {
        messages
            .filter(|message| !message.ts.is_empty())
            .map(|message| message.ts.clone())
            .max()
    }

    pub fn user_reacted(&self, reaction_name: &str, user_id: Option<&str>) -> bool {
        let Some(user_id) = user_id else {
            return false;
        };

        self.reactions
            .as_ref()
            .into_iter()
            .flatten()
            .any(|reaction| {
                reaction.name.as_deref() == Some(reaction_name)
                    && reaction
                        .users
                        .as_ref()
                        .is_some_and(|users| users.iter().any(|user| user == user_id))
            })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackMessageEdit {
    pub user: Option<String>,
    pub ts: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackReaction {
    pub name: Option<String>,
    pub count: Option<u64>,
    pub users: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchMatch {
    pub channel: Option<SlackSearchChannel>,
    pub user: Option<String>,
    pub username: Option<String>,
    pub text: Option<String>,
    pub ts: Option<String>,
    pub permalink: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackSearchChannel {
    pub id: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedItem {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub channel: Option<String>,
    pub message: Option<SlackMessage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackUser {
    pub id: Option<String>,
    pub name: Option<String>,
    pub real_name: Option<String>,
    pub profile: Option<SlackUserProfile>,
}

impl SlackUser {
    pub fn display_name(&self) -> Option<String> {
        self.profile
            .as_ref()
            .and_then(SlackUserProfile::display_name)
            .or_else(|| self.real_name.clone())
            .or_else(|| self.name.clone())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackUserProfile {
    pub display_name: Option<String>,
    pub real_name: Option<String>,
}

impl SlackUserProfile {
    pub fn display_name(&self) -> Option<String> {
        self.display_name
            .as_ref()
            .filter(|name| !name.trim().is_empty())
            .cloned()
            .or_else(|| self.real_name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_display_name_uses_loaded_user_name() {
        let conversation = SlackConversation {
            id: "D123".to_string(),
            user: Some("U123".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        let names = HashMap::from([("U123".to_string(), "Ada Lovelace".to_string())]);

        assert_eq!(conversation.display_name_with_users(&names), "Ada Lovelace");
    }

    #[test]
    fn conversation_preserves_unknown_slack_properties() {
        let conversation: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "C123",
            "name": "general",
            "is_channel": true,
            "unread_count": 3,
            "unread_count_display": 2,
            "is_ext_shared": true,
            "last_read": "1700000000.000000"
        }))
        .expect("failed to parse conversation");

        assert_eq!(conversation.unread_count, Some(3));
        assert_eq!(
            conversation.extra.get("unread_count_display"),
            Some(&serde_json::json!(2))
        );
        assert_eq!(
            conversation.extra.get("last_read"),
            Some(&serde_json::json!("1700000000.000000"))
        );

        let serialized = serde_json::to_value(&conversation).expect("failed to serialize");
        assert_eq!(serialized["unread_count_display"], serde_json::json!(2));
        assert_eq!(
            serialized["last_read"],
            serde_json::json!("1700000000.000000")
        );
    }

    #[test]
    fn conversation_unread_activity_uses_known_and_extra_unread_fields() {
        let unread_count: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "C1",
            "unread_count": 3,
            "unread_count_display": 0
        }))
        .expect("failed to parse conversation");
        let unread_display: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "C2",
            "unread_count": 0,
            "unread_count_display": 4
        }))
        .expect("failed to parse conversation");
        let unread_flag: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "C3",
            "has_unreads": true
        }))
        .expect("failed to parse conversation");

        assert_eq!(unread_count.unread_activity_count(), 3);
        assert_eq!(unread_display.unread_activity_count(), 4);
        assert_eq!(unread_flag.unread_activity_count(), 1);
        assert!(unread_display.has_unread_activity());
    }

    #[test]
    fn conversation_clear_unread_activity_resets_known_and_extra_fields() {
        let mut conversation: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "C1",
            "unread_count": 4,
            "unread_count_display": 2,
            "has_unreads": true,
            "unread_count_string": "5",
            "last_read": "1710000000.000000"
        }))
        .expect("failed to parse conversation");

        conversation.clear_unread_activity();

        assert_eq!(conversation.unread_activity_count(), 0);
        assert_eq!(conversation.unread_count, Some(0));
        assert_eq!(
            conversation.extra.get("unread_count_display"),
            Some(&serde_json::json!(0))
        );
        assert_eq!(
            conversation.extra.get("has_unreads"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            conversation.extra.get("unread_count_string"),
            Some(&serde_json::json!("0"))
        );
        assert_eq!(
            conversation.extra.get("last_read"),
            Some(&serde_json::json!("1710000000.000000"))
        );
    }

    #[test]
    fn conversation_state_helpers_use_extra_slack_properties() {
        let mut conversation = SlackConversation {
            id: "C1".to_string(),
            ..Default::default()
        };
        conversation
            .extra
            .insert("is_muted".to_string(), serde_json::json!(true));
        conversation
            .extra
            .insert("is_ext_shared".to_string(), serde_json::json!(true));

        assert!(conversation.is_muted_conversation());
        assert!(conversation.is_external_conversation());
    }

    #[test]
    fn image_file_prefers_medium_thumbnail() {
        let file = SlackFile {
            mimetype: Some("image/png".to_string()),
            url_private: Some("https://files.example/original.png".to_string()),
            thumb_160: Some("https://files.example/160.png".to_string()),
            thumb_480: Some("https://files.example/480.png".to_string()),
            ..Default::default()
        };

        assert_eq!(file.preview_url(), Some("https://files.example/480.png"));
    }

    #[test]
    fn file_detail_label_uses_type_and_size() {
        let file = SlackFile {
            pretty_type: Some("PDF".to_string()),
            size: Some(1_572_864),
            ..Default::default()
        };

        assert_eq!(file.detail_label(), "PDF - 1.5 MB");
    }
}
