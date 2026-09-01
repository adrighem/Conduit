use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use reqwest::header::{CONTENT_TYPE, COOKIE, RETRY_AFTER, USER_AGENT};
use reqwest::multipart::Form;
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::auth::browser_session_cookie_header;
use crate::http_client;
use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SlackConversation, SlackFile, SlackMessage, SlackMessageEdit,
    SlackUnreadState, SlackUser, SlackUserGroup, SlackUserProfile, SlackUserStatus, StoredToken,
};
use crate::rich_message::SlackControlAction;
use crate::search::{
    SearchField, SearchQuery, ID_FIELD_WEIGHT, PRIMARY_FIELD_WEIGHT, SECONDARY_FIELD_WEIGHT,
};
use crate::slack_message_wire::{deserialize_message, deserialize_messages};

const MAX_UPLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_MEDIA_DOWNLOAD_BYTES: u64 = MAX_UPLOAD_BYTES;
pub(crate) const MAX_PREVIEW_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_PREVIEW_VIDEO_BYTES: usize = 16 * 1024 * 1024;
const MAX_RATE_LIMIT_RETRIES: usize = 2;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(10);
const API_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_RETRY_AFTER_SECONDS: u64 = 1;
const MAX_RETRY_AFTER_SECONDS: u64 = 300;
pub(crate) const CHANNEL_HISTORY_PAGE_LIMIT: usize = 30;
pub(crate) const MESSAGE_CONTEXT_LIMIT: usize = 15;
const UNREAD_STATE_HISTORY_LIMIT: usize = 1;
const THREAD_HISTORY_PAGE_LIMIT: usize = 50;
const DEFAULT_DEBUG_CONVERSATION_PROPERTY_LIMIT: usize = 20;
const DEBUG_CONVERSATION_PROPERTIES_ENV: &str = "CONDUIT_DEBUG_CONVERSATION_PROPERTIES";
const CONVERSATIONS_LIST_METHOD: &str = "conversations.list";
const USERS_CONVERSATIONS_METHOD: &str = "users.conversations";
const USERS_LIST_METHOD: &str = "users.list";
const CLIENT_USER_BOOT_METHOD: &str = "client.userBoot";
const CLIENT_COUNTS_METHOD: &str = "client.counts";
const SLACK_API_BASE_URL: &str = "https://slack.com/api";
const USER_BOOT_OMIT_EXTRAS: &str = "feature_usage_data,plan_info,salesforce_features";
const MAX_SLACK_ROUTE_BYTES: usize = 2048;
const READ_MARKER_SCOPES: [&str; 4] = ["channels:write", "groups:write", "im:write", "mpim:write"];
static NEXT_CLIENT_MESSAGE_ID: AtomicU64 = AtomicU64::new(0);

fn next_client_message_id() -> String {
    let counter = NEXT_CLIENT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let digest = Sha256::digest(format!("{}:{now}:{counter}", std::process::id()).as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn browser_action_client_token() -> String {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("web-{milliseconds}")
}

fn parse_private_action_json(value: &str) -> Result<Value> {
    let action: Value = serde_json::from_str(value)
        .map_err(|_| SlackError::validation("Slack message action metadata is invalid"))?;
    let Some(action_object) = action.as_object() else {
        return Err(SlackError::validation(
            "Slack message action metadata is invalid",
        ));
    };
    if action_object.get("type").and_then(Value::as_str) != Some("button")
        || action_object.contains_key("confirm")
        || action_object.contains_key("url")
    {
        return Err(SlackError::validation(
            "Slack message action metadata is unsupported",
        ));
    }
    Ok(action)
}

pub type Result<T> = std::result::Result<T, SlackError>;

pub(crate) fn constructed_message_permalink(
    workspace_url: &str,
    channel_id: &str,
    message_ts: &str,
) -> Option<String> {
    let target = crate::message_handoff::MessageRef::new(channel_id, message_ts).ok()?;
    crate::message_handoff::SafeSlackPermalink::construct(workspace_url, &target)
        .ok()
        .map(|permalink| permalink.as_str().to_string())
}

#[cfg(test)]
pub(crate) fn validated_message_permalink(
    permalink: &str,
    workspace_url: &str,
    channel_id: &str,
    message_ts: &str,
) -> Option<String> {
    let target = crate::message_handoff::MessageRef::new(channel_id, message_ts).ok()?;
    crate::message_handoff::SafeSlackPermalink::validate_authoritative(
        permalink,
        workspace_url,
        &target,
    )
    .ok()
    .map(|permalink| permalink.as_str().to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlackErrorCategory {
    Authentication,
    Connectivity,
    RateLimited,
    LocalIo,
    Validation,
    Unexpected,
}

#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    #[error("Slack method {method} failed: {code}")]
    Api { method: String, code: String },
    #[error("Slack method {method} was rate limited; try again shortly")]
    RateLimited { method: String },
    #[error("{message}")]
    Validation { message: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl SlackError {
    fn api(method: impl Into<String>, code: impl Into<String>) -> Self {
        Self::Api {
            method: method.into(),
            code: code.into(),
        }
    }

    fn validation(message: impl Into<String>) -> Self {
        Self::Validation {
            message: message.into(),
        }
    }

    pub fn category(&self) -> SlackErrorCategory {
        match self {
            Self::Api { code, .. } if slack_error_code_requires_authentication(code) => {
                SlackErrorCategory::Authentication
            }
            Self::Api { code, .. } if slack_error_code_is_rate_limited(code) => {
                SlackErrorCategory::RateLimited
            }
            Self::Api { .. } => SlackErrorCategory::Unexpected,
            Self::RateLimited { .. } => SlackErrorCategory::RateLimited,
            Self::Validation { .. } => SlackErrorCategory::Validation,
            Self::Other(error) => classify_wrapped_slack_error(error),
        }
    }

    pub fn is_permission_denied(&self) -> bool {
        matches!(
            self,
            Self::Api { code, .. }
                if matches!(
                    code.as_str(),
                    "access_denied"
                        | "cant_invite"
                        | "invitee_cant_see_channel"
                        | "missing_scope"
                        | "no_external_invite_permission"
                        | "no_permission"
                        | "not_in_channel"
                        | "restricted_action"
                        | "user_is_restricted"
                )
        )
    }
}

fn slack_error_code_requires_authentication(code: &str) -> bool {
    matches!(
        code,
        "account_inactive" | "invalid_auth" | "not_authed" | "token_expired" | "token_revoked"
    )
}

fn slack_error_code_is_rate_limited(code: &str) -> bool {
    matches!(code, "ratelimited" | "rate_limited")
}

fn classify_wrapped_slack_error(error: &anyhow::Error) -> SlackErrorCategory {
    for source in error.chain() {
        if let Some(request) = source.downcast_ref::<reqwest::Error>() {
            if request.status().is_some_and(|status| {
                status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
            }) {
                return SlackErrorCategory::Authentication;
            }
            if request.is_timeout() || request.is_connect() || request.is_request() {
                return SlackErrorCategory::Connectivity;
            }
        }
        if let Some(io) = source.downcast_ref::<std::io::Error>() {
            return match io.kind() {
                std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::TimedOut => SlackErrorCategory::Connectivity,
                _ => SlackErrorCategory::LocalIo,
            };
        }
    }
    SlackErrorCategory::Unexpected
}

fn workspace_search_api_query(query: &str) -> String {
    let mut in_quoted_phrase = false;
    query
        .split_whitespace()
        .map(|term| {
            let quoted = in_quoted_phrase || term.contains('"');
            if term.matches('"').count() % 2 == 1 {
                in_quoted_phrase = !in_quoted_phrase;
            }
            if quoted
                || workspace_search_term_is_modifier(term)
                || term.contains('*')
                || term.chars().count() < 3
            {
                term.to_string()
            } else {
                format!("{term}*")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn workspace_search_term_is_modifier(term: &str) -> bool {
    term.starts_with('-') || term.contains(':')
}

fn workspace_search_content_query(query: &str) -> String {
    let mut in_quoted_modifier = false;
    query
        .split_whitespace()
        .filter(|term| {
            if in_quoted_modifier {
                if term.matches('"').count() % 2 == 1 {
                    in_quoted_modifier = false;
                }
                return false;
            }
            if workspace_search_term_is_modifier(term) {
                if term.matches('"').count() % 2 == 1 {
                    in_quoted_modifier = true;
                }
                return false;
            }
            true
        })
        .map(|term| term.trim_matches(['"', '*']))
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn filter_workspace_search_matches(query: &str, matches: Vec<SearchMatch>) -> Vec<SearchMatch> {
    let content_query = workspace_search_content_query(query);
    let search_query = SearchQuery::parse(&content_query);
    let mut ranked = matches
        .into_iter()
        .enumerate()
        .filter_map(|(original_index, item)| {
            let score = search_query.score([
                SearchField::new(
                    item.text.as_deref().unwrap_or_default(),
                    PRIMARY_FIELD_WEIGHT,
                ),
                SearchField::new(
                    item.username.as_deref().unwrap_or_default(),
                    SECONDARY_FIELD_WEIGHT,
                ),
                SearchField::new(item.user.as_deref().unwrap_or_default(), ID_FIELD_WEIGHT),
                SearchField::new(
                    item.channel
                        .as_ref()
                        .and_then(|channel| channel.name.as_deref())
                        .unwrap_or_default(),
                    SECONDARY_FIELD_WEIGHT,
                ),
                SearchField::new(
                    item.channel
                        .as_ref()
                        .and_then(|channel| channel.id.as_deref())
                        .unwrap_or_default(),
                    ID_FIELD_WEIGHT,
                ),
            ])?;
            Some((score, original_index, item))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(
        |(left_score, left_index, _), (right_score, right_index, _)| {
            right_score
                .band()
                .cmp(&left_score.band())
                .then_with(|| left_index.cmp(right_index))
        },
    );
    ranked.into_iter().map(|(_, _, item)| item).collect()
}

#[derive(Clone)]
pub struct SlackApi {
    http: Client,
    pub(crate) api_base_url: String,
    access_token: String,
    scopes: HashSet<String>,
    browser_cookie_d: Option<String>,
    user_agent: Option<String>,
}

async fn send_tracked_slack_request(
    request: reqwest::RequestBuilder,
) -> reqwest::Result<reqwest::Response> {
    crate::debug::pipeline_counters().record_api_request();
    request.send().await
}

#[derive(Clone, PartialEq, Eq)]
pub struct SlackMessageActionRequest {
    pub(crate) channel_id: String,
    pub(crate) message_ts: String,
    pub(crate) thread_ts: Option<String>,
    pub(crate) service_id: String,
    pub(crate) app_id: Option<String>,
    pub(crate) bot_user_id: Option<String>,
    pub(crate) action: SlackControlAction,
}

impl std::fmt::Debug for SlackMessageActionRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SlackMessageActionRequest")
            .field("channel_id", &self.channel_id)
            .field("message_ts", &self.message_ts)
            .field("thread_ts", &self.thread_ts)
            .field("service_id", &"[REDACTED]")
            .field("app_id", &self.app_id.as_ref().map(|_| "[REDACTED]"))
            .field(
                "bot_user_id",
                &self.bot_user_id.as_ref().map(|_| "[REDACTED]"),
            )
            .field("action", &self.action)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackUnreadSnapshot {
    pub channels: Vec<SlackUnreadSnapshotRecord>,
    pub ims: Vec<SlackUnreadSnapshotRecord>,
    pub mpims: Vec<SlackUnreadSnapshotRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackUnreadSnapshotRecord {
    pub conversation_id: String,
    pub last_read: Option<String>,
    pub latest: Option<String>,
    pub has_unreads: bool,
    pub mention_count: u64,
    pub is_open: bool,
}

impl SlackApi {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn browser_cookie_d(&self) -> Option<&str> {
        self.browser_cookie_d.as_deref()
    }

    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    pub fn new(token: StoredToken) -> Self {
        let scopes = token_scope_set(token.scope.as_deref());
        Self {
            http: http_client::builder()
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .read_timeout(HTTP_READ_TIMEOUT)
                .build()
                .expect("valid Slack HTTP client configuration"),
            api_base_url: SLACK_API_BASE_URL.to_string(),
            access_token: token.access_token,
            scopes,
            browser_cookie_d: token.browser_cookie_d,
            user_agent: token.user_agent,
        }
    }

    pub async fn auth_test(&self) -> Result<AuthInfo> {
        let response: AuthTestResponse = self.post_form("auth.test", &[]).await?;
        Ok(AuthInfo {
            team: response.team,
            team_id: response.team_id,
            user: response.user,
            user_id: response.user_id,
            url: response.url,
        })
    }

    /// Loads the browser client's compact unread snapshot for one authenticated
    /// Slack workspace. This is intentionally unavailable without the browser
    /// cookie associated with the stored session; an imported browser user
    /// agent is forwarded when available.
    pub async fn browser_unread_snapshot(
        &self,
        workspace_url: &str,
    ) -> Result<SlackUnreadSnapshot> {
        self.ensure_browser_session_credentials()?;
        let api_base_url = self.browser_workspace_api_base_url(workspace_url)?;
        let user_boot: BrowserUserBootResponse = self
            .post_browser_form(
                &api_base_url,
                CLIENT_USER_BOOT_METHOD,
                &[],
                &[
                    ("version_all_channels", "false".to_string()),
                    ("return_all_relevant_mpdms", "true".to_string()),
                    ("omit_extras", USER_BOOT_OMIT_EXTRAS.to_string()),
                    ("_x_app_name", "client".to_string()),
                    ("_x_reason", "initial-data".to_string()),
                    ("_x_sonic", "true".to_string()),
                ],
            )
            .await?;
        let slack_route = validated_slack_route(user_boot.slack_route)?;
        let open_ims = normalize_open_im_ids(user_boot.ims)?;
        let counts: BrowserCountsResponse = self
            .post_browser_form(
                &api_base_url,
                CLIENT_COUNTS_METHOD,
                &[("slack_route", slack_route)],
                &[
                    ("include_all_unreads", "true".to_string()),
                    ("include_file_channels", "true".to_string()),
                    ("org_wide_aware", "true".to_string()),
                    ("thread_counts_by_channel", "true".to_string()),
                    ("_x_app_name", "client".to_string()),
                    ("_x_mode", "online".to_string()),
                    ("_x_reason", "fetchClientCountsOnConnect".to_string()),
                    ("_x_sonic", "true".to_string()),
                ],
            )
            .await?;

        normalize_browser_unread_snapshot(counts, &open_ims)
    }

    pub(crate) async fn execute_message_action(
        &self,
        workspace_url: &str,
        team_id: &str,
        request: &SlackMessageActionRequest,
    ) -> Result<()> {
        self.ensure_browser_session_credentials()?;
        if team_id.trim().is_empty()
            || request.channel_id.trim().is_empty()
            || request.message_ts.trim().is_empty()
            || request.service_id.trim().is_empty()
        {
            return Err(SlackError::validation(
                "Slack message action metadata is incomplete",
            ));
        }
        let api_base_url = self.browser_workspace_api_base_url(workspace_url)?;
        let client_token = browser_action_client_token();
        let common = [
            ("_x_app_name", "client".to_string()),
            ("_x_mode", "online".to_string()),
            ("_x_sonic", "true".to_string()),
        ];

        match &request.action {
            SlackControlAction::Block { action } => {
                let action = parse_private_action_json(action.expose())?;
                let mut container = json!({
                    "type": "message",
                    "message_ts": request.message_ts,
                    "channel_id": request.channel_id,
                    "is_ephemeral": false,
                });
                if let Some(thread_ts) = request.thread_ts.as_ref() {
                    container["thread_ts"] = Value::String(thread_ts.clone());
                }
                let mut params = vec![
                    ("service_id", request.service_id.clone()),
                    ("service_team_id", team_id.to_string()),
                    ("actions", Value::Array(vec![action]).to_string()),
                    ("container", container.to_string()),
                    ("client_token", client_token),
                    ("_x_reason", "dispatch_action_to_developer".to_string()),
                ];
                if let Some(app_id) = request.app_id.as_ref() {
                    params.push(("app_id", app_id.clone()));
                }
                params.extend(common);
                let _: MessageActionResponse = self
                    .post_browser_form(&api_base_url, "blocks.actions", &[], &params)
                    .await?;
            }
            SlackControlAction::LegacyAttachment {
                attachment_id,
                callback_id,
                action,
            } => {
                let action = parse_private_action_json(action.expose())?;
                let mut payload = json!({
                    "actions": [action],
                    "attachment_id": attachment_id,
                    "callback_id": callback_id.expose(),
                    "channel_id": request.channel_id,
                    "is_ephemeral": false,
                    "message_ts": request.message_ts,
                    "prompt_app_install": false,
                    "team_id": team_id,
                });
                if let Some(thread_ts) = request.thread_ts.as_ref() {
                    payload["thread_ts"] = Value::String(thread_ts.clone());
                }
                let bot_user_id = request.bot_user_id.as_ref().ok_or_else(|| {
                    SlackError::validation("Slack message action metadata is incomplete")
                })?;
                let mut params = vec![
                    ("payload", payload.to_string()),
                    ("client_token", client_token),
                    ("service_id", request.service_id.clone()),
                    ("bot_user_id", bot_user_id.clone()),
                    ("_x_reason", "user_attachment_action_dispatch".to_string()),
                ];
                if let Some(app_id) = request.app_id.as_ref() {
                    params.push(("app_id", app_id.clone()));
                }
                params.extend(common);
                let _: MessageActionResponse = self
                    .post_browser_form(&api_base_url, "chat.attachmentAction", &[], &params)
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn conversations(&self) -> Result<Vec<SlackConversation>> {
        let mut cursor: Option<String> = None;
        let mut conversations = Vec::new();

        loop {
            let mut params = vec![
                (
                    "types",
                    "public_channel,private_channel,mpim,im".to_string(),
                ),
                ("exclude_archived", "false".to_string()),
                ("limit", "200".to_string()),
            ];
            if let Some(cursor) = cursor.as_ref() {
                params.push(("cursor", cursor.clone()));
            }

            let response: ConversationListResponse =
                self.post_form(USERS_CONVERSATIONS_METHOD, &params).await?;
            conversations.extend(response.channels);

            cursor = response
                .response_metadata
                .and_then(|metadata| metadata.next_cursor);
            if cursor.as_deref().unwrap_or_default().is_empty() {
                break;
            }
        }

        conversations.sort_by_key(|conversation| conversation.display_name().to_lowercase());
        log_conversation_properties(USERS_CONVERSATIONS_METHOD, &conversations);
        Ok(conversations)
    }

    /// Lists every accessible public or private channel, including channels the
    /// current user has not joined yet.
    pub async fn discover_conversations(&self) -> Result<Vec<SlackConversation>> {
        let mut cursor: Option<String> = None;
        let mut conversations = Vec::new();

        loop {
            let params = paginated_list_params(cursor.as_deref(), true);
            let response: ConversationListResponse =
                self.post_form(CONVERSATIONS_LIST_METHOD, &params).await?;
            conversations.extend(
                response
                    .channels
                    .into_iter()
                    .filter(is_discoverable_conversation),
            );

            cursor = next_cursor(response.response_metadata);
            if cursor.is_none() {
                break;
            }
        }

        conversations.sort_by_key(|conversation| conversation.display_name().to_lowercase());
        Ok(conversations)
    }

    /// Lists workspace users across every page returned by Slack.
    pub async fn users(&self) -> Result<Vec<SlackUser>> {
        let mut cursor: Option<String> = None;
        let mut users = Vec::new();

        loop {
            let params = paginated_list_params(cursor.as_deref(), false);
            let response: UsersListResponse = self.post_form(USERS_LIST_METHOD, &params).await?;
            users.extend(response.members);

            cursor = next_cursor(response.response_metadata);
            if cursor.is_none() {
                break;
            }
        }

        users.sort_by_key(|user| user.display_name().unwrap_or_default().to_lowercase());
        Ok(users)
    }

    /// Lists workspace-defined emoji. Slack represents aliases as
    /// `alias:target`, which is intentionally preserved for catalog-level
    /// resolution.
    #[allow(dead_code)]
    pub async fn custom_emojis(&self) -> Result<HashMap<String, String>> {
        let response: EmojiListResponse = self.post_form("emoji.list", &[]).await?;
        Ok(response.emoji)
    }

    pub async fn join_conversation(&self, channel_id: &str) -> Result<SlackConversation> {
        let response: ConversationJoinResponse = self
            .post_form("conversations.join", &[("channel", channel_id.to_string())])
            .await?;
        Ok(response.channel)
    }

    pub async fn leave_conversation(&self, channel_id: &str) -> Result<()> {
        let _: BasicResponse = self
            .post_form(
                "conversations.leave",
                &[("channel", channel_id.to_string())],
            )
            .await?;
        Ok(())
    }

    pub async fn open_direct_message(&self, user_id: &str) -> Result<SlackConversation> {
        self.open_direct_message_with_users(&[user_id.to_string()])
            .await
    }

    pub async fn open_direct_message_with_users(
        &self,
        user_ids: &[String],
    ) -> Result<SlackConversation> {
        let users = conversation_user_ids_param(user_ids, 8)?;
        let response: ConversationOpenResponse = self
            .post_form("conversations.open", &[("users", users)])
            .await?;
        Ok(response.channel)
    }

    pub async fn create_channel(&self, name: &str, is_private: bool) -> Result<SlackConversation> {
        let params = channel_creation_params(name, is_private)?;
        let response: ConversationJoinResponse =
            self.post_form("conversations.create", &params).await?;
        Ok(response.channel)
    }

    pub async fn invite_to_channel(
        &self,
        channel_id: &str,
        user_ids: &[String],
    ) -> Result<SlackConversation> {
        let users = conversation_user_ids_param(user_ids, 100)?;
        let response: ConversationJoinResponse = self
            .post_form(
                "conversations.invite",
                &[
                    ("channel", channel_id.to_string()),
                    ("users", users),
                    ("force", "true".to_string()),
                ],
            )
            .await?;
        Ok(response.channel)
    }

    pub fn can_mark_read(&self) -> bool {
        self.scopes.is_empty()
            || READ_MARKER_SCOPES
                .iter()
                .any(|scope| self.scopes.contains(*scope))
    }

    pub async fn history(&self, channel_id: &str) -> Result<SlackMessagePage> {
        self.history_page(channel_id, None).await
    }

    pub async fn history_page(
        &self,
        channel_id: &str,
        cursor: Option<&str>,
    ) -> Result<SlackMessagePage> {
        let params = history_request_params(channel_id, cursor, CHANNEL_HISTORY_PAGE_LIMIT, true);

        let response: HistoryResponse = self.post_form("conversations.history", &params).await?;
        Ok(SlackMessagePage::from_response(
            response,
            std::convert::identity,
        ))
    }

    pub async fn history_context(
        &self,
        channel_id: &str,
        message_ts: &str,
    ) -> Result<SlackMessagePage> {
        let params = message_context_request_params(channel_id, message_ts);
        let response: HistoryResponse = self.post_form("conversations.history", &params).await?;
        Ok(SlackMessagePage::from_response(
            response,
            std::convert::identity,
        ))
    }

    pub async fn conversation_with_unread_state(
        &self,
        channel_id: &str,
    ) -> Result<(Option<SlackConversation>, SlackUnreadState)> {
        let mut last_read: Option<String> = None;
        let mut details = None;

        match self.conversation_info(channel_id).await {
            Ok(conversation) => {
                let unread_state = conversation.unread_state();
                if unread_state.known {
                    return Ok((Some(conversation), unread_state));
                }

                last_read = conversation_last_read_ts(&conversation).map(ToString::to_string);
                if let (Some(last_read), Some(latest_ts)) =
                    (last_read.as_deref(), conversation_latest_ts(&conversation))
                {
                    let unread_state = unread_state_from_last_read(last_read, latest_ts);
                    return Ok((Some(conversation), unread_state));
                }
                details = Some(conversation);
            }
            Err(error) => crate::debug::log(
                "slack",
                &format!(
                    "ConversationInfoUnreadFallback channel_id={channel_id} category={:?} error={error:#}",
                    error.category()
                ),
            ),
        }

        let params = history_request_params(channel_id, None, UNREAD_STATE_HISTORY_LIMIT, true);
        let response: HistoryResponse = self.post_form("conversations.history", &params).await?;
        let unread_state = unread_state_from_history_response(&response);
        if unread_state.known {
            return Ok((details, unread_state));
        }

        if let (Some(last_read), Some(latest_message)) =
            (last_read.as_deref(), response.messages.first())
        {
            return Ok((
                details,
                unread_state_from_last_read(last_read, &latest_message.ts),
            ));
        }

        Ok((details, unread_state))
    }

    pub async fn conversation_info(&self, channel_id: &str) -> Result<SlackConversation> {
        let response: ConversationInfoResponse = self
            .post_form("conversations.info", &[("channel", channel_id.to_string())])
            .await?;
        Ok(response.channel)
    }

    pub async fn conversation_members(&self, channel_id: &str) -> Result<Vec<String>> {
        let mut cursor: Option<String> = None;
        let mut members = Vec::new();
        loop {
            let mut params = vec![
                ("channel", channel_id.to_string()),
                ("limit", "200".to_string()),
            ];
            if let Some(cursor) = cursor.as_ref() {
                params.push(("cursor", cursor.clone()));
            }
            let response: ConversationMembersResponse =
                self.post_form("conversations.members", &params).await?;
            members.extend(response.members);
            cursor = next_cursor(response.response_metadata);
            if cursor.is_none() {
                break;
            }
        }
        members.sort();
        members.dedup();
        Ok(members)
    }

    pub async fn thread_replies(&self, channel_id: &str, ts: &str) -> Result<SlackMessagePage> {
        self.thread_replies_page(channel_id, ts, None).await
    }

    pub async fn thread_replies_page(
        &self,
        channel_id: &str,
        ts: &str,
        cursor: Option<&str>,
    ) -> Result<SlackMessagePage> {
        let mut params = vec![
            ("channel", channel_id.to_string()),
            ("ts", ts.to_string()),
            ("limit", THREAD_HISTORY_PAGE_LIMIT.to_string()),
        ];
        if let Some(cursor) = cursor.filter(|cursor| !cursor.trim().is_empty()) {
            params.push(("cursor", cursor.to_string()));
        }

        let response: HistoryResponse = self.post_form("conversations.replies", &params).await?;
        Ok(SlackMessagePage::from_response(
            response,
            thread_replies_in_history_order,
        ))
    }

    pub async fn thread_replies_context(
        &self,
        channel_id: &str,
        thread_ts: &str,
        message_ts: &str,
    ) -> Result<SlackMessagePage> {
        let params = thread_message_context_request_params(channel_id, thread_ts, message_ts);
        let response: HistoryResponse = self.post_form("conversations.replies", &params).await?;
        Ok(SlackMessagePage::from_response(
            response,
            thread_replies_in_history_order,
        ))
    }

    pub async fn search_messages(&self, query: &str) -> Result<Vec<SearchMatch>> {
        let api_query = workspace_search_api_query(query);
        let response: SearchResponse = self
            .post_form(
                "search.messages",
                &[
                    ("query", api_query),
                    ("count", "100".to_string()),
                    ("page", "1".to_string()),
                ],
            )
            .await?;
        Ok(filter_workspace_search_matches(
            query,
            response.messages.matches,
        ))
    }

    pub async fn saved_items(&self) -> Result<Vec<SavedItem>> {
        self.star_items().await
    }

    pub async fn starred_conversation_ids(&self) -> Result<HashSet<String>> {
        Ok(self
            .star_items()
            .await?
            .into_iter()
            .filter_map(|item| match item.kind.as_deref() {
                Some("channel" | "im" | "mpim") => item.channel,
                Some("group") => item.group.or(item.channel),
                _ => None,
            })
            .filter(|id| !id.trim().is_empty())
            .collect())
    }

    pub async fn files(&self) -> Result<Vec<SlackFile>> {
        let response: FilesListResponse = self
            .post_form(
                "files.list",
                &[("count", "50".to_string()), ("page", "1".to_string())],
            )
            .await?;
        Ok(response.files)
    }

    pub async fn file(&self, file_id: &str) -> Result<SlackFile> {
        let response: FileInfoResponse = self
            .post_form("files.info", &[("file", file_id.to_string())])
            .await?;
        Ok(response.file)
    }

    #[allow(dead_code)]
    pub async fn user_display_name(&self, user_id: &str) -> Result<String> {
        Ok(self
            .user(user_id)
            .await?
            .display_name()
            .unwrap_or_else(|| user_id.to_string()))
    }

    pub async fn user(&self, user_id: &str) -> Result<SlackUser> {
        let response: UserInfoResponse = self
            .post_form("users.info", &[("user", user_id.to_string())])
            .await?;
        Ok(response.user)
    }

    pub async fn user_profile(&self, user_id: &str) -> Result<SlackUserProfile> {
        let response: UserProfileResponse = self
            .post_form(
                "users.profile.get",
                &[
                    ("user", user_id.to_string()),
                    ("include_labels", "true".to_string()),
                ],
            )
            .await?;
        response.profile.ok_or_else(|| {
            SlackError::Other(anyhow::anyhow!(
                "Slack users.profile.get response omitted the profile"
            ))
        })
    }

    pub async fn set_current_user_status(
        &self,
        status: &SlackUserStatus,
    ) -> Result<SlackUserProfile> {
        let status_text = status.text.trim();
        if status_text.chars().count() > 100 {
            return Err(SlackError::validation(
                "Slack status text must be 100 characters or fewer",
            ));
        }
        let emoji_name = status.emoji_name();
        let status_emoji = if emoji_name.is_empty() {
            String::new()
        } else {
            format!(":{emoji_name}:")
        };
        let profile = json!({
            "status_text": status_text,
            "status_emoji": status_emoji,
            "status_expiration": status.expiration.max(0),
        })
        .to_string();
        let response: UserProfileResponse = self
            .post_form("users.profile.set", &[("profile", profile)])
            .await?;
        response.profile.ok_or_else(|| {
            SlackError::Other(anyhow::anyhow!(
                "Slack users.profile.set response omitted the profile"
            ))
        })
    }

    #[allow(dead_code)]
    pub async fn user_groups(&self) -> Result<Vec<SlackUserGroup>> {
        let response: UserGroupsListResponse = self
            .post_form("usergroups.list", &[("include_users", "true".to_string())])
            .await?;
        Ok(response.usergroups)
    }

    pub async fn download_preview_asset(&self, url: &str) -> Result<DownloadedPreviewAsset> {
        if !supports_native_preview_asset_url(url) {
            return Err(SlackError::validation(
                "preview URL is not a trusted Slack asset URL",
            ));
        }
        let request = if is_trusted_slack_download_url(url) {
            self.authenticated_request(Method::GET, url)
        } else {
            self.http.get(url)
        };
        let response = send_tracked_slack_request(request)
            .await
            .context("failed to download Slack preview asset")?
            .error_for_status()
            .context("Slack preview asset returned an HTTP error")?;

        let mime_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(PreviewAssetMime::parse)
            .ok_or_else(|| {
                SlackError::validation(
                    "Slack attachment preview returned an unsupported content type",
                )
            })?;

        let max_bytes = mime_type.max_bytes();

        if response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(SlackError::validation(
                "Slack attachment preview is too large",
            ));
        }

        let initial_capacity = response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default()
            .min(max_bytes);
        let mut bytes = Vec::with_capacity(initial_capacity);
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .context("failed to read Slack attachment preview bytes")?
        {
            append_bounded_preview_chunk(&mut bytes, &chunk, max_bytes)?;
        }

        if !mime_type.is_valid_payload(&bytes) {
            return Err(SlackError::validation(
                "Slack attachment preview bytes do not match the declared content type",
            ));
        }

        Ok(DownloadedPreviewAsset { mime_type, bytes })
    }

    /// Downloads viewable Slack media to `destination` without retaining the
    /// complete response in memory. The destination is replaced atomically
    /// after a successful download and never contains a partial response.
    pub async fn download_media(&self, url: &str, destination: &Path) -> Result<DownloadedMedia> {
        ensure_trusted_slack_download_url(url)?;
        let response = send_tracked_slack_request(self.authenticated_request(Method::GET, url))
            .await
            .context("failed to download Slack media")?
            .error_for_status()
            .context("Slack media returned an HTTP error")?;

        let mime_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(supported_media_mime_type)
            .ok_or_else(|| SlackError::validation("Slack media has an unsupported content type"))?
            .to_string();

        ensure_media_size(response.content_length())?;
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("failed to create the Slack media cache directory")?;
        }

        let partial_path = partial_download_path(destination);
        let result = async {
            let mut file = tokio::fs::File::create(&partial_path)
                .await
                .context("failed to create the Slack media cache file")?;
            let mut response = response;
            let mut size = 0_u64;
            while let Some(chunk) = response
                .chunk()
                .await
                .context("failed to read Slack media bytes")?
            {
                size = size
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| SlackError::validation("Slack media is larger than 1 GiB"))?;
                ensure_media_size(Some(size))?;
                file.write_all(&chunk)
                    .await
                    .context("failed to write the Slack media cache file")?;
            }
            file.flush()
                .await
                .context("failed to flush the Slack media cache file")?;
            drop(file);
            tokio::fs::rename(&partial_path, destination)
                .await
                .context("failed to finalize the Slack media cache file")?;
            Ok::<_, SlackError>(size)
        }
        .await;

        match result {
            Ok(size) => Ok(DownloadedMedia {
                path: destination.to_path_buf(),
                mime_type,
                size,
            }),
            Err(error) => {
                let _ = tokio::fs::remove_file(&partial_path).await;
                Err(error)
            }
        }
    }

    /// Downloads a private Slack attachment to a local cache path. Credentials
    /// are only attached after the URL has been restricted to Slack-owned HTTPS
    /// hosts, so an attachment can never forward the session to another host.
    pub async fn download_attachment<F>(
        &self,
        url: &str,
        destination: &Path,
        progress: F,
    ) -> Result<DownloadedAttachment>
    where
        F: Fn(DownloadProgressUpdate),
    {
        ensure_trusted_slack_download_url(url)?;

        if let Ok(metadata) = tokio::fs::metadata(destination).await {
            if metadata.is_file() && metadata.len() > 0 {
                ensure_attachment_size(Some(metadata.len()))?;
                progress(DownloadProgressUpdate::new(1.0, "Attachment ready"));
                return Ok(DownloadedAttachment {
                    path: destination.to_path_buf(),
                    size: metadata.len(),
                });
            }
        }

        progress(DownloadProgressUpdate::new(0.05, "Starting download"));
        let response = send_tracked_slack_request(self.authenticated_request(Method::GET, url))
            .await
            .context("failed to download Slack attachment")?
            .error_for_status()
            .context("Slack attachment returned an HTTP error")?;
        let expected_size = response.content_length();
        ensure_attachment_size(expected_size)?;

        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("failed to create the Slack attachment cache directory")?;
        }

        let partial_path = partial_download_path(destination);
        let result = async {
            let mut file = tokio::fs::File::create(&partial_path)
                .await
                .context("failed to create the Slack attachment cache file")?;
            let mut response = response;
            let mut size = 0_u64;
            while let Some(chunk) = response
                .chunk()
                .await
                .context("failed to read Slack attachment bytes")?
            {
                size = size.checked_add(chunk.len() as u64).ok_or_else(|| {
                    SlackError::validation("Slack attachment is larger than 1 GiB")
                })?;
                ensure_attachment_size(Some(size))?;
                file.write_all(&chunk)
                    .await
                    .context("failed to write the Slack attachment cache file")?;
                if let Some(total) = expected_size.filter(|total| *total > 0) {
                    let fraction = 0.05 + 0.9 * (size as f64 / total as f64).min(1.0);
                    progress(DownloadProgressUpdate::new(
                        fraction,
                        "Downloading attachment",
                    ));
                }
            }
            file.flush()
                .await
                .context("failed to flush the Slack attachment cache file")?;
            drop(file);
            tokio::fs::rename(&partial_path, destination)
                .await
                .context("failed to finalize the Slack attachment cache file")?;
            Ok::<_, SlackError>(size)
        }
        .await;

        match result {
            Ok(size) => {
                progress(DownloadProgressUpdate::new(1.0, "Attachment ready"));
                Ok(DownloadedAttachment {
                    path: destination.to_path_buf(),
                    size,
                })
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&partial_path).await;
                Err(error)
            }
        }
    }

    pub async fn post_message(
        &self,
        channel_id: &str,
        text: &str,
        blocks_json: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<SlackMessage> {
        let client_msg_id = next_client_message_id();
        let params = post_message_params(channel_id, text, blocks_json, thread_ts, &client_msg_id);

        let response: PostMessageResponse = self.post_form("chat.postMessage", &params).await?;
        let mut message = response.message;
        message.client_msg_id.get_or_insert(client_msg_id);
        if message.blocks.is_none() {
            if let Some(blocks) = blocks_json
                .filter(|blocks| !blocks.trim().is_empty())
                .and_then(|blocks| serde_json::from_str::<Value>(blocks).ok())
            {
                message.blocks = Some(blocks);
                message.refresh_canonical_content();
            }
        }
        Ok(message)
    }

    pub async fn update_message(
        &self,
        channel_id: &str,
        original: &SlackMessage,
        text: &str,
        blocks_json: Option<&str>,
    ) -> Result<SlackMessage> {
        let params = update_message_params(channel_id, &original.ts, text, blocks_json);
        let response: UpdateMessageResponse = self.post_form("chat.update", &params).await?;
        Ok(merge_updated_message(original, text, blocks_json, response))
    }

    pub async fn message_permalink(&self, channel_id: &str, message_ts: &str) -> Result<String> {
        let response: MessagePermalinkResponse = self
            .post_form(
                "chat.getPermalink",
                &[
                    ("channel", channel_id.to_string()),
                    ("message_ts", message_ts.to_string()),
                ],
            )
            .await?;
        response
            .permalink
            .ok_or_else(|| SlackError::validation("Slack did not return a message permalink"))
    }

    pub async fn set_reaction(
        &self,
        channel_id: &str,
        ts: &str,
        name: &str,
        add: bool,
    ) -> Result<()> {
        let method = if add {
            "reactions.add"
        } else {
            "reactions.remove"
        };
        let _: BasicResponse = self
            .post_form(
                method,
                &[
                    ("channel", channel_id.to_string()),
                    ("timestamp", ts.to_string()),
                    ("name", name.to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn set_saved(&self, channel_id: &str, ts: &str, add: bool) -> Result<()> {
        let method = if add { "stars.add" } else { "stars.remove" };
        let _: BasicResponse = self
            .post_form(
                method,
                &[
                    ("channel", channel_id.to_string()),
                    ("timestamp", ts.to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn set_conversation_starred(&self, channel_id: &str, starred: bool) -> Result<()> {
        let channel_id = channel_id.trim();
        if channel_id.is_empty() {
            return Err(SlackError::validation("conversation ID is required"));
        }
        let method = if starred { "stars.add" } else { "stars.remove" };
        let response: Result<BasicResponse> = self
            .post_form(method, &[("channel", channel_id.to_string())])
            .await;
        match response {
            Ok(_) => Ok(()),
            Err(SlackError::Api { code, .. })
                if (starred && code == "already_starred")
                    || (!starred && code == "not_starred") =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn mark_read(&self, channel_id: &str, ts: &str) -> Result<()> {
        let _: BasicResponse = self
            .post_form(
                "conversations.mark",
                &[("channel", channel_id.to_string()), ("ts", ts.to_string())],
            )
            .await?;
        Ok(())
    }

    pub async fn upload_files<F>(
        &self,
        channel_id: &str,
        thread_ts: Option<&str>,
        paths: &[PathBuf],
        blocks_json: Option<&str>,
        progress: F,
    ) -> Result<Vec<SlackFile>>
    where
        F: Fn(UploadProgressUpdate),
    {
        if paths.is_empty() {
            return Err(SlackError::validation("select at least one file"));
        }
        let count = paths.len() as f64;
        let mut pending_files = Vec::with_capacity(paths.len());

        for (index, path) in paths.iter().enumerate() {
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| SlackError::validation("file path has no valid filename"))?
                .to_string();
            let metadata = tokio::fs::metadata(path)
                .await
                .with_context(|| format!("failed to inspect {}", path.display()))?;
            if metadata.len() > MAX_UPLOAD_BYTES {
                return Err(SlackError::validation(format!(
                    "{} is larger than 1 GiB",
                    path.display()
                )));
            }

            let base = index as f64 / count * 0.85;
            let span = 0.85 / count;
            progress(UploadProgressUpdate::new(
                base + span * 0.15,
                &format!("Reading {filename}"),
            ));
            let bytes = tokio::fs::read(path)
                .await
                .with_context(|| format!("failed to read {}", path.display()))?;

            progress(UploadProgressUpdate::new(
                base + span * 0.35,
                "Requesting upload URL",
            ));
            let upload: UploadUrlResponse = self
                .post_form(
                    "files.getUploadURLExternal",
                    &[
                        ("filename", filename.clone()),
                        ("length", bytes.len().to_string()),
                    ],
                )
                .await?;

            progress(UploadProgressUpdate::new(
                base + span * 0.60,
                &format!("Uploading {filename}"),
            ));
            send_tracked_slack_request(self.http.post(&upload.upload_url).body(bytes))
                .await
                .context("failed to upload file bytes to Slack upload URL")?
                .error_for_status()
                .context("Slack upload URL returned an HTTP error")?;
            pending_files.push(json!({ "id": upload.file_id, "title": filename }));
            progress(UploadProgressUpdate::new(base + span, "File data uploaded"));
        }

        progress(UploadProgressUpdate::new(0.90, "Completing upload"));
        let files = Value::Array(pending_files).to_string();
        let params = complete_upload_params(files, channel_id, thread_ts, blocks_json);
        let complete: CompleteUploadResponse = self
            .post_form("files.completeUploadExternal", &params)
            .await?;

        progress(UploadProgressUpdate::new(1.0, "Upload complete"));
        if complete.files.is_empty() {
            return Err(SlackError::validation(
                "Slack did not return uploaded file metadata",
            ));
        }
        Ok(complete.files)
    }

    async fn post_form<T>(&self, method: &str, params: &[(&str, String)]) -> Result<T>
    where
        T: for<'de> Deserialize<'de> + SlackResponse,
    {
        let url = format!("{}/{method}", self.api_base_url.trim_end_matches('/'));
        let mut form = params.to_vec();
        if self.browser_cookie_d.is_some() && !form.iter().any(|(key, _)| *key == "token") {
            form.push(("token", self.access_token.clone()));
        }
        let mut retries = 0;

        loop {
            let response = send_tracked_slack_request(
                self.authenticated_request(Method::POST, &url)
                    .timeout(API_REQUEST_TIMEOUT)
                    .form(&form),
            )
            .await
            .with_context(|| format!("failed to call Slack method {method}"))?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = retry_after_delay(&response);
                if retries >= MAX_RATE_LIMIT_RETRIES {
                    crate::debug::log(
                        "slack",
                        &format!("Slack method {method} rate limited; not retrying automatically"),
                    );
                    return Err(SlackError::RateLimited {
                        method: method.to_string(),
                    });
                }
                retries += 1;
                crate::debug::log(
                    "slack",
                    &format!(
                        "Slack method {method} rate limited; retrying in {}s",
                        retry_after.as_secs()
                    ),
                );
                tokio::time::sleep(retry_after).await;
                continue;
            }

            let response = response
                .error_for_status()
                .with_context(|| format!("Slack method {method} returned an HTTP error"))?
                .json::<T>()
                .await
                .with_context(|| format!("failed to parse Slack method {method} response"))?;

            return response.into_result(method);
        }
    }

    async fn star_items(&self) -> Result<Vec<SavedItem>> {
        let mut cursor: Option<String> = None;
        let mut items = Vec::new();
        loop {
            let mut params = vec![("limit", "200".to_string())];
            if let Some(cursor) = cursor.as_ref() {
                params.push(("cursor", cursor.clone()));
            }
            let response: StarsListResponse = self.post_form("stars.list", &params).await?;
            items.extend(response.items);
            cursor = next_cursor(response.response_metadata);
            if cursor.is_none() {
                return Ok(items);
            }
        }
    }

    async fn post_browser_form<T>(
        &self,
        api_base_url: &str,
        method: &str,
        query_params: &[(&'static str, String)],
        params: &[(&'static str, String)],
    ) -> Result<T>
    where
        T: for<'de> Deserialize<'de> + SlackResponse,
    {
        let (cookie, user_agent) = self.browser_session_headers()?;
        let mut url =
            reqwest::Url::parse(&format!("{}/{method}", api_base_url.trim_end_matches('/')))
                .map_err(|_| SlackError::validation("Slack browser API URL is invalid"))?;
        if !query_params.is_empty() {
            let mut query = url.query_pairs_mut();
            for (key, value) in query_params {
                query.append_pair(key, value);
            }
        }

        let mut form = Form::new().text("token", self.access_token.clone());
        for (key, value) in params {
            form = form.text(*key, value.clone());
        }
        let mut request = self
            .http
            .post(url)
            .timeout(API_REQUEST_TIMEOUT)
            .header(COOKIE, cookie)
            .multipart(form);
        if let Some(user_agent) = user_agent {
            request = request.header(USER_AGENT, user_agent);
        }
        let response = send_tracked_slack_request(request)
            .await
            .with_context(|| format!("failed to call Slack method {method}"))?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            return Err(SlackError::RateLimited {
                method: method.to_string(),
            });
        }
        let response = response
            .error_for_status()
            .with_context(|| format!("Slack method {method} returned an HTTP error"))?
            .json::<T>()
            .await
            .with_context(|| format!("failed to parse Slack method {method} response"))?;

        response.into_result(method)
    }

    fn ensure_browser_session_credentials(&self) -> Result<()> {
        if self
            .browser_cookie_d
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
            || self.access_token.trim().is_empty()
        {
            return Err(SlackError::validation(
                "browser session credentials are unavailable",
            ));
        }
        Ok(())
    }

    fn browser_session_headers(&self) -> Result<(String, Option<&str>)> {
        self.ensure_browser_session_credentials()?;
        let cookie = self
            .browser_cookie_d
            .as_deref()
            .map(str::trim)
            .filter(|cookie| !cookie.is_empty())
            .ok_or_else(|| SlackError::validation("browser session credentials are unavailable"))?;
        let user_agent = self
            .user_agent
            .as_deref()
            .map(str::trim)
            .filter(|user_agent| !user_agent.is_empty());
        Ok((browser_session_cookie_header(cookie), user_agent))
    }

    fn browser_workspace_api_base_url(&self, workspace_url: &str) -> Result<String> {
        let workspace_api_base_url = validated_workspace_api_base_url(workspace_url)?;
        if self.api_base_url != SLACK_API_BASE_URL {
            return Ok(self.api_base_url.trim_end_matches('/').to_string());
        }
        Ok(workspace_api_base_url)
    }

    fn authenticated_request(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(&self.access_token);

        if let Some(cookie) = self
            .browser_cookie_d
            .as_deref()
            .map(str::trim)
            .filter(|cookie| !cookie.is_empty())
        {
            request = request.header(COOKIE, browser_session_cookie_header(cookie));
        }

        if let Some(user_agent) = self
            .user_agent
            .as_deref()
            .map(str::trim)
            .filter(|user_agent| !user_agent.is_empty())
        {
            request = request.header(USER_AGENT, user_agent);
        }

        request
    }
}

fn validated_workspace_api_base_url(workspace_url: &str) -> Result<String> {
    let workspace_url = workspace_url.trim();
    let mut parsed = reqwest::Url::parse(workspace_url)
        .map_err(|_| SlackError::validation("Slack workspace URL is invalid"))?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
        || parsed.port().is_some_and(|port| port != 443)
    {
        return Err(SlackError::validation("Slack workspace URL is not trusted"));
    }
    let host = parsed
        .host_str()
        .map(str::to_ascii_lowercase)
        .filter(|host| is_slack_workspace_host(host))
        .ok_or_else(|| SlackError::validation("Slack workspace URL is not trusted"))?;
    parsed
        .set_port(None)
        .map_err(|_| SlackError::validation("Slack workspace URL is not trusted"))?;
    Ok(format!("https://{host}/api"))
}

fn is_slack_workspace_host(host: &str) -> bool {
    let Some(workspace) = host.strip_suffix(".slack.com") else {
        return false;
    };
    !workspace.is_empty()
        && workspace.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn validated_slack_route(route: Option<String>) -> Result<String> {
    route
        .filter(|route| {
            !route.trim().is_empty()
                && route.trim() == route
                && route.len() <= MAX_SLACK_ROUTE_BYTES
                && !route.chars().any(char::is_control)
        })
        .ok_or_else(|| SlackError::validation("Slack unread routing metadata is unavailable"))
}

fn normalize_open_im_ids(ims: Vec<BrowserBootIm>) -> Result<HashSet<String>> {
    let mut open_ims = HashSet::new();
    for im in ims {
        let id = im.id.trim();
        if id.is_empty() {
            return Err(SlackError::validation(
                "Slack unread snapshot contains an invalid conversation record",
            ));
        }
        if im.is_open {
            open_ims.insert(id.to_string());
        }
    }
    Ok(open_ims)
}

fn normalize_browser_unread_snapshot(
    counts: BrowserCountsResponse,
    open_ims: &HashSet<String>,
) -> Result<SlackUnreadSnapshot> {
    let channels = normalize_browser_unread_records(counts.channels, &HashSet::new())?;
    let ims = normalize_browser_unread_records(counts.ims, open_ims)?;
    let mpims = normalize_browser_unread_records(counts.mpims, &HashSet::new())?;
    if channels.is_empty() && ims.is_empty() && mpims.is_empty() {
        return Err(SlackError::validation(
            "Slack unread snapshot did not contain conversation records",
        ));
    }

    let mut seen = HashSet::new();
    if channels
        .iter()
        .chain(&ims)
        .chain(&mpims)
        .any(|record| !seen.insert(record.conversation_id.as_str()))
    {
        return Err(SlackError::validation(
            "Slack unread snapshot contains duplicate conversation records",
        ));
    }

    Ok(SlackUnreadSnapshot {
        channels,
        ims,
        mpims,
    })
}

fn normalize_browser_unread_records(
    records: Vec<BrowserCountRecord>,
    open_ids: &HashSet<String>,
) -> Result<Vec<SlackUnreadSnapshotRecord>> {
    records
        .into_iter()
        .map(|record| normalize_browser_unread_record(record, open_ids))
        .collect()
}

fn normalize_browser_unread_record(
    record: BrowserCountRecord,
    open_ids: &HashSet<String>,
) -> Result<SlackUnreadSnapshotRecord> {
    let conversation_id = record.id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(SlackError::validation(
            "Slack unread snapshot contains an invalid conversation record",
        ));
    }
    let last_read = normalized_optional_string(record.last_read);
    let latest = normalized_optional_string(record.latest);
    let has_unreads = match record.has_unreads {
        Some(has_unreads) => has_unreads,
        None => {
            let (Some(last_read), Some(latest)) = (last_read.as_deref(), latest.as_deref()) else {
                return Err(SlackError::validation(
                    "Slack unread snapshot record is missing unread state",
                ));
            };
            let (Some(last_read), Some(latest)) =
                (parse_slack_ts(last_read), parse_slack_ts(latest))
            else {
                return Err(SlackError::validation(
                    "Slack unread snapshot record has invalid read cursors",
                ));
            };
            latest > last_read
        }
    };

    Ok(SlackUnreadSnapshotRecord {
        is_open: open_ids.contains(&conversation_id),
        conversation_id,
        last_read,
        latest,
        has_unreads,
        mention_count: record.mention_count,
    })
}

fn normalized_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

fn post_message_params(
    channel_id: &str,
    text: &str,
    blocks_json: Option<&str>,
    thread_ts: Option<&str>,
    client_msg_id: &str,
) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("channel", channel_id.to_string()),
        ("text", text.to_string()),
        ("client_msg_id", client_msg_id.to_string()),
    ];
    if let Some(blocks_json) = blocks_json.filter(|blocks| !blocks.trim().is_empty()) {
        params.push(("blocks", blocks_json.to_string()));
    }
    if let Some(thread_ts) = thread_ts.filter(|thread_ts| !thread_ts.trim().is_empty()) {
        params.push(("thread_ts", thread_ts.to_string()));
    }
    params
}

fn update_message_params(
    channel_id: &str,
    message_ts: &str,
    text: &str,
    blocks_json: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("channel", channel_id.to_string()),
        ("ts", message_ts.to_string()),
        ("text", text.to_string()),
    ];
    if let Some(blocks_json) = blocks_json.filter(|blocks| !blocks.trim().is_empty()) {
        params.push(("blocks", blocks_json.to_string()));
    }
    params
}

fn merge_updated_message(
    original: &SlackMessage,
    submitted_text: &str,
    submitted_blocks: Option<&str>,
    response: UpdateMessageResponse,
) -> SlackMessage {
    let response_message = response.message.as_ref().and_then(Value::as_object);
    let response_string = |field: &str| {
        response_message
            .and_then(|message| message.get(field))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    };
    let mut message = original.clone();
    message.ts = response
        .ts
        .filter(|ts| !ts.trim().is_empty())
        .unwrap_or_else(|| original.ts.clone());
    message.text = response_string("text")
        .or(response.text)
        .or_else(|| Some(submitted_text.to_string()));
    message.user = response_string("user").or_else(|| original.user.clone());
    message.blocks = response_message
        .and_then(|value| value.get("blocks"))
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| submitted_blocks.and_then(|blocks| serde_json::from_str(blocks).ok()));
    message.edited = response_message
        .and_then(|value| value.get("edited"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .or_else(|| original.edited.clone())
        .or_else(|| {
            Some(SlackMessageEdit {
                user: message.user.clone(),
                ts: None,
            })
        });
    message.refresh_canonical_content();
    message
}

fn complete_upload_params(
    files: String,
    channel_id: &str,
    thread_ts: Option<&str>,
    blocks_json: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut params = vec![("files", files), ("channel_id", channel_id.to_string())];
    if let Some(thread_ts) = thread_ts.filter(|thread_ts| !thread_ts.trim().is_empty()) {
        params.push(("thread_ts", thread_ts.to_string()));
    }
    if let Some(blocks_json) = blocks_json.filter(|blocks| !blocks.trim().is_empty()) {
        params.push(("blocks", blocks_json.to_string()));
    }
    params
}

fn conversation_user_ids_param(user_ids: &[String], maximum: usize) -> Result<String> {
    let mut user_ids = user_ids
        .iter()
        .map(|user_id| user_id.trim())
        .filter(|user_id| !user_id.is_empty())
        .collect::<Vec<_>>();
    user_ids.sort_unstable();
    user_ids.dedup();
    if user_ids.is_empty() {
        return Err(SlackError::validation("select at least one person"));
    }
    if user_ids.len() > maximum {
        return Err(SlackError::validation(format!(
            "select no more than {maximum} people"
        )));
    }
    Ok(user_ids.join(","))
}

fn channel_creation_params(name: &str, is_private: bool) -> Result<Vec<(&'static str, String)>> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > 80
        || !name.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        })
    {
        return Err(SlackError::validation(
            "channel names must use lowercase letters, numbers, hyphens, or underscores",
        ));
    }
    Ok(vec![
        ("name", name.to_string()),
        ("is_private", is_private.to_string()),
    ])
}

fn paginated_list_params(
    cursor: Option<&str>,
    include_channel_types: bool,
) -> Vec<(&'static str, String)> {
    let mut params = Vec::with_capacity(4);
    if include_channel_types {
        params.push(("types", "public_channel,private_channel".to_string()));
        params.push(("exclude_archived", "true".to_string()));
    }
    params.push(("limit", "200".to_string()));
    if let Some(cursor) = cursor.map(str::trim).filter(|cursor| !cursor.is_empty()) {
        params.push(("cursor", cursor.to_string()));
    }
    params
}

fn next_cursor(metadata: Option<ResponseMetadata>) -> Option<String> {
    metadata
        .and_then(|metadata| metadata.next_cursor)
        .map(|cursor| cursor.trim().to_string())
        .filter(|cursor| !cursor.is_empty())
}

fn is_discoverable_conversation(conversation: &SlackConversation) -> bool {
    !conversation.is_archived.unwrap_or(false)
        && (conversation.is_channel.unwrap_or(false)
            || conversation.is_group.unwrap_or(false)
            || conversation.is_private.unwrap_or(false))
        && !conversation.is_im.unwrap_or(false)
        && !conversation.is_mpim.unwrap_or(false)
}

fn retry_after_delay(response: &reqwest::Response) -> Duration {
    let seconds = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(retry_after_seconds)
        .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS);

    Duration::from_secs(seconds)
}

fn retry_after_seconds(value: &str) -> u64 {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS)
        .min(MAX_RETRY_AFTER_SECONDS)
}

fn history_request_params(
    channel_id: &str,
    cursor: Option<&str>,
    limit: usize,
    include_unreads: bool,
) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("channel", channel_id.to_string()),
        ("limit", limit.to_string()),
    ];
    if let Some(cursor) = cursor.filter(|cursor| !cursor.trim().is_empty()) {
        params.push(("cursor", cursor.to_string()));
    } else if include_unreads {
        params.push(("unreads", "true".to_string()));
    }
    params
}

fn message_context_request_params(
    channel_id: &str,
    message_ts: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("channel", channel_id.to_string()),
        ("latest", message_ts.to_string()),
        ("inclusive", "true".to_string()),
        ("limit", MESSAGE_CONTEXT_LIMIT.to_string()),
    ]
}

fn thread_message_context_request_params(
    channel_id: &str,
    thread_ts: &str,
    message_ts: &str,
) -> Vec<(&'static str, String)> {
    let mut params = message_context_request_params(channel_id, message_ts);
    params.push(("ts", thread_ts.to_string()));
    params
}

fn token_scope_set(scope: Option<&str>) -> HashSet<String> {
    scope
        .unwrap_or_default()
        .split(|character: char| character == ',' || character.is_ascii_whitespace())
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[derive(Debug, Clone)]
pub struct SlackMessagePage {
    pub messages: Vec<SlackMessage>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub unread_state: SlackUnreadState,
}

impl SlackMessagePage {
    fn from_response(
        response: HistoryResponse,
        normalize_messages: impl FnOnce(Vec<SlackMessage>) -> Vec<SlackMessage>,
    ) -> Self {
        let unread_state = unread_state_from_history_response(&response);
        let next_cursor = response
            .response_metadata
            .and_then(|metadata| metadata.next_cursor)
            .and_then(|cursor| {
                let cursor = cursor.trim().to_string();
                (!cursor.is_empty()).then_some(cursor)
            });
        let has_more = response.has_more.unwrap_or(false) || next_cursor.is_some();

        Self {
            messages: normalize_messages(response.messages),
            has_more,
            next_cursor,
            unread_state,
        }
    }
}

fn unread_state_from_history_response(response: &HistoryResponse) -> SlackUnreadState {
    let display_count = response
        .unread_count_display
        .or_else(|| {
            response
                .unread_count_string
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok())
        })
        .unwrap_or_else(|| response.unread_count.unwrap_or_default());
    let has_unread = response.has_unreads.unwrap_or(false)
        || response.is_unread.unwrap_or(false)
        || response.unread_count.is_some_and(|count| count > 0)
        || display_count > 0;
    let known = response.unread_count.is_some()
        || response.unread_count_display.is_some()
        || response.unread_count_string.is_some()
        || response.has_unreads.is_some()
        || response.is_unread.is_some();

    SlackUnreadState::from_parts(known, has_unread, display_count)
}

fn unread_state_from_last_read(last_read: &str, latest_ts: &str) -> SlackUnreadState {
    SlackUnreadState::from_parts(true, slack_ts_is_after(latest_ts, last_read), 0)
}

fn slack_ts_is_after(left: &str, right: &str) -> bool {
    match (parse_slack_ts(left), parse_slack_ts(right)) {
        (Some(left), Some(right)) => left > right,
        _ => left > right,
    }
}

fn parse_slack_ts(value: &str) -> Option<(u64, u64)> {
    let (seconds, micros) = value.trim().split_once('.')?;
    Some((seconds.parse().ok()?, micros.parse().ok()?))
}

fn conversation_last_read_ts(conversation: &SlackConversation) -> Option<&str> {
    conversation
        .extra
        .get("last_read")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn conversation_latest_ts(conversation: &SlackConversation) -> Option<&str> {
    let latest = conversation.extra.get("latest")?;
    match latest {
        Value::String(value) => Some(value.as_str()),
        Value::Object(object) => object.get("ts").and_then(Value::as_str),
        _ => None,
    }
    .map(str::trim)
    .filter(|value| !value.is_empty())
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PreviewAssetMime {
    Png,
    Jpeg,
    Gif,
    Webp,
    Avif,
    Mp4,
    Webm,
    Quicktime,
    Ogg,
}

impl PreviewAssetMime {
    pub(crate) const ALL: [Self; 9] = [
        Self::Png,
        Self::Jpeg,
        Self::Gif,
        Self::Webp,
        Self::Avif,
        Self::Mp4,
        Self::Webm,
        Self::Quicktime,
        Self::Ogg,
    ];

    pub(crate) fn parse(content_type: &str) -> Option<Self> {
        let normalized = content_type.split(';').next()?.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            "image/avif" => Some(Self::Avif),
            "video/mp4" => Some(Self::Mp4),
            "video/webm" => Some(Self::Webm),
            "video/quicktime" => Some(Self::Quicktime),
            "video/ogg" => Some(Self::Ogg),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Avif => "image/avif",
            Self::Mp4 => "video/mp4",
            Self::Webm => "video/webm",
            Self::Quicktime => "video/quicktime",
            Self::Ogg => "video/ogg",
        }
    }

    pub(crate) const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Mp4 => "mp4",
            Self::Webm => "webm",
            Self::Quicktime => "mov",
            Self::Ogg => "ogv",
        }
    }

    pub(crate) const fn is_video(self) -> bool {
        matches!(self, Self::Mp4 | Self::Webm | Self::Quicktime | Self::Ogg)
    }

    pub(crate) const fn max_bytes(self) -> usize {
        if self.is_video() {
            MAX_PREVIEW_VIDEO_BYTES
        } else {
            MAX_PREVIEW_IMAGE_BYTES
        }
    }

    pub(crate) const fn validate_size(self, size: usize) -> bool {
        size > 0 && size <= self.max_bytes()
    }

    pub(crate) fn validate_bytes(self, bytes: &[u8]) -> bool {
        self.validate_size(bytes.len()) && self.validate_signature(bytes)
    }

    pub(crate) fn validate_cached_content(self, size: u64, prefix: &[u8]) -> bool {
        usize::try_from(size)
            .ok()
            .is_some_and(|size| self.validate_size(size))
            && self.validate_signature(prefix)
    }

    pub(crate) fn is_valid_payload(self, bytes: &[u8]) -> bool {
        self.validate_bytes(bytes)
    }

    fn validate_signature(self, bytes: &[u8]) -> bool {
        match self {
            Self::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => bytes.starts_with(b"\xff\xd8\xff"),
            Self::Gif => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
            Self::Webp => {
                bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP"
            }
            Self::Avif => iso_base_media_has_brand(bytes, is_avif_brand),
            Self::Mp4 => iso_base_media_has_brand(bytes, is_mp4_brand),
            Self::Quicktime => iso_base_media_has_brand(bytes, is_quicktime_brand),
            Self::Webm => bytes.starts_with(b"\x1a\x45\xdf\xa3"),
            Self::Ogg => bytes.starts_with(b"OggS"),
        }
    }
}

impl std::fmt::Display for PreviewAssetMime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct DownloadedPreviewAsset {
    pub(crate) mime_type: PreviewAssetMime,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DownloadedMedia {
    pub path: PathBuf,
    pub mime_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DownloadedAttachment {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DownloadProgressUpdate {
    pub fraction: f64,
    pub label: String,
}

impl DownloadProgressUpdate {
    fn new(fraction: f64, label: &str) -> Self {
        Self {
            fraction,
            label: label.to_string(),
        }
    }
}

fn is_trusted_slack_download_url(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    url.host_str().is_some_and(|host| {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        host == "slack.com" || host.ends_with(".slack.com")
    })
}

fn is_trusted_avatar_url(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    url.host_str().is_some_and(|host| {
        matches!(
            host.trim_end_matches('.').to_ascii_lowercase().as_str(),
            "a.slack-edge.com" | "avatars.slack-edge.com" | "secure.gravatar.com"
        )
    })
}

pub(crate) fn supports_native_preview_asset_url(url: &str) -> bool {
    is_trusted_slack_download_url(url) || is_trusted_avatar_url(url)
}

fn ensure_trusted_slack_download_url(url: &str) -> Result<()> {
    if !is_trusted_slack_download_url(url) {
        return Err(SlackError::validation(
            "download URL is not a trusted Slack URL",
        ));
    }
    Ok(())
}

fn append_bounded_preview_chunk(bytes: &mut Vec<u8>, chunk: &[u8], max_bytes: usize) -> Result<()> {
    let next_size = bytes
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| SlackError::validation("Slack attachment preview is too large"))?;
    if next_size > max_bytes {
        return Err(SlackError::validation(
            "Slack attachment preview is too large",
        ));
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

fn supported_media_mime_type(content_type: &str) -> Option<&str> {
    let mime_type = content_type.split(';').next()?.trim();
    (mime_type.starts_with("image/")
        || matches!(
            mime_type,
            "video/mp4" | "video/webm" | "video/quicktime" | "video/x-matroska" | "video/ogg"
        ))
    .then_some(mime_type)
}

#[cfg(test)]
pub(crate) fn supported_preview_mime_type(mime_type: &str) -> bool {
    PreviewAssetMime::parse(mime_type).is_some()
}

fn is_avif_brand(brand: &[u8]) -> bool {
    matches!(brand, b"avif" | b"avis")
}

fn is_mp4_brand(brand: &[u8]) -> bool {
    matches!(
        brand,
        b"isom"
            | b"iso2"
            | b"iso3"
            | b"iso4"
            | b"iso5"
            | b"iso6"
            | b"iso7"
            | b"iso8"
            | b"iso9"
            | b"mp41"
            | b"mp42"
            | b"mp71"
            | b"avc1"
            | b"hvc1"
            | b"hev1"
            | b"dash"
            | b"M4V "
            | b"M4VH"
            | b"M4VP"
            | b"F4V "
            | b"F4P "
            | b"MSNV"
    ) || brand.starts_with(b"3gp")
        || brand.starts_with(b"3g2")
}

fn is_quicktime_brand(brand: &[u8]) -> bool {
    brand == b"qt  "
}

fn iso_base_media_has_brand(bytes: &[u8], mut matches_brand: impl FnMut(&[u8]) -> bool) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    if matches_brand(&bytes[8..12]) {
        return true;
    }
    let compatible_end = bytes.len().min(64);
    bytes
        .get(16..compatible_end)
        .is_some_and(|brands| brands.chunks_exact(4).any(matches_brand))
}

fn ensure_media_size(size: Option<u64>) -> Result<()> {
    if size.is_some_and(|size| size > MAX_MEDIA_DOWNLOAD_BYTES) {
        return Err(SlackError::validation("Slack media is larger than 1 GiB"));
    }
    Ok(())
}

fn ensure_attachment_size(size: Option<u64>) -> Result<()> {
    if size.is_some_and(|size| size > MAX_MEDIA_DOWNLOAD_BYTES) {
        return Err(SlackError::validation(
            "Slack attachment is larger than 1 GiB",
        ));
    }
    Ok(())
}

fn partial_download_path(destination: &Path) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.part", std::process::id()));
    destination.with_file_name(name)
}

#[derive(Debug, Clone)]
pub struct UploadProgressUpdate {
    pub fraction: f64,
    pub label: String,
}

impl UploadProgressUpdate {
    fn new(fraction: f64, label: &str) -> Self {
        Self {
            fraction,
            label: label.to_string(),
        }
    }
}

trait SlackResponse: Sized {
    fn ok(&self) -> bool;
    fn error(&self) -> Option<&str>;

    fn into_result(self, method: &str) -> Result<Self> {
        if self.ok() {
            Ok(self)
        } else {
            Err(SlackError::api(
                method,
                self.error().unwrap_or("unknown_error"),
            ))
        }
    }
}

macro_rules! impl_slack_response {
    ($type_name:ty) => {
        impl SlackResponse for $type_name {
            fn ok(&self) -> bool {
                self.ok
            }

            fn error(&self) -> Option<&str> {
                self.error.as_deref()
            }
        }
    };
}

#[derive(Debug, Deserialize)]
struct ResponseMetadata {
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthTestResponse {
    ok: bool,
    error: Option<String>,
    url: Option<String>,
    team: Option<String>,
    user: Option<String>,
    team_id: Option<String>,
    user_id: Option<String>,
}
impl_slack_response!(AuthTestResponse);

#[derive(Debug, Deserialize)]
struct BrowserUserBootResponse {
    ok: bool,
    error: Option<String>,
    slack_route: Option<String>,
    #[serde(default)]
    ims: Vec<BrowserBootIm>,
}
impl_slack_response!(BrowserUserBootResponse);

#[derive(Debug, Deserialize)]
struct BrowserBootIm {
    id: String,
    #[serde(default)]
    is_open: bool,
}

#[derive(Debug, Deserialize)]
struct BrowserCountsResponse {
    ok: bool,
    error: Option<String>,
    #[serde(default)]
    channels: Vec<BrowserCountRecord>,
    #[serde(default)]
    ims: Vec<BrowserCountRecord>,
    #[serde(default)]
    mpims: Vec<BrowserCountRecord>,
}
impl_slack_response!(BrowserCountsResponse);

#[derive(Debug, Deserialize)]
struct MessageActionResponse {
    ok: bool,
    error: Option<String>,
}
impl_slack_response!(MessageActionResponse);

#[derive(Debug, Deserialize)]
struct BrowserCountRecord {
    id: String,
    last_read: Option<String>,
    latest: Option<String>,
    has_unreads: Option<bool>,
    #[serde(default)]
    mention_count: u64,
}

fn log_conversation_properties(method: &str, conversations: &[SlackConversation]) {
    if !crate::debug::enabled() {
        return;
    }

    crate::debug::log(
        "slack",
        &format!("{method} returned {} conversations", conversations.len()),
    );

    let log_limit = conversation_property_log_limit(
        std::env::var(DEBUG_CONVERSATION_PROPERTIES_ENV)
            .ok()
            .as_deref(),
        conversations.len(),
    );
    if log_limit == 0 {
        crate::debug::log(
            "slack",
            &format!(
                "conversation property logging disabled; set {DEBUG_CONVERSATION_PROPERTIES_ENV}=20 or all to enable"
            ),
        );
        return;
    }

    for conversation in conversations.iter().take(log_limit) {
        let properties = serde_json::to_string_pretty(conversation)
            .unwrap_or_else(|_| format!("{conversation:#?}"));
        crate::debug::log(
            "slack",
            &format!(
                "conversation id={} type={} title={} properties=\n{}",
                conversation.id,
                conversation_debug_kind(conversation),
                conversation.display_name(),
                properties
            ),
        );
    }

    if conversations.len() > log_limit {
        crate::debug::log(
            "slack",
            &format!(
                "conversation property logging truncated at {log_limit}/{}; set {DEBUG_CONVERSATION_PROPERTIES_ENV}=all to log every conversation",
                conversations.len()
            ),
        );
    }
}

fn conversation_property_log_limit(setting: Option<&str>, total: usize) -> usize {
    let Some(setting) = setting.map(str::trim).filter(|setting| !setting.is_empty()) else {
        return 0;
    };

    if setting.eq_ignore_ascii_case("all") {
        return total;
    }

    if setting.eq_ignore_ascii_case("true") || setting == "1" {
        return DEFAULT_DEBUG_CONVERSATION_PROPERTY_LIMIT.min(total);
    }

    setting.parse::<usize>().unwrap_or_default().min(total)
}

fn conversation_debug_kind(conversation: &SlackConversation) -> &'static str {
    if conversation.is_im.unwrap_or(false) {
        "direct_message"
    } else if conversation.is_mpim.unwrap_or(false) {
        "group_direct_message"
    } else if conversation.is_private.unwrap_or(false) || conversation.is_group.unwrap_or(false) {
        "private_channel"
    } else if conversation.is_channel.unwrap_or(false) {
        "public_channel"
    } else {
        "unknown"
    }
}

#[derive(Debug, Deserialize)]
struct ConversationListResponse {
    ok: bool,
    error: Option<String>,
    channels: Vec<SlackConversation>,
    response_metadata: Option<ResponseMetadata>,
}
impl_slack_response!(ConversationListResponse);

#[derive(Debug, Deserialize)]
struct ConversationInfoResponse {
    ok: bool,
    error: Option<String>,
    channel: SlackConversation,
}
impl_slack_response!(ConversationInfoResponse);

#[derive(Debug, Deserialize)]
struct ConversationJoinResponse {
    ok: bool,
    error: Option<String>,
    channel: SlackConversation,
}
impl_slack_response!(ConversationJoinResponse);

#[derive(Debug, Deserialize)]
struct ConversationOpenResponse {
    ok: bool,
    error: Option<String>,
    channel: SlackConversation,
}
impl_slack_response!(ConversationOpenResponse);

#[derive(Debug, Deserialize)]
struct ConversationMembersResponse {
    ok: bool,
    error: Option<String>,
    members: Vec<String>,
    response_metadata: Option<ResponseMetadata>,
}
impl_slack_response!(ConversationMembersResponse);

#[derive(Debug, Deserialize)]
struct UsersListResponse {
    ok: bool,
    error: Option<String>,
    members: Vec<SlackUser>,
    response_metadata: Option<ResponseMetadata>,
}
impl_slack_response!(UsersListResponse);

#[derive(Debug, Deserialize)]
struct HistoryResponse {
    ok: bool,
    error: Option<String>,
    #[serde(deserialize_with = "deserialize_messages")]
    messages: Vec<SlackMessage>,
    has_more: Option<bool>,
    unread_count: Option<u64>,
    unread_count_display: Option<u64>,
    unread_count_string: Option<String>,
    has_unreads: Option<bool>,
    is_unread: Option<bool>,
    response_metadata: Option<ResponseMetadata>,
}
impl_slack_response!(HistoryResponse);

#[derive(Debug, Deserialize)]
struct SearchMessages {
    matches: Vec<SearchMatch>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    ok: bool,
    error: Option<String>,
    messages: SearchMessages,
}
impl_slack_response!(SearchResponse);

#[derive(Debug, Deserialize)]
struct StarsListResponse {
    ok: bool,
    error: Option<String>,
    #[serde(default)]
    items: Vec<SavedItem>,
    response_metadata: Option<ResponseMetadata>,
}
impl_slack_response!(StarsListResponse);

#[derive(Debug, Deserialize)]
struct FilesListResponse {
    ok: bool,
    error: Option<String>,
    files: Vec<SlackFile>,
}
impl_slack_response!(FilesListResponse);

#[derive(Debug, Deserialize)]
struct FileInfoResponse {
    ok: bool,
    error: Option<String>,
    file: SlackFile,
}
impl_slack_response!(FileInfoResponse);

#[derive(Debug, Deserialize)]
struct UserInfoResponse {
    ok: bool,
    error: Option<String>,
    user: SlackUser,
}
impl_slack_response!(UserInfoResponse);

#[derive(Debug, Deserialize)]
struct UserProfileResponse {
    ok: bool,
    error: Option<String>,
    profile: Option<SlackUserProfile>,
}
impl_slack_response!(UserProfileResponse);

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct UserGroupsListResponse {
    ok: bool,
    error: Option<String>,
    usergroups: Vec<SlackUserGroup>,
}
impl_slack_response!(UserGroupsListResponse);

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct EmojiListResponse {
    ok: bool,
    error: Option<String>,
    #[serde(default)]
    emoji: HashMap<String, String>,
}
impl_slack_response!(EmojiListResponse);

#[derive(Debug, Deserialize)]
struct PostMessageResponse {
    ok: bool,
    error: Option<String>,
    #[serde(deserialize_with = "deserialize_message")]
    message: SlackMessage,
}
impl_slack_response!(PostMessageResponse);

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct UpdateMessageResponse {
    ok: bool,
    error: Option<String>,
    channel: Option<String>,
    ts: Option<String>,
    text: Option<String>,
    message: Option<Value>,
}
impl_slack_response!(UpdateMessageResponse);

#[derive(Debug, Deserialize)]
struct MessagePermalinkResponse {
    ok: bool,
    error: Option<String>,
    permalink: Option<String>,
}
impl_slack_response!(MessagePermalinkResponse);

#[derive(Debug, Deserialize)]
struct BasicResponse {
    ok: bool,
    error: Option<String>,
}
impl_slack_response!(BasicResponse);

#[derive(Debug, Deserialize)]
struct UploadUrlResponse {
    ok: bool,
    error: Option<String>,
    upload_url: String,
    file_id: String,
}
impl_slack_response!(UploadUrlResponse);

#[derive(Debug, Deserialize)]
struct CompleteUploadResponse {
    ok: bool,
    error: Option<String>,
    files: Vec<SlackFile>,
}
impl_slack_response!(CompleteUploadResponse);

fn thread_replies_in_history_order(mut messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    // Slack conversations.replies returns the parent first, while conversations.history
    // returns newest-first. Keep these API methods consistent for the message renderer.
    messages.reverse();
    messages
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::thread;

    use super::*;
    use crate::rich_message::SensitiveValue;
    use tiny_http::{Header, Response, Server};

    fn iso_base_media_fixture(major_brand: &[u8; 4], compatible_brands: &[&[u8; 4]]) -> Vec<u8> {
        let size = 16 + compatible_brands.len() * 4;
        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(&(size as u32).to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(major_brand);
        bytes.extend_from_slice(&[0; 4]);
        for brand in compatible_brands {
            bytes.extend_from_slice(*brand);
        }
        bytes
    }

    fn browser_test_token(browser_cookie_d: Option<&str>) -> StoredToken {
        StoredToken {
            access_token: "browser-access-value".to_string(),
            token_type: Some("browser_session".to_string()),
            scope: None,
            refresh_token: None,
            expires_in: None,
            expires_at: None,
            team_id: None,
            team_name: None,
            user_id: None,
            client_id: None,
            browser_cookie_d: browser_cookie_d.map(str::to_string),
            user_agent: Some("Conduit Browser Test Agent".to_string()),
        }
    }

    fn multipart_field_value<'a>(body: &'a str, field: &str) -> Option<&'a str> {
        let marker = format!("name=\"{field}\"");
        let field_start = body.find(&marker)? + marker.len();
        let value_start = body[field_start..].find("\r\n\r\n")? + field_start + 4;
        let value_end = body[value_start..].find("\r\n")? + value_start;
        Some(&body[value_start..value_end])
    }

    fn multipart_field_names(body: &str) -> HashSet<String> {
        body.split("name=\"")
            .skip(1)
            .filter_map(|part| part.split_once('"').map(|(name, _)| name))
            .filter(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
            .map(str::to_string)
            .collect()
    }

    fn request_query_field(path: &str, field: &str) -> Option<String> {
        let query = path.split_once('?')?.1;
        url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(key, value)| (key == field).then(|| value.into_owned()))
    }

    fn user_test_token() -> StoredToken {
        StoredToken {
            access_token: "xoxp-test-token".to_string(),
            token_type: None,
            scope: Some("stars:read,stars:write".to_string()),
            refresh_token: None,
            expires_in: None,
            expires_at: None,
            team_id: None,
            team_name: None,
            user_id: None,
            client_id: None,
            browser_cookie_d: None,
            user_agent: None,
        }
    }

    #[test]
    fn generated_client_message_ids_are_unique_uuid_values() {
        let first = next_client_message_id();
        let second = next_client_message_id();

        assert_ne!(first, second);
        assert_eq!(first.len(), 36);
        assert_eq!(first.as_bytes()[14], b'4');
        assert!(matches!(first.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert_eq!(
            first.chars().filter(|character| *character == '-').count(),
            4
        );
    }

    #[test]
    fn block_message_action_uses_private_browser_dispatch_shape() {
        let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
        let address = server.server_addr();
        let received = thread::spawn(move || {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            request
                .respond(
                    Response::from_string(r#"{"ok":true}"#).with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
                )
                .expect("mock Slack response should be sent");
            (path, body)
        });
        let mut api = SlackApi::new(browser_test_token(Some("browser-cookie-value")));
        api.api_base_url = format!("http://{address}/api");
        let request = SlackMessageActionRequest {
            channel_id: "C123".to_string(),
            message_ts: "1710000000.000100".to_string(),
            thread_ts: None,
            service_id: "B123".to_string(),
            app_id: Some("A123".to_string()),
            bot_user_id: Some("U123".to_string()),
            action: SlackControlAction::Block {
                action: SensitiveValue::new(
                    r#"{"type":"button","block_id":"block","action_id":"approve","value":"opaque","text":{"type":"plain_text","text":"Approve"}}"#,
                ),
            },
        };

        tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.execute_message_action("https://example.slack.com/", "T123", &request))
            .expect("message action should dispatch");
        let (path, body) = received.join().expect("mock Slack server should finish");

        assert_eq!(path, "/api/blocks.actions");
        assert_eq!(multipart_field_value(&body, "service_id"), Some("B123"));
        assert_eq!(multipart_field_value(&body, "app_id"), Some("A123"));
        assert_eq!(
            multipart_field_value(&body, "service_team_id"),
            Some("T123")
        );
        assert!(multipart_field_value(&body, "client_token")
            .is_some_and(|value| value.starts_with("web-")));
        assert_eq!(
            multipart_field_value(&body, "_x_reason"),
            Some("dispatch_action_to_developer")
        );
        let actions: Value = serde_json::from_str(
            multipart_field_value(&body, "actions").expect("actions field should exist"),
        )
        .expect("actions should be JSON");
        assert_eq!(actions[0]["action_id"], "approve");
        let container: Value = serde_json::from_str(
            multipart_field_value(&body, "container").expect("container field should exist"),
        )
        .expect("container should be JSON");
        assert_eq!(container["type"], "message");
        assert_eq!(container["channel_id"], "C123");
        assert!(multipart_field_value(&body, "action_ts").is_none());
    }

    #[test]
    fn legacy_message_action_uses_attachment_dispatch_shape() {
        let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
        let address = server.server_addr();
        let received = thread::spawn(move || {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            request
                .respond(
                    Response::from_string(r#"{"ok":true,"replaced":true}"#).with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
                )
                .expect("mock Slack response should be sent");
            (path, body)
        });
        let mut api = SlackApi::new(browser_test_token(Some("browser-cookie-value")));
        api.api_base_url = format!("http://{address}/api");
        let request = SlackMessageActionRequest {
            channel_id: "C123".to_string(),
            message_ts: "1710000000.000100".to_string(),
            thread_ts: Some("1710000000.000000".to_string()),
            service_id: "B123".to_string(),
            app_id: None,
            bot_user_id: Some("U123".to_string()),
            action: SlackControlAction::LegacyAttachment {
                attachment_id: 2,
                callback_id: SensitiveValue::new("callback"),
                action: SensitiveValue::new(
                    r#"{"type":"button","name":"response","text":"Yes","value":"yes"}"#,
                ),
            },
        };

        tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.execute_message_action("https://example.slack.com/", "T123", &request))
            .expect("message action should dispatch");
        let (path, body) = received.join().expect("mock Slack server should finish");

        assert_eq!(path, "/api/chat.attachmentAction");
        assert_eq!(multipart_field_value(&body, "service_id"), Some("B123"));
        assert_eq!(multipart_field_value(&body, "bot_user_id"), Some("U123"));
        assert_eq!(
            multipart_field_value(&body, "_x_reason"),
            Some("user_attachment_action_dispatch")
        );
        let payload: Value = serde_json::from_str(
            multipart_field_value(&body, "payload").expect("payload field should exist"),
        )
        .expect("payload should be JSON");
        assert_eq!(payload["attachment_id"], 2);
        assert_eq!(payload["callback_id"], "callback");
        assert_eq!(payload["actions"][0]["value"], "yes");
        assert_eq!(payload["thread_ts"], "1710000000.000000");
        assert_eq!(payload["prompt_app_install"], false);
    }

    #[test]
    fn message_permalink_uses_chat_get_permalink_with_exact_message() {
        let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
        let address = server.server_addr();
        let received = thread::spawn(move || {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            request
                .respond(
                    Response::from_string(
                        r#"{"ok":true,"permalink":"https://example.slack.com/archives/C123/p1710000000000100"}"#,
                    )
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
                )
                .expect("mock Slack response should be sent");
            let form = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>();
            (path, form)
        });

        let mut api = SlackApi::new(browser_test_token(None));
        api.api_base_url = format!("http://{address}/api");
        let permalink = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.message_permalink("C123", "1710000000.000100"))
            .expect("message permalink should resolve");

        assert_eq!(
            permalink,
            "https://example.slack.com/archives/C123/p1710000000000100"
        );
        let (path, form) = received.join().expect("mock Slack server should finish");
        assert_eq!(path, "/api/chat.getPermalink");
        assert_eq!(
            form.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from(["channel".to_string(), "message_ts".to_string()])
        );
        assert_eq!(form.get("channel").map(String::as_str), Some("C123"));
        assert_eq!(
            form.get("message_ts").map(String::as_str),
            Some("1710000000.000100")
        );
    }

    #[test]
    fn current_user_status_uses_users_profile_set_with_exact_profile_fields() {
        let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
        let address = server.server_addr();
        let received = thread::spawn(move || {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            request
                .respond(
                    Response::from_string(
                        r#"{"ok":true,"profile":{"display_name":"Vincent","status_text":"Focus time","status_emoji":":headphones:","status_expiration":2000000000}}"#,
                    )
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
                )
                .expect("mock Slack response should be sent");
            let form = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>();
            (path, form)
        });

        let mut api = SlackApi::new(user_test_token());
        api.api_base_url = format!("http://{address}/api");
        let profile = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.set_current_user_status(&SlackUserStatus {
                text: " Focus time ".to_string(),
                emoji: "headphones".to_string(),
                expiration: 2_000_000_000,
            }))
            .expect("current user status should update");

        let (path, form) = received.join().expect("mock Slack server should finish");
        assert_eq!(path, "/api/users.profile.set");
        assert_eq!(
            form.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from(["profile".to_string()])
        );
        let payload: Value = serde_json::from_str(
            form.get("profile")
                .expect("profile JSON should be included in the form"),
        )
        .expect("profile form value should be JSON");
        assert_eq!(
            payload,
            json!({
                "status_text": "Focus time",
                "status_emoji": ":headphones:",
                "status_expiration": 2_000_000_000_i64,
            })
        );
        assert_eq!(profile.display_name.as_deref(), Some("Vincent"));
        assert_eq!(
            profile.status(),
            Some(SlackUserStatus {
                text: "Focus time".to_string(),
                emoji: ":headphones:".to_string(),
                expiration: 2_000_000_000,
            })
        );
    }

    #[test]
    fn clearing_current_user_status_sends_empty_text_emoji_and_zero_expiration() {
        let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
        let address = server.server_addr();
        let received = thread::spawn(move || {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            request
                .respond(
                    Response::from_string(
                        r#"{"ok":true,"profile":{"status_text":"","status_emoji":"","status_expiration":0}}"#,
                    )
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
                )
                .expect("mock Slack response should be sent");
            let form = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>();
            (path, form)
        });

        let mut api = SlackApi::new(user_test_token());
        api.api_base_url = format!("http://{address}/api");
        let profile = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.set_current_user_status(&SlackUserStatus::default()))
            .expect("current user status should clear");

        let (path, form) = received.join().expect("mock Slack server should finish");
        assert_eq!(path, "/api/users.profile.set");
        let payload: Value = serde_json::from_str(
            form.get("profile")
                .expect("profile JSON should be included in the form"),
        )
        .expect("profile form value should be JSON");
        assert_eq!(
            payload,
            json!({
                "status_text": "",
                "status_emoji": "",
                "status_expiration": 0,
            })
        );
        assert_eq!(profile.status(), None);
    }

    #[test]
    fn current_user_status_rejects_text_longer_than_slacks_limit() {
        let api = SlackApi::new(user_test_token());
        let error = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.set_current_user_status(&SlackUserStatus {
                text: "a".repeat(101),
                ..Default::default()
            }))
            .expect_err("status text longer than 100 characters should be rejected");

        assert_eq!(error.category(), SlackErrorCategory::Validation);
    }

    #[test]
    fn current_user_status_preserves_slack_errors_that_omit_profile() {
        let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
        let address = server.server_addr();
        let received = thread::spawn(move || {
            let request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            request
                .respond(
                    Response::from_string(r#"{"ok":false,"error":"missing_scope"}"#).with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
                )
                .expect("mock Slack response should be sent");
            path
        });

        let mut api = SlackApi::new(user_test_token());
        api.api_base_url = format!("http://{address}/api");
        let error = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.set_current_user_status(&SlackUserStatus {
                text: "Focus time".to_string(),
                ..Default::default()
            }))
            .expect_err("Slack missing-scope response should remain an API error");

        assert!(matches!(
            error,
            SlackError::Api { ref code, .. } if code == "missing_scope"
        ));
        assert_eq!(
            received.join().expect("mock Slack server should finish"),
            "/api/users.profile.set"
        );
    }

    #[test]
    fn message_permalink_validation_is_exact_and_workspace_scoped() {
        let workspace = "https://example.slack.com/";
        let expected = "https://example.slack.com/archives/C123/p1710000000000100";

        assert_eq!(
            constructed_message_permalink(workspace, "C123", "1710000000.0001").as_deref(),
            Some(expected)
        );
        assert_eq!(
            validated_message_permalink(
                &format!("{expected}?thread_ts=1710000000.000000&cid=C123"),
                workspace,
                "C123",
                "1710000000.000100",
            )
            .as_deref(),
            Some(
                "https://example.slack.com/archives/C123/p1710000000000100?thread_ts=1710000000.000000&cid=C123"
            )
        );

        for invalid in [
            "http://example.slack.com/archives/C123/p1710000000000100",
            "https://other.slack.com/archives/C123/p1710000000000100",
            "https://example.slack.com/archives/C999/p1710000000000100",
            "https://example.slack.com/archives/C123/p1710000000000200",
            "https://example.slack.com/archives/C123/p1710000000000100#fragment",
            "https://example.slack.com/archives/C123/p1710000000000100?token=secret",
            "https://example.slack.com/archives/C123/p1710000000000100?cid=C999",
            "https://example.slack.com/archives/C123/p1710000000000100?thread_ts=bad&cid=C123",
        ] {
            assert!(
                validated_message_permalink(invalid, workspace, "C123", "1710000000.000100")
                    .is_none(),
                "{invalid} should be rejected"
            );
        }
        assert!(
            constructed_message_permalink(workspace, "C123/path", "1710000000.000100").is_none()
        );
        assert!(constructed_message_permalink(workspace, "C123", "1710000000.bad").is_none());
        assert!(constructed_message_permalink(
            "https://example.invalid/",
            "C123",
            "1710000000.000100"
        )
        .is_none());
    }

    #[test]
    fn slack_errors_classify_api_failures_for_recovery() {
        let auth = SlackError::api("auth.test", "invalid_auth");
        let rate_limited = SlackError::api("conversations.history", "ratelimited");
        let unexpected = SlackError::api("conversations.history", "fatal_error");

        assert_eq!(auth.category(), SlackErrorCategory::Authentication);
        assert_eq!(rate_limited.category(), SlackErrorCategory::RateLimited);
        assert_eq!(unexpected.category(), SlackErrorCategory::Unexpected);
    }

    #[test]
    fn slack_errors_classify_validation_and_wrapped_sources() {
        let validation = SlackError::validation("download URL is not trusted");
        let timeout = SlackError::from(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "request timed out",
        )));
        let local_io = SlackError::from(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cache is not writable",
        )));

        assert_eq!(validation.category(), SlackErrorCategory::Validation);
        assert_eq!(timeout.category(), SlackErrorCategory::Connectivity);
        assert_eq!(local_io.category(), SlackErrorCategory::LocalIo);
        assert!(matches!(
            &timeout,
            SlackError::Other(source)
                if source.downcast_ref::<std::io::Error>().is_some()
        ));
    }

    #[test]
    fn conversation_mutation_params_validate_and_normalize_input() {
        assert_eq!(
            conversation_user_ids_param(
                &[" U2 ".to_string(), "U1".to_string(), "U2".to_string()],
                8,
            )
            .unwrap(),
            "U1,U2"
        );
        assert!(conversation_user_ids_param(&[], 8).is_err());
        let distinct = (0..9).map(|index| format!("U{index}")).collect::<Vec<_>>();
        assert!(conversation_user_ids_param(&distinct, 8).is_err());
        assert_eq!(
            channel_creation_params("project_alpha-2", true).unwrap(),
            vec![
                ("name", "project_alpha-2".to_string()),
                ("is_private", "true".to_string())
            ]
        );
        assert!(channel_creation_params("Invalid channel", false).is_err());
        assert!(channel_creation_params(&"a".repeat(81), false).is_err());
    }

    fn message(ts: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn workspace_search_adds_prefix_wildcards_and_preserves_modifiers() {
        assert_eq!(
            workspace_search_api_query(
                "  supp bro ui from:ada -in:random has::eyes: \"exact quoted phrase\"  "
            ),
            "supp* bro* ui from:ada -in:random has::eyes: \"exact quoted phrase\""
        );
    }

    #[test]
    fn workspace_search_ignores_quoted_modifier_values_when_filtering_results() {
        assert_eq!(
            workspace_search_content_query("broker from:\"Ada Lovelace\" support"),
            "broker support"
        );
    }

    #[test]
    fn workspace_search_results_match_all_content_substrings() {
        let matches = vec![
            SearchMatch {
                username: Some("Ada Lovelace".to_string()),
                text: Some("The broker needs online payment support".to_string()),
                ..Default::default()
            },
            SearchMatch {
                username: Some("Ada Lovelace".to_string()),
                text: Some("The broker migration is complete".to_string()),
                ..Default::default()
            },
        ];

        let filtered = filter_workspace_search_matches("SUPP bro from:ada", matches);

        assert_eq!(filtered.len(), 1);
        assert_eq!(
            filtered[0].text.as_deref(),
            Some("The broker needs online payment support")
        );
    }

    #[test]
    fn workspace_search_prioritizes_relevance_bands_over_api_order() {
        let matches = vec![
            SearchMatch {
                text: Some("supportive".to_string()),
                ..Default::default()
            },
            SearchMatch {
                text: Some("support".to_string()),
                ..Default::default()
            },
        ];

        let ranked = filter_workspace_search_matches("support", matches);

        assert_eq!(ranked[0].text.as_deref(), Some("support"));
        assert_eq!(ranked[1].text.as_deref(), Some("supportive"));
    }

    #[test]
    fn workspace_search_preserves_api_order_within_a_relevance_band() {
        let matches = vec![
            SearchMatch {
                text: Some("support".to_string()),
                username: Some("Zed".to_string()),
                ..Default::default()
            },
            SearchMatch {
                text: Some("support".to_string()),
                username: Some("Ada".to_string()),
                ..Default::default()
            },
        ];

        let ranked = filter_workspace_search_matches("support", matches);

        assert_eq!(ranked[0].username.as_deref(), Some("Zed"));
        assert_eq!(ranked[1].username.as_deref(), Some("Ada"));
    }

    #[test]
    fn modifier_only_workspace_search_keeps_api_results() {
        let matches = vec![SearchMatch {
            text: Some("Any message".to_string()),
            ..Default::default()
        }];

        assert_eq!(
            filter_workspace_search_matches("from:ada in:general", matches).len(),
            1
        );
    }

    #[test]
    fn thread_replies_are_normalized_to_history_order() {
        let messages = thread_replies_in_history_order(vec![
            message("1710000000.000100"),
            message("1710000100.000100"),
            message("1710000200.000100"),
        ]);
        let timestamps = messages
            .iter()
            .map(|message| message.ts.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            timestamps,
            vec![
                "1710000200.000100",
                "1710000100.000100",
                "1710000000.000100"
            ]
        );
    }

    #[test]
    fn retry_after_seconds_uses_bounded_positive_integer_values() {
        assert_eq!(retry_after_seconds("4"), 4);
        assert_eq!(retry_after_seconds("0"), DEFAULT_RETRY_AFTER_SECONDS);
        assert_eq!(
            retry_after_seconds("not-a-number"),
            DEFAULT_RETRY_AFTER_SECONDS
        );
        assert_eq!(retry_after_seconds("120"), 120);
        assert_eq!(retry_after_seconds("900"), MAX_RETRY_AFTER_SECONDS);
    }

    #[test]
    fn media_content_types_allow_images_and_common_video_formats() {
        assert_eq!(
            supported_media_mime_type("image/jpeg; charset=binary"),
            Some("image/jpeg")
        );
        assert_eq!(supported_media_mime_type("image/avif"), Some("image/avif"));
        assert_eq!(supported_media_mime_type("video/mp4"), Some("video/mp4"));
        assert_eq!(
            supported_media_mime_type("video/webm; codecs=vp9"),
            Some("video/webm")
        );
        assert_eq!(supported_media_mime_type("audio/mpeg"), None);
        assert_eq!(supported_media_mime_type("text/html"), None);
        assert_eq!(supported_media_mime_type("application/octet-stream"), None);
    }

    #[test]
    fn preview_mime_types_use_an_exact_allowlist() {
        let allowed = [
            ("image/png", PreviewAssetMime::Png),
            ("image/jpeg", PreviewAssetMime::Jpeg),
            ("image/gif", PreviewAssetMime::Gif),
            ("image/webp", PreviewAssetMime::Webp),
            ("image/avif", PreviewAssetMime::Avif),
            ("video/mp4", PreviewAssetMime::Mp4),
            ("video/webm", PreviewAssetMime::Webm),
            ("video/quicktime", PreviewAssetMime::Quicktime),
            ("video/ogg", PreviewAssetMime::Ogg),
        ];

        for (mime_type, expected) in allowed {
            assert_eq!(PreviewAssetMime::parse(mime_type), Some(expected));
            assert_eq!(expected.as_str(), mime_type);
            assert!(expected
                .extension()
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()));
            assert!(supported_preview_mime_type(mime_type));
        }
        for mime_type in [
            "image/svg+xml",
            "text/html",
            "application/octet-stream",
            "image/unknown",
        ] {
            assert_eq!(PreviewAssetMime::parse(mime_type), None);
            assert!(!supported_preview_mime_type(mime_type));
        }

        assert_eq!(
            PreviewAssetMime::parse(" Image/PNG; charset=binary "),
            Some(PreviewAssetMime::Png)
        );
        assert_eq!(
            PreviewAssetMime::parse("video/webm; codecs=vp9"),
            Some(PreviewAssetMime::Webm)
        );
        assert_eq!(PreviewAssetMime::ALL.len(), allowed.len());
    }

    #[test]
    fn preview_payload_signatures_must_match_the_declared_mime_type() {
        let avif = iso_base_media_fixture(b"mif1", &[b"avif"]);
        let mp4 = iso_base_media_fixture(b"isom", &[b"mp42"]);
        let quicktime = iso_base_media_fixture(b"qt  ", &[]);
        let valid = [
            (PreviewAssetMime::Png, b"\x89PNG\r\n\x1a\n".as_slice()),
            (PreviewAssetMime::Jpeg, b"\xff\xd8\xff\xe0".as_slice()),
            (PreviewAssetMime::Gif, b"GIF89a".as_slice()),
            (PreviewAssetMime::Webp, b"RIFF\x04\0\0\0WEBP".as_slice()),
            (PreviewAssetMime::Avif, avif.as_slice()),
            (PreviewAssetMime::Mp4, mp4.as_slice()),
            (PreviewAssetMime::Webm, b"\x1a\x45\xdf\xa3".as_slice()),
            (PreviewAssetMime::Quicktime, quicktime.as_slice()),
            (PreviewAssetMime::Ogg, b"OggS".as_slice()),
        ];

        for (mime_type, bytes) in valid {
            assert!(mime_type.is_valid_payload(bytes), "{}", mime_type.as_str());
        }

        assert!(!PreviewAssetMime::Png.is_valid_payload(b"<html>"));
        assert!(!PreviewAssetMime::Jpeg.is_valid_payload(b"\x89PNG\r\n\x1a\n"));
        assert!(!PreviewAssetMime::Avif.is_valid_payload(&mp4));
        assert!(!PreviewAssetMime::Mp4.is_valid_payload(&avif));
        assert!(!PreviewAssetMime::Mp4.is_valid_payload(&quicktime));
        assert!(!PreviewAssetMime::Quicktime.is_valid_payload(&mp4));
        assert!(!PreviewAssetMime::Png.is_valid_payload(&[]));
        assert!(!PreviewAssetMime::Quicktime.is_valid_payload(b"\0\0\0\x08ftyp"));
        assert!(PreviewAssetMime::Png
            .validate_cached_content(MAX_PREVIEW_IMAGE_BYTES as u64, b"\x89PNG\r\n\x1a\n"));
        assert!(!PreviewAssetMime::Png
            .validate_cached_content(MAX_PREVIEW_IMAGE_BYTES as u64 + 1, b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn preview_payload_validation_enforces_per_kind_size_bounds() {
        assert_eq!(PreviewAssetMime::Png.max_bytes(), MAX_PREVIEW_IMAGE_BYTES);
        assert_eq!(PreviewAssetMime::Mp4.max_bytes(), MAX_PREVIEW_VIDEO_BYTES);
        assert!(PreviewAssetMime::Png.validate_size(MAX_PREVIEW_IMAGE_BYTES));
        assert!(!PreviewAssetMime::Png.validate_size(MAX_PREVIEW_IMAGE_BYTES + 1));
        assert!(!PreviewAssetMime::Gif.validate_size(0));
    }

    #[test]
    fn emoji_list_response_preserves_urls_and_aliases() {
        let response: EmojiListResponse = serde_json::from_value(serde_json::json!({
            "ok": true,
            "emoji": {
                "party_parrot": "https://emoji.example/parrot.gif",
                "ship_it": "alias:rocket"
            }
        }))
        .expect("emoji response should parse");

        assert_eq!(
            response.emoji.get("party_parrot").map(String::as_str),
            Some("https://emoji.example/parrot.gif")
        );
        assert_eq!(
            response.emoji.get("ship_it").map(String::as_str),
            Some("alias:rocket")
        );
    }

    #[test]
    fn media_download_size_is_bounded() {
        assert!(ensure_media_size(None).is_ok());
        assert!(ensure_media_size(Some(MAX_MEDIA_DOWNLOAD_BYTES)).is_ok());
        assert!(ensure_media_size(Some(MAX_MEDIA_DOWNLOAD_BYTES + 1)).is_err());
    }

    #[test]
    fn preview_chunks_are_rejected_before_exceeding_the_memory_limit() {
        let mut bytes = vec![1, 2];
        append_bounded_preview_chunk(&mut bytes, &[3, 4], 4).unwrap();
        assert_eq!(bytes, vec![1, 2, 3, 4]);

        let error = append_bounded_preview_chunk(&mut bytes, &[5], 4).unwrap_err();
        assert!(error.to_string().contains("too large"));
        assert_eq!(bytes, vec![1, 2, 3, 4]);
    }

    #[test]
    fn authenticated_downloads_are_restricted_to_slack_https_hosts() {
        assert!(is_trusted_slack_download_url(
            "https://files.slack.com/files-pri/T1-F1/download/report.pdf"
        ));
        assert!(is_trusted_slack_download_url(
            "https://signicat.slack.com/files/U1/F1/report.pdf"
        ));
        assert!(is_trusted_slack_download_url("https://slack.com/file.pdf"));

        assert!(!is_trusted_slack_download_url(
            "http://files.slack.com/file.pdf"
        ));
        assert!(!is_trusted_slack_download_url(
            "https://slack.com.evil.example/file.pdf"
        ));
        assert!(!is_trusted_slack_download_url(
            "https://token@files.slack.com/file.pdf"
        ));
        assert!(!is_trusted_slack_download_url("not a URL"));
        assert!(ensure_trusted_slack_download_url(
            "https://files.slack.com/files-pri/T1-F1/download/report.pdf"
        )
        .is_ok());
        assert!(ensure_trusted_slack_download_url("https://evil.example/preview.png").is_err());
        assert!(supports_native_preview_asset_url(
            "https://files.slack.com/files-pri/T1-F1/download/report.pdf"
        ));
        assert!(!supports_native_preview_asset_url(
            "https://images.example.test/card.png"
        ));
    }

    #[test]
    fn public_avatar_downloads_are_restricted_to_exact_https_hosts() {
        assert!(is_trusted_avatar_url(
            "https://avatars.slack-edge.com/2026-01-01/avatar_72.png"
        ));
        assert!(is_trusted_avatar_url(
            "https://secure.gravatar.com/avatar/hash.jpg"
        ));
        assert!(is_trusted_avatar_url(
            "https://a.slack-edge.com/80588/img/slackbot_72.png"
        ));
        assert!(!is_trusted_avatar_url(
            "http://avatars.slack-edge.com/avatar.png"
        ));
        assert!(!is_trusted_avatar_url(
            "https://avatars.slack-edge.com.evil.example/avatar.png"
        ));
        assert!(!is_trusted_avatar_url(
            "https://token@secure.gravatar.com/avatar/hash.jpg"
        ));
        assert!(supports_native_preview_asset_url(
            "https://avatars.slack-edge.com/2026-01-01/avatar_72.png"
        ));
    }

    #[test]
    fn attachment_download_size_is_bounded() {
        assert!(ensure_attachment_size(None).is_ok());
        assert!(ensure_attachment_size(Some(MAX_MEDIA_DOWNLOAD_BYTES)).is_ok());
        assert!(ensure_attachment_size(Some(MAX_MEDIA_DOWNLOAD_BYTES + 1)).is_err());
    }

    #[test]
    fn completed_upload_targets_requested_thread() {
        let params = complete_upload_params(
            "files-json".to_string(),
            "C123",
            Some("1710000000.000100"),
            None,
        );

        assert!(params.contains(&("channel_id", "C123".to_string())));
        assert!(params.contains(&("thread_ts", "1710000000.000100".to_string())));
    }

    #[test]
    fn completed_batch_upload_uses_rich_blocks_without_initial_comment() {
        let blocks = r#"[{"type":"rich_text"}]"#;
        let params = complete_upload_params(
            "files-json".to_string(),
            "C123",
            Some("1710000000.000100"),
            Some(blocks),
        );

        assert!(params.contains(&("blocks", blocks.to_string())));
        assert!(!params.iter().any(|(name, _)| *name == "initial_comment"));
    }

    #[test]
    fn batch_upload_completes_ordered_files_once_with_rich_blocks() {
        let directory = std::env::temp_dir().join(format!(
            "conduit-batch-upload-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&directory).expect("upload fixture directory should exist");
        let paths = [directory.join("first.txt"), directory.join("second.txt")];
        std::fs::write(&paths[0], b"first").expect("first upload fixture should be written");
        std::fs::write(&paths[1], b"second").expect("second upload fixture should be written");

        let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
        let address = server.server_addr();
        let server_base = format!("http://{address}");
        let upload_base = server_base.clone();
        let received = thread::spawn(move || {
            let mut observations = Vec::new();
            let mut upload_url_index = 0;
            for _ in 0..5 {
                let mut request = server.recv().expect("mock Slack request should arrive");
                let path = request.url().to_string();
                let mut body = String::new();
                request
                    .as_reader()
                    .read_to_string(&mut body)
                    .expect("mock Slack request body should be readable");
                let response = if path == "/api/files.getUploadURLExternal" {
                    upload_url_index += 1;
                    Response::from_string(format!(
                        r#"{{"ok":true,"upload_url":"{upload_base}/upload/{upload_url_index}","file_id":"F{upload_url_index}"}}"#
                    ))
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    )
                } else if path.starts_with("/upload/") {
                    Response::from_string("")
                } else if path == "/api/files.completeUploadExternal" {
                    Response::from_string(
                        r#"{"ok":true,"files":[{"id":"F1","name":"first.txt"},{"id":"F2","name":"second.txt"}]}"#,
                    )
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    )
                } else {
                    panic!("unexpected mock Slack path {path}");
                };
                request
                    .respond(response)
                    .expect("mock Slack response should be sent");
                observations.push((path, body));
            }
            observations
        });

        let blocks = r#"[{"type":"rich_text","elements":[]}]"#;
        let mut api = SlackApi::new(user_test_token());
        api.api_base_url = format!("{server_base}/api");
        let files = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.upload_files("C123", Some("1.0"), &paths, Some(blocks), |_| {}))
            .expect("batch upload should complete");

        assert_eq!(files.len(), 2);
        let observations = received.join().expect("mock Slack server should finish");
        assert_eq!(
            observations
                .iter()
                .map(|(path, _)| path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "/api/files.getUploadURLExternal",
                "/upload/1",
                "/api/files.getUploadURLExternal",
                "/upload/2",
                "/api/files.completeUploadExternal",
            ]
        );
        let complete_body = observations
            .iter()
            .find_map(|(path, body)| (path == "/api/files.completeUploadExternal").then_some(body))
            .expect("completion request should be present");
        let form = url::form_urlencoded::parse(complete_body.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(form.get("channel_id").map(String::as_str), Some("C123"));
        assert_eq!(form.get("thread_ts").map(String::as_str), Some("1.0"));
        assert_eq!(form.get("blocks").map(String::as_str), Some(blocks));
        assert!(!form.contains_key("initial_comment"));
        let uploaded: Value = serde_json::from_str(form.get("files").expect("files form value"))
            .expect("files form value should be JSON");
        assert_eq!(uploaded.as_array().map(Vec::len), Some(2));
        assert_eq!(uploaded[0]["id"], "F1");
        assert_eq!(uploaded[1]["id"], "F2");
        std::fs::remove_dir_all(directory).expect("upload fixtures should be removed");
    }

    #[test]
    fn rich_message_post_keeps_accessible_fallback_and_blocks() {
        let params = post_message_params(
            "C123",
            "Hello <@UADA>",
            Some(r#"[{"type":"rich_text"}]"#),
            Some("1710000000.000100"),
            "client-message-id",
        );

        assert!(params.contains(&("channel", "C123".to_string())));
        assert!(params.contains(&("text", "Hello <@UADA>".to_string())));
        assert!(params.contains(&("blocks", r#"[{"type":"rich_text"}]"#.to_string())));
        assert!(params.contains(&("thread_ts", "1710000000.000100".to_string())));
        assert!(params.contains(&("client_msg_id", "client-message-id".to_string())));
    }

    #[test]
    fn rich_message_post_restores_submitted_blocks_when_response_omits_them() {
        let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
        let address = server.server_addr();
        let received = thread::spawn(move || {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            request
                .respond(
                    Response::from_string(
                        r#"{"ok":true,"message":{"ts":"1710000000.000100","text":"Hello"}}"#,
                    )
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
                )
                .expect("mock Slack response should be sent");
            body
        });
        let blocks = r#"[{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"Hello","style":{"bold":true}}]}]}]"#;
        let mut api = SlackApi::new(user_test_token());
        api.api_base_url = format!("http://{address}/api");

        let message = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.post_message("C123", "Hello", Some(blocks), None))
            .expect("message should post");

        assert!(matches!(
            message.document.nodes(),
            [crate::rich_message::MessageNode::RichText(_)]
        ));
        let body = received.join().expect("mock Slack server should finish");
        let form = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(form.get("blocks").map(String::as_str), Some(blocks));
    }

    #[test]
    fn rich_message_update_targets_existing_message_and_restores_omitted_blocks() {
        let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
        let address = server.server_addr();
        let received = thread::spawn(move || {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            request
                .respond(
                    Response::from_string(
                        r#"{"ok":true,"channel":"C123","ts":"1710000000.000100","text":"Edited","message":{"text":"Edited","user":"U1"}}"#,
                    )
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
                )
                .expect("mock Slack response should be sent");
            (path, body)
        });
        let blocks = r#"[{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"Edited","style":{"italic":true}}]}]}]"#;
        let original = SlackMessage {
            user: Some("U1".into()),
            text: Some("Original".into()),
            ts: "1710000000.000100".into(),
            reactions: Some(vec![]),
            ..SlackMessage::default()
        };
        let mut api = SlackApi::new(user_test_token());
        api.api_base_url = format!("http://{address}/api");

        let message = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.update_message("C123", &original, "Edited", Some(blocks)))
            .expect("message should update");

        assert_eq!(message.ts, original.ts);
        assert_eq!(message.body_text(), "Edited");
        assert_eq!(message.reactions, original.reactions);
        assert!(message.edited.is_some());
        assert!(matches!(
            message.document.nodes(),
            [crate::rich_message::MessageNode::RichText(_)]
        ));
        let (path, body) = received.join().expect("mock Slack server should finish");
        assert_eq!(path, "/api/chat.update");
        let form = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(form.get("channel").map(String::as_str), Some("C123"));
        assert_eq!(
            form.get("ts").map(String::as_str),
            Some("1710000000.000100")
        );
        assert_eq!(form.get("text").map(String::as_str), Some("Edited"));
        assert_eq!(form.get("blocks").map(String::as_str), Some(blocks));
        assert!(!form.contains_key("client_msg_id"));
        assert!(!form.contains_key("thread_ts"));
    }

    #[test]
    fn plain_message_update_omits_blocks_without_changing_the_target() {
        let form = update_message_params("C123", "1710000000.000100", "Edited", None)
            .into_iter()
            .collect::<HashMap<_, _>>();

        assert_eq!(form.get("channel").map(String::as_str), Some("C123"));
        assert_eq!(
            form.get("ts").map(String::as_str),
            Some("1710000000.000100")
        );
        assert_eq!(form.get("text").map(String::as_str), Some("Edited"));
        assert!(!form.contains_key("blocks"));
    }

    #[test]
    fn starred_conversation_ids_are_paginated_and_ignore_message_stars() {
        let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
        let address = server
            .server_addr()
            .to_ip()
            .expect("mock Slack server should use an IP address");
        let received = thread::spawn(move || {
            let mut observations = Vec::new();
            for response_body in [
                r#"{
                    "ok": true,
                    "items": [
                        {"type": "channel", "channel": "C1"},
                        {"type": "im", "channel": "D1"},
                        {"type": "group", "group": "G1"},
                        {"type": "message", "channel": "C-message", "message": {"ts": "1.0"}}
                    ],
                    "response_metadata": {"next_cursor": "next-page"}
                }"#,
                r#"{
                    "ok": true,
                    "items": [{"type": "channel", "channel": "C2"}],
                    "response_metadata": {"next_cursor": ""}
                }"#,
            ] {
                let mut request = server.recv().expect("mock Slack request should arrive");
                let path = request.url().to_string();
                let mut body = String::new();
                request
                    .as_reader()
                    .read_to_string(&mut body)
                    .expect("mock Slack request body should be readable");
                request
                    .respond(
                        Response::from_string(response_body).with_header(
                            Header::from_bytes("Content-Type", "application/json")
                                .expect("content type header should be valid"),
                        ),
                    )
                    .expect("mock Slack response should be sent");
                observations.push((path, body));
            }
            observations
        });

        let mut api = SlackApi::new(user_test_token());
        api.api_base_url = format!("http://{address}/api");
        let ids = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.starred_conversation_ids())
            .expect("starred conversations should load");

        assert_eq!(
            ids,
            HashSet::from([
                "C1".to_string(),
                "C2".to_string(),
                "D1".to_string(),
                "G1".to_string(),
            ])
        );
        let observations = received.join().expect("mock Slack server should finish");
        assert_eq!(observations.len(), 2);
        for (index, (path, body)) in observations.iter().enumerate() {
            assert_eq!(path, "/api/stars.list");
            let form = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>();
            assert_eq!(form.get("limit").map(String::as_str), Some("200"));
            assert_eq!(
                form.get("cursor").map(String::as_str),
                (index == 1).then_some("next-page")
            );
        }
    }

    #[test]
    fn conversation_star_toggle_uses_a_bare_channel_target() {
        let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
        let address = server
            .server_addr()
            .to_ip()
            .expect("mock Slack server should use an IP address");
        let received = thread::spawn(move || {
            let mut observations = Vec::new();
            for _ in 0..2 {
                let mut request = server.recv().expect("mock Slack request should arrive");
                let path = request.url().to_string();
                let mut body = String::new();
                request
                    .as_reader()
                    .read_to_string(&mut body)
                    .expect("mock Slack request body should be readable");
                request
                    .respond(
                        Response::from_string(r#"{"ok":true}"#).with_header(
                            Header::from_bytes("Content-Type", "application/json")
                                .expect("content type header should be valid"),
                        ),
                    )
                    .expect("mock Slack response should be sent");
                observations.push((path, body));
            }
            observations
        });

        let mut api = SlackApi::new(user_test_token());
        api.api_base_url = format!("http://{address}/api");
        let runtime = tokio::runtime::Runtime::new().expect("test runtime should start");
        runtime
            .block_on(api.set_conversation_starred("D123", true))
            .expect("conversation should be starred");
        runtime
            .block_on(api.set_conversation_starred("D123", false))
            .expect("conversation should be unstarred");

        let observations = received.join().expect("mock Slack server should finish");
        assert_eq!(observations.len(), 2);
        for ((path, body), expected_method) in observations
            .into_iter()
            .zip(["/api/stars.add", "/api/stars.remove"])
        {
            assert_eq!(path, expected_method);
            let form = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect::<HashMap<_, _>>();
            assert_eq!(form.get("channel").map(String::as_str), Some("D123"));
            assert!(!form.contains_key("timestamp"));
        }
    }

    #[test]
    fn partial_media_download_lives_next_to_destination() {
        let destination = Path::new("/tmp/conduit/media/photo.jpg");
        let partial = partial_download_path(destination);

        assert_eq!(partial.parent(), destination.parent());
        assert!(partial
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("photo.jpg.") && name.ends_with(".part")));
    }

    #[test]
    fn discovery_requests_only_non_archived_channel_types() {
        assert_eq!(
            paginated_list_params(Some(" next-page "), true),
            vec![
                ("types", "public_channel,private_channel".to_string()),
                ("exclude_archived", "true".to_string()),
                ("limit", "200".to_string()),
                ("cursor", "next-page".to_string()),
            ]
        );
    }

    #[test]
    fn users_requests_are_paginated_without_channel_parameters() {
        assert_eq!(
            paginated_list_params(Some("users-page"), false),
            vec![
                ("limit", "200".to_string()),
                ("cursor", "users-page".to_string()),
            ]
        );
        assert_eq!(
            paginated_list_params(Some("  "), false),
            vec![("limit", "200".to_string())]
        );
    }

    #[test]
    fn discovery_filter_rejects_archived_channels_and_direct_messages() {
        let public_channel = SlackConversation {
            is_channel: Some(true),
            ..Default::default()
        };
        let private_channel = SlackConversation {
            is_private: Some(true),
            ..Default::default()
        };
        let archived_channel = SlackConversation {
            is_channel: Some(true),
            is_archived: Some(true),
            ..Default::default()
        };
        let direct_message = SlackConversation {
            is_im: Some(true),
            ..Default::default()
        };

        assert!(is_discoverable_conversation(&public_channel));
        assert!(is_discoverable_conversation(&private_channel));
        assert!(!is_discoverable_conversation(&archived_channel));
        assert!(!is_discoverable_conversation(&direct_message));
    }

    #[test]
    fn pagination_cursor_is_trimmed_and_empty_values_end_pagination() {
        assert_eq!(
            next_cursor(Some(ResponseMetadata {
                next_cursor: Some(" next-page ".to_string()),
            }))
            .as_deref(),
            Some("next-page")
        );
        assert_eq!(
            next_cursor(Some(ResponseMetadata {
                next_cursor: Some("  ".to_string()),
            })),
            None
        );
        assert_eq!(next_cursor(None), None);
    }

    #[test]
    fn conversation_property_logging_is_opt_in_and_bounded() {
        assert_eq!(conversation_property_log_limit(None, 100), 0);
        assert_eq!(
            conversation_property_log_limit(Some("true"), 100),
            DEFAULT_DEBUG_CONVERSATION_PROPERTY_LIMIT
        );
        assert_eq!(conversation_property_log_limit(Some("1"), 5), 5);
        assert_eq!(conversation_property_log_limit(Some("7"), 100), 7);
        assert_eq!(conversation_property_log_limit(Some("all"), 100), 100);
        assert_eq!(conversation_property_log_limit(Some("invalid"), 100), 0);
    }

    #[test]
    fn message_page_has_more_uses_response_metadata_cursor() {
        let page = SlackMessagePage::from_response(
            HistoryResponse {
                ok: true,
                error: None,
                messages: vec![message("1710000000.000100")],
                has_more: Some(false),
                unread_count: None,
                unread_count_display: None,
                unread_count_string: None,
                has_unreads: None,
                is_unread: None,
                response_metadata: Some(ResponseMetadata {
                    next_cursor: Some(" next-page ".to_string()),
                }),
            },
            std::convert::identity,
        );

        assert!(page.has_more);
        assert_eq!(page.next_cursor.as_deref(), Some("next-page"));
    }

    #[test]
    fn latest_history_request_includes_unread_state() {
        assert!(
            history_request_params("C123", None, CHANNEL_HISTORY_PAGE_LIMIT, true)
                .contains(&("unreads", "true".to_string()))
        );
        assert_eq!(
            history_request_params("C123", None, UNREAD_STATE_HISTORY_LIMIT, true)
                .iter()
                .find(|(key, _)| *key == "limit")
                .map(|(_, value)| value.as_str()),
            Some("1")
        );
        assert!(!history_request_params(
            "C123",
            Some("next-page"),
            CHANNEL_HISTORY_PAGE_LIMIT,
            true
        )
        .iter()
        .any(|(key, _)| *key == "unreads"));
    }

    #[test]
    fn message_context_requests_are_bounded_inclusive_and_targeted() {
        let params = message_context_request_params("C123", "1710000000.000100");

        assert_eq!(
            params,
            vec![
                ("channel", "C123".to_string()),
                ("latest", "1710000000.000100".to_string()),
                ("inclusive", "true".to_string()),
                ("limit", "15".to_string()),
            ]
        );

        assert_eq!(
            thread_message_context_request_params(
                "C123",
                "1709999999.000100",
                "1710000000.000100",
            )
            .last(),
            Some(&("ts", "1709999999.000100".to_string()))
        );
    }

    #[test]
    fn message_page_preserves_badgeless_unread_state() {
        let page = SlackMessagePage::from_response(
            HistoryResponse {
                ok: true,
                error: None,
                messages: vec![message("1710000000.000100")],
                has_more: Some(false),
                unread_count: Some(5),
                unread_count_display: Some(0),
                unread_count_string: None,
                has_unreads: None,
                is_unread: None,
                response_metadata: None,
            },
            std::convert::identity,
        );

        assert!(page.unread_state.known);
        assert!(page.unread_state.has_unread);
        assert_eq!(page.unread_state.display_count, 0);
    }

    #[test]
    fn last_read_comparison_detects_badgeless_unread_state() {
        let unread = unread_state_from_last_read("1710000000.000000", "1710000001.000000");
        let read = unread_state_from_last_read("1710000001.000000", "1710000001.000000");

        assert!(unread.known);
        assert!(unread.has_unread);
        assert_eq!(unread.display_count, 0);
        assert!(read.known);
        assert!(!read.has_unread);
    }

    #[test]
    fn conversation_latest_ts_accepts_latest_object_and_string() {
        let object_latest: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "C1",
            "latest": {
                "ts": "1710000001.000000"
            }
        }))
        .expect("conversation should parse");
        let string_latest: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "C2",
            "latest": "1710000002.000000"
        }))
        .expect("conversation should parse");

        assert_eq!(
            conversation_latest_ts(&object_latest),
            Some("1710000001.000000")
        );
        assert_eq!(
            conversation_latest_ts(&string_latest),
            Some("1710000002.000000")
        );
    }

    #[test]
    fn token_scope_set_accepts_commas_and_whitespace() {
        let scopes = token_scope_set(Some("channels:read,channels:write im:write"));

        assert!(scopes.contains("channels:read"));
        assert!(scopes.contains("channels:write"));
        assert!(scopes.contains("im:write"));
    }

    #[test]
    fn browser_unread_workspace_urls_are_restricted_to_slack_origins() {
        assert_eq!(
            validated_workspace_api_base_url("https://example.slack.com/").unwrap(),
            "https://example.slack.com/api"
        );
        assert_eq!(
            validated_workspace_api_base_url("https://grid.example.slack.com:443/").unwrap(),
            "https://grid.example.slack.com/api"
        );

        for workspace_url in [
            "http://example.slack.com/",
            "https://example.slack.com:8443/",
            "https://person@example.slack.com/",
            "https://example.slack.com/path",
            "https://example.slack.com/?mode=online",
            "https://example.slack.com/#fragment",
            "https://slack.com.evil.example/",
            "https://slack.com/",
        ] {
            assert!(validated_workspace_api_base_url(workspace_url).is_err());
        }

        assert_eq!(
            validated_slack_route(Some("opaque-route".to_string())).unwrap(),
            "opaque-route"
        );
        assert!(validated_slack_route(None).is_err());
        assert!(validated_slack_route(Some(" route".to_string())).is_err());
        assert!(validated_slack_route(Some("bad\nroute".to_string())).is_err());
    }

    #[test]
    fn browser_unread_snapshot_normalizes_cursors_and_rejects_schema_drift() {
        let open_ims = HashSet::from(["D-open".to_string()]);
        let snapshot = normalize_browser_unread_snapshot(
            BrowserCountsResponse {
                ok: true,
                error: None,
                channels: vec![BrowserCountRecord {
                    id: "C1".to_string(),
                    last_read: Some("1710000000.000000".to_string()),
                    latest: Some("1710000001.000000".to_string()),
                    has_unreads: None,
                    mention_count: 0,
                }],
                ims: vec![BrowserCountRecord {
                    id: "D-open".to_string(),
                    last_read: Some("1710000002.000000".to_string()),
                    latest: Some("1710000002.000000".to_string()),
                    has_unreads: None,
                    mention_count: 7,
                }],
                mpims: Vec::new(),
            },
            &open_ims,
        )
        .unwrap();

        assert!(snapshot.channels[0].has_unreads);
        assert!(!snapshot.ims[0].has_unreads);
        assert!(snapshot.ims[0].is_open);
        assert_eq!(snapshot.ims[0].mention_count, 7);

        assert!(normalize_browser_unread_snapshot(
            BrowserCountsResponse {
                ok: true,
                error: None,
                channels: Vec::new(),
                ims: Vec::new(),
                mpims: Vec::new(),
            },
            &HashSet::new(),
        )
        .is_err());
        assert!(normalize_browser_unread_snapshot(
            BrowserCountsResponse {
                ok: true,
                error: None,
                channels: vec![BrowserCountRecord {
                    id: "  ".to_string(),
                    last_read: None,
                    latest: None,
                    has_unreads: Some(false),
                    mention_count: 0,
                }],
                ims: Vec::new(),
                mpims: Vec::new(),
            },
            &HashSet::new(),
        )
        .is_err());
        assert!(normalize_browser_unread_snapshot(
            BrowserCountsResponse {
                ok: true,
                error: None,
                channels: vec![BrowserCountRecord {
                    id: "C1".to_string(),
                    last_read: Some("invalid".to_string()),
                    latest: Some("also-invalid".to_string()),
                    has_unreads: None,
                    mention_count: 0,
                }],
                ims: Vec::new(),
                mpims: Vec::new(),
            },
            &HashSet::new(),
        )
        .is_err());
    }

    #[test]
    fn browser_unread_snapshot_requires_browser_session_credentials() {
        let api = SlackApi::new(browser_test_token(None));
        let error = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.browser_unread_snapshot("https://example.slack.com/"))
            .expect_err("browser cookie should be required");

        assert_eq!(error.category(), SlackErrorCategory::Validation);

        let mut token = browser_test_token(Some("browser-cookie-value"));
        token.user_agent = None;
        SlackApi::new(token)
            .ensure_browser_session_credentials()
            .expect("browser user agent should remain optional");
    }

    #[test]
    fn browser_unread_snapshot_uses_boot_route_and_browser_request_shape() {
        let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
        let address = server
            .server_addr()
            .to_ip()
            .expect("mock Slack server should use an IP address");
        let received = thread::spawn(move || {
            let mut observations = Vec::new();
            for response_body in [
                r#"{
                    "ok": true,
                    "slack_route": "test-route-value",
                    "ims": [
                        {"id": "D-open", "is_open": true},
                        {"id": "D-closed", "is_open": false}
                    ]
                }"#,
                r#"{
                    "ok": true,
                    "channels": [{
                        "id": "C1",
                        "last_read": "1710000000.000000",
                        "latest": "1710000001.000000",
                        "has_unreads": true,
                        "mention_count": 0
                    }],
                    "ims": [
                        {
                            "id": "D-open",
                            "last_read": "1710000002.000000",
                            "latest": "1710000002.000000",
                            "mention_count": 3
                        },
                        {
                            "id": "D-closed",
                            "last_read": "1710000003.000000",
                            "latest": "1710000003.000000",
                            "has_unreads": false,
                            "mention_count": 0
                        }
                    ],
                    "mpims": [{
                        "id": "G1",
                        "last_read": "1710000000.000000",
                        "latest": "1710000004.000000",
                        "has_unreads": true,
                        "mention_count": 1
                    }]
                }"#,
            ] {
                let mut request = server.recv().expect("mock Slack request should arrive");
                let path = request.url().to_string();
                let request_path = path.split_once('?').map_or(path.as_str(), |(path, _)| path);
                let authorization_absent = !request
                    .headers()
                    .iter()
                    .any(|header| header.field.equiv("authorization"));
                let cookie_valid = request
                    .headers()
                    .iter()
                    .find(|header| header.field.equiv("cookie"))
                    .is_some_and(|header| {
                        let value = header.value.as_str();
                        value.starts_with("d=browser-cookie-value; d-s=")
                    });
                let user_agent_valid = request
                    .headers()
                    .iter()
                    .find(|header| header.field.equiv("user-agent"))
                    .is_some_and(|header| header.value.as_str() == "Conduit Browser Test Agent");
                let mut body = String::new();
                request
                    .as_reader()
                    .read_to_string(&mut body)
                    .expect("mock Slack request body should be readable");
                let token_valid =
                    multipart_field_value(&body, "token") == Some("browser-access-value");
                let route_valid = if request_path.ends_with("client.userBoot") {
                    request_query_field(&path, "slack_route").is_none()
                        && multipart_field_value(&body, "slack_route").is_none()
                } else {
                    request_query_field(&path, "slack_route").as_deref() == Some("test-route-value")
                        && multipart_field_value(&body, "slack_route").is_none()
                };
                let flags_valid = if request_path.ends_with("client.userBoot") {
                    multipart_field_value(&body, "version_all_channels") == Some("false")
                        && multipart_field_value(&body, "return_all_relevant_mpdms") == Some("true")
                        && multipart_field_value(&body, "omit_extras")
                            == Some(USER_BOOT_OMIT_EXTRAS)
                        && multipart_field_value(&body, "_x_app_name") == Some("client")
                        && multipart_field_value(&body, "_x_reason") == Some("initial-data")
                        && multipart_field_value(&body, "_x_sonic") == Some("true")
                } else {
                    multipart_field_value(&body, "include_all_unreads") == Some("true")
                        && multipart_field_value(&body, "include_file_channels") == Some("true")
                        && multipart_field_value(&body, "org_wide_aware") == Some("true")
                        && multipart_field_value(&body, "thread_counts_by_channel") == Some("true")
                        && multipart_field_value(&body, "_x_app_name") == Some("client")
                        && multipart_field_value(&body, "_x_mode") == Some("online")
                        && multipart_field_value(&body, "_x_reason")
                            == Some("fetchClientCountsOnConnect")
                        && multipart_field_value(&body, "_x_sonic") == Some("true")
                };
                observations.push((
                    path,
                    authorization_absent,
                    cookie_valid,
                    user_agent_valid,
                    token_valid,
                    route_valid,
                    flags_valid,
                    multipart_field_names(&body),
                ));
                request
                    .respond(
                        Response::from_string(response_body).with_header(
                            Header::from_bytes("Content-Type", "application/json")
                                .expect("content type header should be valid"),
                        ),
                    )
                    .expect("mock Slack response should be sent");
            }
            observations
        });

        let mut api = SlackApi::new(browser_test_token(Some("browser-cookie-value")));
        api.api_base_url = format!("http://{address}/api");
        let snapshot = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.browser_unread_snapshot("https://example.slack.com/"))
            .expect("browser unread snapshot should succeed");

        assert_eq!(snapshot.channels.len(), 1);
        assert_eq!(snapshot.ims.len(), 2);
        assert_eq!(snapshot.mpims.len(), 1);
        assert!(snapshot.channels[0].has_unreads);
        assert!(snapshot.ims[0].is_open);
        assert!(!snapshot.ims[0].has_unreads);
        assert_eq!(snapshot.ims[0].mention_count, 3);
        assert!(!snapshot.ims[1].is_open);
        assert!(snapshot.mpims[0].has_unreads);

        let observations = received.join().expect("mock Slack server should finish");
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].0, "/api/client.userBoot");
        assert_eq!(
            observations[1].0,
            "/api/client.counts?slack_route=test-route-value"
        );
        assert!(observations
            .iter()
            .all(
                |(_, no_auth, cookie, agent, token, route, flags, _)| *no_auth
                    && *cookie
                    && *agent
                    && *token
                    && *route
                    && *flags
            ));
        assert_eq!(
            observations[0].7,
            HashSet::from([
                "token".to_string(),
                "version_all_channels".to_string(),
                "return_all_relevant_mpdms".to_string(),
                "omit_extras".to_string(),
                "_x_app_name".to_string(),
                "_x_reason".to_string(),
                "_x_sonic".to_string(),
            ])
        );
        assert_eq!(
            observations[1].7,
            HashSet::from([
                "token".to_string(),
                "include_all_unreads".to_string(),
                "include_file_channels".to_string(),
                "org_wide_aware".to_string(),
                "thread_counts_by_channel".to_string(),
                "_x_app_name".to_string(),
                "_x_mode".to_string(),
                "_x_reason".to_string(),
                "_x_sonic".to_string(),
            ])
        );
    }

    #[test]
    fn browser_session_auth_test_sends_upstream_form_and_cookie_shape() {
        let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
        let address = server
            .server_addr()
            .to_ip()
            .expect("mock Slack server should use an IP address");
        let received = thread::spawn(move || {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            let headers = request
                .headers()
                .iter()
                .map(|header| {
                    (
                        header.field.as_str().to_ascii_lowercase().to_string(),
                        header.value.as_str().to_string(),
                    )
                })
                .collect::<HashMap<_, _>>();
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            request
                .respond(
                    Response::from_string(
                        r#"{"ok":true,"team":"Example","team_id":"T1","user":"Vincent","user_id":"U1","url":"https://example.slack.com/"}"#,
                    )
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
                )
                .expect("mock Slack response should be sent");
            (path, headers, body)
        });

        let token = StoredToken {
            access_token: "xoxc-browser-token".to_string(),
            token_type: Some("browser_session".to_string()),
            scope: None,
            refresh_token: None,
            expires_in: None,
            expires_at: None,
            team_id: None,
            team_name: None,
            user_id: None,
            client_id: None,
            browser_cookie_d: Some("xoxd-cookie-value".to_string()),
            user_agent: Some("Exact Browser User Agent".to_string()),
        };
        let mut api = SlackApi::new(token);
        api.api_base_url = format!("http://{address}/api");
        let auth = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.auth_test())
            .expect("browser-session auth.test should succeed");
        assert_eq!(auth.team_id.as_deref(), Some("T1"));

        let (path, headers, body) = received.join().expect("mock Slack server should finish");
        assert_eq!(path, "/api/auth.test");
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer xoxc-browser-token")
        );
        assert_eq!(
            headers.get("user-agent").map(String::as_str),
            Some("Exact Browser User Agent")
        );
        let cookie = headers
            .get("cookie")
            .expect("browser-session cookie should be sent");
        assert!(cookie.starts_with("d=xoxd-cookie-value; d-s="));
        let form = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(
            form.get("token").map(String::as_str),
            Some("xoxc-browser-token")
        );
    }

    #[test]
    fn file_info_requests_the_file_and_returns_the_response_model() {
        let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
        let address = server
            .server_addr()
            .to_ip()
            .expect("mock Slack server should use an IP address");
        let received = thread::spawn(move || {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            request
                .respond(
                    Response::from_string(r#"{"ok":true,"file":{"id":"F123","title":"Design"}}"#)
                        .with_header(
                            Header::from_bytes("Content-Type", "application/json")
                                .expect("content type header should be valid"),
                        ),
                )
                .expect("mock Slack response should be sent");
            (path, body)
        });

        let token = StoredToken {
            access_token: "xoxp-test-token".to_string(),
            token_type: None,
            scope: None,
            refresh_token: None,
            expires_in: None,
            expires_at: None,
            team_id: None,
            team_name: None,
            user_id: None,
            client_id: None,
            browser_cookie_d: None,
            user_agent: None,
        };
        let mut api = SlackApi::new(token);
        api.api_base_url = format!("http://{address}/api");
        let file = tokio::runtime::Runtime::new()
            .expect("test runtime should start")
            .block_on(api.file("F123"))
            .expect("files.info should succeed");

        let (path, body) = received.join().expect("mock Slack server should finish");
        assert_eq!(path, "/api/files.info");
        let form = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(form.get("file").map(String::as_str), Some("F123"));
        assert_eq!(file.id.as_deref(), Some("F123"));
        assert_eq!(file.display_title(), "Design");
    }
}
