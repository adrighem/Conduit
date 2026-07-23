use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::huddles::model::{SlackHuddleRoom, SlackHuddleState};

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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    pub fn has_huddle_metadata(&self) -> bool {
        self.extra
            .get("properties")
            .and_then(|properties| properties.get("huddles"))
            .is_some_and(|huddles| !huddles.is_null())
    }

    pub fn is_user_deleted(&self) -> bool {
        self.extra_bool("is_user_deleted")
    }

    pub fn is_direct_message(&self) -> bool {
        self.is_im.unwrap_or(false) || self.is_mpim.unwrap_or(false)
    }

    pub fn is_dormant(&self) -> bool {
        self.extra_bool("is_dormant")
    }

    pub fn priority_hint(&self) -> f64 {
        self.extra_value("priority")
            .and_then(|value| match value {
                Value::Number(number) => number.as_f64(),
                Value::String(value) => value.trim().parse::<f64>().ok(),
                _ => None,
            })
            .filter(|priority| priority.is_finite())
            .unwrap_or_default()
    }

    pub fn has_active_direct_message_hint(&self) -> bool {
        self.is_direct_message() && (self.extra_bool("is_open") || self.priority_hint() > 0.0)
    }

    pub fn last_read_ts(&self) -> Option<&str> {
        self.extra.get("last_read").and_then(Value::as_str)
    }

    pub fn latest_message_ts(&self) -> Option<&str> {
        self.extra.get("latest").and_then(|latest| {
            latest
                .as_str()
                .or_else(|| latest.get("ts").and_then(Value::as_str))
        })
    }

    pub fn advance_read_cursor(&mut self, ts: &str, remaining_unread: u64) {
        self.extra
            .insert("last_read".to_string(), Value::String(ts.to_string()));
        self.apply_unread_state(SlackUnreadState::from_parts(
            true,
            remaining_unread > 0,
            remaining_unread,
        ));
    }

    pub fn display_name(&self) -> String {
        self.display_name_with_users(&HashMap::new(), None)
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

    pub fn apply_unread_snapshot(&mut self, snapshot: &SlackConversationUnreadSnapshot) {
        if self.id != snapshot.channel_id || !snapshot.unread_state.known {
            return;
        }

        self.apply_unread_state(snapshot.unread_state);
        if let Some(last_read) = snapshot.last_read.as_deref() {
            let should_advance = self
                .last_read_ts()
                .is_none_or(|current| slack_timestamp_is_after(last_read, current));
            if should_advance {
                self.extra.insert(
                    "last_read".to_string(),
                    Value::String(last_read.to_string()),
                );
            }
        }
        if let Some(latest) = snapshot.latest.as_deref() {
            let should_advance = self
                .latest_message_ts()
                .is_none_or(|current| slack_timestamp_is_after(latest, current));
            if should_advance {
                self.extra
                    .insert("latest".to_string(), Value::String(latest.to_string()));
            }
        }
        if let Some(mention_count) = snapshot.mention_count {
            self.extra.insert(
                "mention_count".to_string(),
                serde_json::json!(mention_count),
            );
        }
        if let Some(is_open) = snapshot.is_open {
            self.extra
                .insert("is_open".to_string(), serde_json::json!(is_open));
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

    pub fn display_name_with_users(
        &self,
        user_names: &HashMap<String, String>,
        current_user_id: Option<&str>,
    ) -> String {
        if self.is_im.unwrap_or(false) {
            if let Some(user) = &self.user {
                if let Some(name) = user_names.get(user) {
                    return name.clone();
                }
                return format!("DM {user}");
            }
        }

        if self.is_mpim.unwrap_or(false) {
            if let Some(name) = self.group_direct_message_display_name(user_names, current_user_id)
            {
                return name;
            }
            // Slack's legacy MPIM `name` can contain the current user's handle.
            // Until member metadata is available, use a neutral fallback.
            return format!("Group DM {}", self.id);
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

    pub fn navigation_name_with_users(
        &self,
        user_names: &HashMap<String, String>,
        user_full_names: &HashMap<String, String>,
        current_user_id: Option<&str>,
    ) -> String {
        if self.is_im.unwrap_or(false) {
            if let Some(user_id) = self.user.as_deref() {
                return direct_message_user_name(
                    user_id,
                    user_names.get(user_id).map(String::as_str),
                    user_full_names.get(user_id).map(String::as_str),
                );
            }
        }

        if self.is_mpim.unwrap_or(false) {
            let mut names = self
                .group_direct_message_user_ids()
                .into_iter()
                .filter(|user_id| Some(user_id.as_str()) != current_user_id)
                .map(|user_id| {
                    user_full_names
                        .get(&user_id)
                        .or_else(|| user_names.get(&user_id))
                        .map(|name| name.trim())
                        .filter(|name| !name.is_empty())
                        .unwrap_or(user_id.as_str())
                        .to_string()
                })
                .collect::<Vec<_>>();
            names.sort_by_key(|name| name.to_lowercase());
            if !names.is_empty() {
                return names.join(", ");
            }
        }

        self.display_name_with_users(user_names, current_user_id)
    }

    fn extra_bool(&self, key: &str) -> bool {
        self.extra_value(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn group_direct_message_display_name(
        &self,
        user_names: &HashMap<String, String>,
        current_user_id: Option<&str>,
    ) -> Option<String> {
        let names = self.group_direct_message_participant_names(user_names, current_user_id);

        (!names.is_empty()).then(|| names.join(", "))
    }

    pub fn group_direct_message_participant_names(
        &self,
        user_names: &HashMap<String, String>,
        excluded_user_id: Option<&str>,
    ) -> Vec<String> {
        let mut names = self
            .group_direct_message_user_ids()
            .into_iter()
            .filter(|user_id| Some(user_id.as_str()) != excluded_user_id)
            .map(|user_id| {
                user_names
                    .get(&user_id)
                    .map(|name| name.trim())
                    .filter(|name| !name.is_empty())
                    .unwrap_or(user_id.as_str())
                    .to_string()
            })
            .collect::<Vec<_>>();

        names.sort_by_key(|name| name.to_lowercase());
        names
    }

    pub fn group_direct_message_user_ids(&self) -> Vec<String> {
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlackConversationUnreadSnapshot {
    pub channel_id: String,
    pub unread_state: SlackUnreadState,
    pub last_read: Option<String>,
    pub latest: Option<String>,
    pub mention_count: Option<u64>,
    pub is_open: Option<bool>,
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

pub(crate) fn slack_timestamp_is_after(candidate: &str, current: &str) -> bool {
    compare_slack_timestamps(candidate, current)
        .unwrap_or_else(|| candidate.cmp(current))
        .is_gt()
}

fn compare_slack_timestamps(left: &str, right: &str) -> Option<Ordering> {
    let (left_seconds, left_fraction) = slack_timestamp_parts(left)?;
    let (right_seconds, right_fraction) = slack_timestamp_parts(right)?;
    match left_seconds.cmp(&right_seconds) {
        Ordering::Equal => {
            let width = left_fraction.len().max(right_fraction.len());
            for index in 0..width {
                let left_digit = left_fraction.as_bytes().get(index).copied().unwrap_or(b'0');
                let right_digit = right_fraction
                    .as_bytes()
                    .get(index)
                    .copied()
                    .unwrap_or(b'0');
                match left_digit.cmp(&right_digit) {
                    Ordering::Equal => {}
                    ordering => return Some(ordering),
                }
            }
            Some(Ordering::Equal)
        }
        ordering => Some(ordering),
    }
}

fn slack_timestamp_parts(value: &str) -> Option<(u64, &str)> {
    let value = value.trim();
    let (seconds, fraction) = value.split_once('.').unwrap_or((value, ""));
    if seconds.is_empty()
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some((seconds.parse().ok()?, fraction))
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
    pub thumb_video: Option<String>,
    pub url_static_preview: Option<String>,
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
        self.url_static_preview
            .as_deref()
            .or(self.thumb_480.as_deref())
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

    pub fn video_preview_url(&self) -> Option<&str> {
        self.preview_url()
            .or(self.thumb_video.as_deref())
            .or(self.url_private.as_deref())
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

    pub fn download_url(&self) -> Option<&str> {
        self.url_private_download
            .as_deref()
            .or(self.url_private.as_deref())
    }

    pub fn supported_media_kind(&self) -> Option<&'static str> {
        let mime = self.mimetype.as_deref()?;
        if mime.starts_with("image/") {
            Some("image")
        } else if matches!(
            mime,
            "video/mp4" | "video/webm" | "video/ogg" | "video/quicktime" | "video/x-matroska"
        ) {
            Some("video")
        } else {
            None
        }
    }

    pub fn media_url(&self) -> Option<&str> {
        match self.supported_media_kind()? {
            "image" => self
                .url_private_download
                .as_deref()
                .or(self.url_private.as_deref())
                .or_else(|| self.preview_url()),
            "video" => self
                .url_private_download
                .as_deref()
                .or(self.url_private.as_deref()),
            _ => None,
        }
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
    #[serde(default)]
    pub client_msg_id: Option<String>,
    pub thread_ts: Option<String>,
    pub reply_count: Option<u64>,
    #[serde(default)]
    pub reply_users: Option<Vec<String>>,
    #[serde(default)]
    pub latest_reply: Option<String>,
    #[serde(default)]
    pub subscribed: Option<bool>,
    #[serde(default)]
    pub last_read: Option<String>,
    #[serde(default)]
    pub unread_count: Option<u64>,
    pub is_starred: Option<bool>,
    pub edited: Option<SlackMessageEdit>,
    pub reactions: Option<Vec<SlackReaction>>,
    pub files: Option<Vec<SlackFile>>,
    pub blocks: Option<Value>,
    #[serde(default)]
    pub no_notifications: Option<bool>,
    #[serde(default, skip_serializing)]
    pub room: Option<SlackHuddleRoom>,
}

impl SlackMessage {
    /// Returns the root timestamp when this message is a reply in a thread.
    ///
    /// Slack may set `thread_ts` to the message's own timestamp for a thread
    /// root, so only a different non-empty timestamp identifies a reply.
    pub fn thread_root_ts(&self) -> Option<&str> {
        self.thread_ts
            .as_deref()
            .filter(|thread_ts| !thread_ts.is_empty() && *thread_ts != self.ts)
    }

    pub fn is_thread_reply(&self) -> bool {
        self.thread_root_ts().is_some()
    }

    /// Normal replies stay in their thread. Slack's explicit
    /// `thread_broadcast` subtype is the exception and is also shown in the
    /// channel timeline.
    pub fn belongs_in_channel_timeline(&self) -> bool {
        !self.is_thread_reply()
            || matches!(
                self.subtype.as_deref(),
                Some("thread_broadcast" | "reply_broadcast")
            )
    }

    pub fn belongs_to_thread(&self, thread_ts: &str) -> bool {
        self.ts == thread_ts || self.thread_root_ts() == Some(thread_ts)
    }

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

    pub fn is_notification_worthy(&self) -> bool {
        if self.no_notifications.unwrap_or(false)
            || matches!(
                self.subtype.as_deref(),
                Some(
                    "channel_archive"
                        | "channel_join"
                        | "channel_leave"
                        | "channel_name"
                        | "channel_purpose"
                        | "channel_topic"
                        | "channel_unarchive"
                        | "group_archive"
                        | "group_join"
                        | "group_leave"
                        | "group_name"
                        | "group_purpose"
                        | "group_topic"
                        | "group_unarchive"
                        | "huddle_thread"
                )
            )
        {
            return false;
        }

        self.text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
            || self.files.as_ref().is_some_and(|files| !files.is_empty())
            || self.blocks.as_ref().is_some_and(|blocks| !blocks.is_null())
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlackUser {
    pub id: Option<String>,
    pub name: Option<String>,
    pub real_name: Option<String>,
    pub deleted: Option<bool>,
    pub is_bot: Option<bool>,
    pub tz: Option<String>,
    pub tz_label: Option<String>,
    pub tz_offset: Option<i64>,
    pub profile: Option<SlackUserProfile>,
}

impl SlackUser {
    pub fn avatar_url(&self) -> Option<String> {
        let profile = self.profile.as_ref()?;
        profile
            .image_72
            .as_ref()
            .or(profile.image_192.as_ref())
            .or(profile.image_512.as_ref())
            .or(profile.image_original.as_ref())
            .filter(|url| !url.trim().is_empty())
            .cloned()
    }

    pub fn display_name(&self) -> Option<String> {
        self.profile
            .as_ref()
            .and_then(SlackUserProfile::display_name)
            .or_else(|| self.real_name.clone())
            .or_else(|| self.name.clone())
    }

    pub fn full_name(&self) -> Option<String> {
        self.profile
            .as_ref()
            .and_then(|profile| profile.real_name.clone())
            .or_else(|| self.real_name.clone())
            .filter(|name| !name.trim().is_empty())
    }

    pub fn direct_message_name(&self) -> Option<String> {
        let display_name = self.display_name();
        let full_name = self.full_name();
        match (display_name.as_deref(), full_name.as_deref()) {
            (None, None) => None,
            (display_name, full_name) => Some(direct_message_user_name(
                self.id.as_deref().unwrap_or_default(),
                display_name,
                full_name,
            )),
        }
    }

    /// Every human-readable identity Slack exposes for this user.
    ///
    /// The preferred display name remains the presentation label, while the
    /// other values allow people to be found by real name, normalized name,
    /// or Slack username.
    pub fn search_aliases(&self) -> Vec<String> {
        let profile = self.profile.as_ref();
        let candidates = [
            profile.and_then(|profile| profile.display_name.as_deref()),
            profile.and_then(|profile| profile.display_name_normalized.as_deref()),
            profile.and_then(|profile| profile.real_name.as_deref()),
            profile.and_then(|profile| profile.real_name_normalized.as_deref()),
            self.real_name.as_deref(),
            self.name.as_deref(),
        ];
        let mut aliases = Vec::new();
        for candidate in candidates.into_iter().flatten() {
            let candidate = candidate.trim();
            if !candidate.is_empty()
                && !aliases
                    .iter()
                    .any(|alias: &String| alias.eq_ignore_ascii_case(candidate))
            {
                aliases.push(candidate.to_string());
            }
        }
        aliases
    }

    pub fn status(&self) -> Option<SlackUserStatus> {
        self.profile.as_ref().and_then(SlackUserProfile::status)
    }
}

fn direct_message_user_name(
    user_id: &str,
    display_name: Option<&str>,
    full_name: Option<&str>,
) -> String {
    let display_name = display_name.map(str::trim).filter(|name| !name.is_empty());
    let full_name = full_name.map(str::trim).filter(|name| !name.is_empty());

    match (full_name, display_name) {
        (Some(full_name), Some(display_name)) if !full_name.eq_ignore_ascii_case(display_name) => {
            format!("{full_name} ({display_name})")
        }
        (Some(full_name), _) => full_name.to_string(),
        (None, Some(display_name)) => display_name.to_string(),
        (None, None) => format!("DM {user_id}"),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackUserStatus {
    pub text: String,
    pub emoji: String,
    pub expiration: i64,
}

impl SlackUserStatus {
    pub fn active_at(&self, unix_seconds: i64) -> bool {
        (!self.text.trim().is_empty() || !self.emoji_name().is_empty())
            && (self.expiration <= 0 || self.expiration > unix_seconds)
    }

    pub fn emoji_name(&self) -> &str {
        self.emoji.trim().trim_matches(':')
    }

    pub fn accessible_text(&self) -> String {
        let text = self.text.trim();
        if text.is_empty() {
            self.emoji_name().replace(['_', '-'], " ")
        } else {
            text.to_string()
        }
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlackUserProfile {
    pub display_name: Option<String>,
    pub display_name_normalized: Option<String>,
    pub real_name: Option<String>,
    pub real_name_normalized: Option<String>,
    pub status_text: Option<String>,
    pub status_emoji: Option<String>,
    pub status_expiration: Option<i64>,
    pub title: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub skype: Option<String>,
    pub pronouns: Option<String>,
    pub about: Option<String>,
    pub location: Option<String>,
    pub image_72: Option<String>,
    pub image_192: Option<String>,
    pub image_512: Option<String>,
    pub image_original: Option<String>,
    #[serde(default)]
    pub huddle_state: SlackHuddleState,
    pub huddle_state_call_id: Option<String>,
    pub huddle_state_channel_id: Option<String>,
    pub huddle_state_expiration_ts: Option<i64>,
    #[serde(default)]
    pub fields: HashMap<String, SlackProfileField>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlackProfileField {
    pub value: Option<String>,
    pub alt: Option<String>,
    pub label: Option<String>,
}

impl SlackProfileField {
    pub fn display_value(&self) -> Option<&str> {
        self.alt
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                self.value
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
            })
    }
}

impl SlackUserProfile {
    pub fn display_name(&self) -> Option<String> {
        self.display_name
            .as_ref()
            .filter(|name| !name.trim().is_empty())
            .cloned()
            .or_else(|| self.real_name.clone())
    }

    pub fn status(&self) -> Option<SlackUserStatus> {
        let status = SlackUserStatus {
            text: self.status_text.clone().unwrap_or_default(),
            emoji: self.status_emoji.clone().unwrap_or_default(),
            expiration: self.status_expiration.unwrap_or_default(),
        };
        (!status.text.trim().is_empty() || !status.emoji_name().is_empty()).then_some(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_message_preserves_client_message_identity() {
        let message: SlackMessage = serde_json::from_value(serde_json::json!({
            "ts": "1710000000.000100",
            "client_msg_id": "5dbb1f7d-cb70-4f65-b0f7-458c98ec2f24"
        }))
        .unwrap();

        assert_eq!(
            message.client_msg_id.as_deref(),
            Some("5dbb1f7d-cb70-4f65-b0f7-458c98ec2f24")
        );
    }

    #[test]
    fn user_search_aliases_preserve_display_real_normalized_and_username_names() {
        let user = SlackUser {
            name: Some("zilvinas.kuusas".to_string()),
            real_name: Some("Žilvinas Kuusas".to_string()),
            profile: Some(SlackUserProfile {
                display_name: Some("Žilvinas".to_string()),
                display_name_normalized: Some("Zilvinas".to_string()),
                real_name: Some("Žilvinas Kuusas".to_string()),
                real_name_normalized: Some("Zilvinas Kuusas".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(user.display_name().as_deref(), Some("Žilvinas"));
        assert_eq!(
            user.search_aliases(),
            vec![
                "Žilvinas",
                "Zilvinas",
                "Žilvinas Kuusas",
                "Zilvinas Kuusas",
                "zilvinas.kuusas",
            ]
        );
    }

    #[test]
    fn user_prefers_small_avatar_and_conversation_exposes_deleted_user_state() {
        let user = SlackUser {
            profile: Some(SlackUserProfile {
                image_72: Some("https://example.com/72.png".to_string()),
                image_192: Some("https://example.com/192.png".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            user.avatar_url().as_deref(),
            Some("https://example.com/72.png")
        );

        let conversation: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "D123",
            "is_im": true,
            "is_user_deleted": true
        }))
        .unwrap();
        assert!(conversation.is_user_deleted());
    }

    #[test]
    fn user_status_normalizes_emoji_and_honors_expiration() {
        let status = SlackUserStatus {
            text: "In a meeting".to_string(),
            emoji: ":spiral_calendar_pad:".to_string(),
            expiration: 200,
        };

        assert_eq!(status.emoji_name(), "spiral_calendar_pad");
        assert!(status.active_at(199));
        assert!(!status.active_at(200));

        let permanent = SlackUserStatus {
            expiration: 0,
            ..status
        };
        assert!(permanent.active_at(i64::MAX));
    }

    #[test]
    fn profile_exposes_text_only_status_but_ignores_empty_status() {
        let profile = SlackUserProfile {
            status_text: Some("Heads down".to_string()),
            ..Default::default()
        };
        assert_eq!(
            profile.status().map(|status| status.accessible_text()),
            Some("Heads down".to_string())
        );
        assert!(SlackUserProfile::default().status().is_none());
    }

    #[test]
    fn dm_display_name_uses_loaded_user_name() {
        let conversation = SlackConversation {
            id: "D123".to_string(),
            user: Some("U123".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        let names = HashMap::from([("U123".to_string(), "Ada Lovelace".to_string())]);

        assert_eq!(
            conversation.display_name_with_users(&names, None),
            "Ada Lovelace"
        );
    }

    #[test]
    fn navigation_names_prefer_full_names_without_changing_display_names() {
        let dm = SlackConversation {
            id: "D123".to_string(),
            user: Some("U1".to_string()),
            is_im: Some(true),
            ..Default::default()
        };
        let group: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "M123",
            "is_mpim": true,
            "members": ["U_SELF", "U1", "U2"]
        }))
        .expect("valid group DM");
        let display_names = HashMap::from([
            ("U1".to_string(), "ada".to_string()),
            ("U2".to_string(), "Grace".to_string()),
        ]);
        let full_names = HashMap::from([
            ("U1".to_string(), "Ada Lovelace".to_string()),
            ("U2".to_string(), "Grace Hopper".to_string()),
        ]);

        assert_eq!(dm.display_name_with_users(&display_names, None), "ada");
        assert_eq!(
            dm.navigation_name_with_users(&display_names, &full_names, None),
            "Ada Lovelace (ada)"
        );
        assert_eq!(
            group.navigation_name_with_users(&display_names, &full_names, Some("U_SELF")),
            "Ada Lovelace, Grace Hopper"
        );
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
            conversation.display_name_with_users(&names, None),
            "Ada, Grace, Zoe"
        );
    }

    #[test]
    fn group_dm_display_name_excludes_current_user() {
        let conversation: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "G123",
            "is_mpim": true,
            "members": ["U_SELF", "U2", "U1"]
        }))
        .expect("failed to parse group direct message");
        let names = HashMap::from([
            ("U_SELF".to_string(), "Vincent".to_string()),
            ("U1".to_string(), "Robey".to_string()),
            ("U2".to_string(), "Fatima".to_string()),
        ]);

        assert_eq!(
            conversation.display_name_with_users(&names, Some("U_SELF")),
            "Fatima, Robey"
        );
        assert_eq!(
            conversation.group_direct_message_participant_names(&names, Some("U_SELF")),
            vec!["Fatima", "Robey"]
        );
    }

    #[test]
    fn group_dm_display_name_falls_back_to_member_ids_or_neutral_label() {
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
            with_members.display_name_with_users(&HashMap::new(), None),
            "U1, U2, U3"
        );
        assert_eq!(
            without_members.display_name_with_users(&HashMap::new(), None),
            "Group DM G456"
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
    fn conversation_unread_snapshot_updates_cursors_monotonically() {
        let mut conversation: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "D1",
            "is_im": true,
            "latest": "10.000002"
        }))
        .unwrap();
        conversation.apply_unread_snapshot(&SlackConversationUnreadSnapshot {
            channel_id: "D1".to_string(),
            unread_state: SlackUnreadState::from_parts(true, true, 0),
            last_read: Some("9.999999".to_string()),
            latest: Some("10.000001".to_string()),
            mention_count: Some(3),
            is_open: Some(true),
        });

        assert_eq!(conversation.last_read_ts(), Some("9.999999"));
        assert_eq!(conversation.latest_message_ts(), Some("10.000002"));
        assert!(conversation.has_unread_activity());
        assert_eq!(conversation.unread_activity_count(), 0);
        assert_eq!(
            conversation.extra.get("mention_count"),
            Some(&serde_json::json!(3))
        );
        assert!(conversation.has_active_direct_message_hint());
        assert!(slack_timestamp_is_after("10.0", "9.999999"));
        assert!(!slack_timestamp_is_after("10.000001", "10.000002"));
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
    fn conversation_direct_message_hints_share_nested_slack_metadata() {
        let conversation: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "D1",
            "is_im": true,
            "properties": {
                "is_open": true,
                "is_dormant": true,
                "is_user_deleted": true,
                "priority": "0.75"
            }
        }))
        .expect("failed to parse conversation");

        assert!(conversation.is_direct_message());
        assert!(conversation.is_dormant());
        assert!(conversation.is_user_deleted());
        assert_eq!(conversation.priority_hint(), 0.75);
        assert!(conversation.has_active_direct_message_hint());
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
    fn file_download_prefers_private_download_and_never_permalink() {
        let mut file = SlackFile {
            permalink: Some("https://workspace.slack.com/files/U1/F1".to_string()),
            url_private: Some("https://files.slack.com/files-pri/F1/file.pdf".to_string()),
            url_private_download: Some(
                "https://files.slack.com/files-pri/F1/download/file.pdf".to_string(),
            ),
            ..Default::default()
        };

        assert_eq!(
            file.download_url(),
            Some("https://files.slack.com/files-pri/F1/download/file.pdf")
        );
        file.url_private_download = None;
        assert_eq!(
            file.download_url(),
            Some("https://files.slack.com/files-pri/F1/file.pdf")
        );
        file.url_private = None;
        assert_eq!(file.download_url(), None);
    }

    #[test]
    fn media_file_uses_original_download_and_supports_common_video_types() {
        let image = SlackFile {
            mimetype: Some("image/png".to_string()),
            url_private_download: Some("https://files.example/original.png".to_string()),
            thumb_480: Some("https://files.example/preview.png".to_string()),
            ..Default::default()
        };
        let video = SlackFile {
            mimetype: Some("video/x-matroska".to_string()),
            url_private: Some("https://files.example/video.mkv".to_string()),
            ..Default::default()
        };

        assert_eq!(image.supported_media_kind(), Some("image"));
        assert_eq!(
            image.media_url(),
            Some("https://files.example/original.png")
        );
        assert_eq!(video.supported_media_kind(), Some("video"));
        assert_eq!(video.media_url(), Some("https://files.example/video.mkv"));
    }

    #[test]
    fn video_file_prefers_slack_static_preview_and_preserves_motion_thumbnail() {
        let file: SlackFile = serde_json::from_value(serde_json::json!({
            "mimetype": "video/mp4",
            "thumb_480": "https://files.example/legacy-preview.png",
            "thumb_video": "https://files.example/motion-preview.mp4",
            "url_static_preview": "https://files.example/static-preview.png"
        }))
        .unwrap();

        assert_eq!(
            file.preview_url(),
            Some("https://files.example/static-preview.png")
        );
        assert_eq!(
            file.thumb_video.as_deref(),
            Some("https://files.example/motion-preview.mp4")
        );
        assert_eq!(
            file.video_preview_url(),
            Some("https://files.example/static-preview.png")
        );

        let fallback = SlackFile {
            mimetype: Some("video/mp4".to_string()),
            url_private: Some("https://files.example/video-preview.mp4".to_string()),
            ..Default::default()
        };
        assert_eq!(
            fallback.video_preview_url(),
            Some("https://files.example/video-preview.mp4")
        );
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

    #[test]
    fn message_thread_membership_distinguishes_roots_replies_and_broadcasts() {
        let root = SlackMessage {
            ts: "1".into(),
            thread_ts: Some("1".into()),
            ..Default::default()
        };
        let reply = SlackMessage {
            ts: "2".into(),
            thread_ts: Some("1".into()),
            ..Default::default()
        };
        let mut broadcast = reply.clone();
        broadcast.subtype = Some("thread_broadcast".into());
        let mut legacy_broadcast = reply.clone();
        legacy_broadcast.subtype = Some("reply_broadcast".into());

        assert!(!root.is_thread_reply());
        assert!(root.belongs_in_channel_timeline());
        assert!(root.belongs_to_thread("1"));
        assert!(reply.is_thread_reply());
        assert_eq!(reply.thread_root_ts(), Some("1"));
        assert!(!reply.belongs_in_channel_timeline());
        assert!(reply.belongs_to_thread("1"));
        assert!(broadcast.belongs_in_channel_timeline());
        assert!(legacy_broadcast.belongs_in_channel_timeline());
    }

    #[test]
    fn advancing_read_cursor_preserves_messages_after_the_cursor() {
        let mut conversation = SlackConversation {
            id: "C123".into(),
            unread_count: Some(3),
            ..Default::default()
        };

        conversation.advance_read_cursor("2.0", 1);

        assert_eq!(conversation.last_read_ts(), Some("2.0"));
        assert!(conversation.has_unread_activity());
        assert_eq!(conversation.unread_activity_count(), 1);
    }
}
