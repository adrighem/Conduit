use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub token_type: Option<String>,
    pub scope: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthInfo {
    pub team: Option<String>,
    pub team_id: Option<String>,
    pub user: Option<String>,
    pub user_id: Option<String>,
    pub url: Option<String>,
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
}

impl SlackConversation {
    pub fn display_name(&self) -> String {
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackFile {
    pub id: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub mimetype: Option<String>,
    pub url_private: Option<String>,
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
