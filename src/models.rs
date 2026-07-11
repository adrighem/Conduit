use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const CONVERSATION_MEMBER_KEYS: [&str; 2] = ["members", "users"];

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
    pub browser_cookie_d: Option<String>,
    pub user_agent: Option<String>,
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

    pub fn display_user_ids(&self) -> Vec<String> {
        let mut user_ids = Vec::new();

        if self.is_im.unwrap_or(false) {
            if let Some(user_id) = self
                .user
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            {
                user_ids.push(user_id.to_string());
            }
        } else if self.is_mpim.unwrap_or(false) {
            user_ids.extend(self.group_direct_message_user_ids());
        }

        user_ids.sort();
        user_ids.dedup();
        user_ids
    }

    pub fn unread_activity_count(&self) -> u64 {
        let extra_unread_count = self
            .extra
            .iter()
            .filter(|(key, _)| is_unread_key(key))
            .filter_map(|(_, value)| unread_count_value(value))
            .max()
            .unwrap_or_default();

        self.unread_count
            .unwrap_or_default()
            .max(extra_unread_count)
    }

    pub fn has_unread_activity(&self) -> bool {
        self.unread_activity_count() > 0
            || self
                .extra
                .iter()
                .filter(|(key, _)| is_unread_key(key))
                .any(|(_, value)| unread_flag_value(value))
    }

    pub fn unread_state(&self) -> SlackUnreadState {
        let known = self.unread_count.is_some() || self.extra.keys().any(|key| is_unread_key(key));
        let display_count = self
            .extra
            .get("unread_count_display")
            .and_then(unread_count_value)
            .or_else(|| {
                self.extra
                    .get("unread_count_string")
                    .and_then(unread_count_value)
            })
            .unwrap_or_else(|| self.unread_count.unwrap_or_default());

        SlackUnreadState::from_parts(known, self.has_unread_activity(), display_count)
    }

    pub fn clear_unread_activity(&mut self) {
        self.unread_count = Some(0);

        for (key, value) in &mut self.extra {
            if is_unread_key(key) {
                *value = cleared_unread_value(value);
            }
        }
    }

    pub fn apply_unread_state(&mut self, state: SlackUnreadState) {
        if !state.known {
            return;
        }

        self.unread_count = Some(state.display_count);
        self.extra.insert(
            "unread_count_display".to_string(),
            serde_json::json!(state.display_count),
        );
        self.extra.insert(
            "has_unreads".to_string(),
            serde_json::json!(state.has_unread),
        );
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

        if self.is_mpim.unwrap_or(false) {
            if let Some(name) = self.group_direct_message_display_name(user_names) {
                return name;
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

    fn group_direct_message_display_name(
        &self,
        user_names: &HashMap<String, String>,
    ) -> Option<String> {
        let mut names = self
            .group_direct_message_user_ids()
            .into_iter()
            .map(|user_id| {
                user_names
                    .get(&user_id)
                    .map(|name| name.trim())
                    .filter(|name| !name.is_empty())
                    .unwrap_or(user_id.as_str())
                    .to_string()
            })
            .collect::<Vec<_>>();

        if names.is_empty() {
            return None;
        }

        names.sort_by_key(|name| name.to_lowercase());
        Some(names.join(", "))
    }

    fn group_direct_message_user_ids(&self) -> Vec<String> {
        let mut user_ids = Vec::new();

        for key in CONVERSATION_MEMBER_KEYS {
            if let Some(value) = self.extra_value(key) {
                user_ids.extend(user_ids_from_value(value));
            }
        }

        user_ids.sort();
        user_ids.dedup();
        user_ids
    }

    fn extra_value(&self, key: &str) -> Option<&Value> {
        self.extra.get(key).or_else(|| {
            self.extra
                .get("properties")
                .and_then(|properties| properties.get(key))
        })
    }
}

fn user_ids_from_value(value: &Value) -> Vec<String> {
    match value {
        Value::Array(values) => values.iter().filter_map(user_id_from_value).collect(),
        Value::String(value) => value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn user_id_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => non_empty_string(value),
        Value::Object(object) => ["id", "user", "user_id"]
            .into_iter()
            .filter_map(|key| object.get(key).and_then(Value::as_str))
            .find_map(non_empty_string),
        _ => None,
    }
}

fn non_empty_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlackUnreadState {
    pub known: bool,
    pub has_unread: bool,
    pub display_count: u64,
}

impl SlackUnreadState {
    pub fn from_parts(known: bool, has_unread: bool, display_count: u64) -> Self {
        Self {
            known,
            has_unread: known && (has_unread || display_count > 0),
            display_count,
        }
    }
}

fn is_unread_key(key: &str) -> bool {
    key.to_lowercase().contains("unread")
}

fn unread_count_value(value: &Value) -> Option<u64> {
    match value {
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

fn unread_flag_value(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_u64().is_some_and(|value| value > 0),
        Value::String(value) => value.parse::<u64>().is_ok_and(|value| value > 0),
        _ => false,
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlackMessageEdit {
    pub user: Option<String>,
    pub ts: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    pub thread_ts: Option<String>,
    pub permalink: Option<String>,
}

impl SearchMatch {
    pub fn message_location(&self) -> Option<SearchMessageLocation> {
        SearchMessageLocation::new(
            self.channel.as_ref()?.id.as_deref()?,
            self.ts.as_deref()?,
            self.thread_ts.as_deref(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMessageLocation {
    channel_id: String,
    message_ts: String,
    thread_ts: Option<String>,
}

impl SearchMessageLocation {
    pub fn new(channel_id: &str, message_ts: &str, thread_ts: Option<&str>) -> Option<Self> {
        let channel_id = channel_id.trim();
        let message_ts = message_ts.trim();
        if channel_id.is_empty() || message_ts.is_empty() {
            return None;
        }
        let thread_ts = thread_ts
            .map(str::trim)
            .filter(|thread_ts| !thread_ts.is_empty() && *thread_ts != message_ts)
            .map(ToString::to_string);

        Some(Self {
            channel_id: channel_id.to_string(),
            message_ts: message_ts.to_string(),
            thread_ts,
        })
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub fn message_ts(&self) -> &str {
        &self.message_ts
    }

    pub fn thread_ts(&self) -> Option<&str> {
        self.thread_ts.as_deref()
    }
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
    pub deleted: Option<bool>,
    pub is_bot: Option<bool>,
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
pub struct SlackUserGroup {
    pub id: String,
    pub handle: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub users: Vec<String>,
}

impl SlackUserGroup {
    pub fn mention_label(&self) -> String {
        self.handle
            .as_deref()
            .filter(|label| !label.trim().is_empty())
            .or(self.name.as_deref())
            .filter(|label| !label.trim().is_empty())
            .unwrap_or(&self.id)
            .trim()
            .trim_start_matches('@')
            .to_string()
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
    fn group_dm_display_name_uses_alphabetized_member_names() {
        let conversation: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G123",
            "name": "mpdm-old-slack-name",
            "is_mpim": true,
            "members": ["U2", "U1", "U3"]
        }))
        .expect("failed to parse group direct message");
        let names = HashMap::from([
            ("U1".to_string(), "Zoe".to_string()),
            ("U2".to_string(), "Ada".to_string()),
            ("U3".to_string(), "Grace".to_string()),
        ]);

        assert_eq!(conversation.display_user_ids(), vec!["U1", "U2", "U3"]);
        assert_eq!(
            conversation.display_name_with_users(&names),
            "Ada, Grace, Zoe"
        );
    }

    #[test]
    fn group_dm_display_name_falls_back_to_member_ids_or_slack_name() {
        let with_members: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G123",
            "name": "mpdm-old-slack-name",
            "is_mpim": true,
            "properties": {
                "users": [
                    {"id": "U3"},
                    {"user": "U1"},
                    "U2"
                ]
            }
        }))
        .expect("failed to parse group direct message");
        let without_members = SlackConversation {
            id: "G456".to_string(),
            name: Some("fallback-name".to_string()),
            is_mpim: Some(true),
            ..Default::default()
        };

        assert_eq!(
            with_members.display_name_with_users(&HashMap::new()),
            "U1, U2, U3"
        );
        assert_eq!(
            without_members.display_name_with_users(&HashMap::new()),
            "fallback-name"
        );
    }

    #[test]
    fn user_group_mention_label_prefers_handle_without_at_prefix() {
        let group = SlackUserGroup {
            id: "S123".to_string(),
            handle: Some("@platform".to_string()),
            name: Some("Platform team".to_string()),
            users: Vec::new(),
        };

        assert_eq!(group.mention_label(), "platform");
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
    fn conversation_unread_activity_uses_known_and_extra_unread_counts() {
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
        assert_eq!(unread_flag.unread_activity_count(), 0);
        assert!(unread_flag.has_unread_activity());
        assert!(unread_display.has_unread_activity());
    }

    #[test]
    fn conversation_applies_badgeless_unread_state() {
        let mut conversation = SlackConversation {
            id: "C1".to_string(),
            unread_count: Some(7),
            ..Default::default()
        };

        conversation.apply_unread_state(SlackUnreadState::from_parts(true, true, 0));

        assert_eq!(conversation.unread_activity_count(), 0);
        assert!(conversation.has_unread_activity());
        assert_eq!(
            conversation.extra.get("unread_count_display"),
            Some(&serde_json::json!(0))
        );
        assert_eq!(
            conversation.extra.get("has_unreads"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn conversation_unread_state_prefers_display_count() {
        let conversation: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "C1",
            "unread_count": 5,
            "unread_count_display": 0
        }))
        .expect("failed to parse conversation");

        let state = conversation.unread_state();

        assert!(state.known);
        assert!(state.has_unread);
        assert_eq!(state.display_count, 0);
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

    #[test]
    fn search_message_location_requires_channel_and_timestamp() {
        let result = SearchMatch {
            channel: Some(SlackSearchChannel {
                id: Some(" C123 ".to_string()),
                name: Some("general".to_string()),
            }),
            ts: Some(" 1710000000.000100 ".to_string()),
            ..Default::default()
        };

        assert_eq!(
            result.message_location(),
            SearchMessageLocation::new("C123", "1710000000.000100", None)
        );
        assert!(SearchMatch::default().message_location().is_none());
        assert!(SearchMatch {
            channel: result.channel.clone(),
            ts: Some("  ".to_string()),
            ..Default::default()
        }
        .message_location()
        .is_none());
    }

    #[test]
    fn search_message_location_normalizes_thread_parents_and_replies() {
        let reply: SearchMatch = serde_json::from_value(serde_json::json!({
            "channel": { "id": "C123" },
            "ts": "1710000001.000100",
            "thread_ts": "1710000000.000100"
        }))
        .expect("search reply should parse");
        assert_eq!(
            reply.message_location().unwrap().thread_ts(),
            Some("1710000000.000100")
        );

        let parent = SearchMatch {
            thread_ts: Some("1710000001.000100".to_string()),
            ..reply
        };
        assert_eq!(parent.message_location().unwrap().thread_ts(), None);
    }
}
