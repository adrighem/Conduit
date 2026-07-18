use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use reqwest::header::{CONTENT_TYPE, COOKIE, RETRY_AFTER, USER_AGENT};
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SlackConversation, SlackFile, SlackMessage, SlackUnreadState,
    SlackUser, SlackUserGroup, SlackUserProfile, StoredToken,
};
use crate::search::{
    SearchField, SearchQuery, ID_FIELD_WEIGHT, PRIMARY_FIELD_WEIGHT, SECONDARY_FIELD_WEIGHT,
};

const MAX_UPLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_MEDIA_DOWNLOAD_BYTES: u64 = MAX_UPLOAD_BYTES;
const MAX_PREVIEW_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PREVIEW_VIDEO_BYTES: usize = 16 * 1024 * 1024;
const MAX_RATE_LIMIT_RETRIES: usize = 2;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
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
const READ_MARKER_SCOPES: [&str; 4] = ["channels:write", "groups:write", "im:write", "mpim:write"];

pub type Result<T> = std::result::Result<T, SlackError>;

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
    access_token: String,
    scopes: HashSet<String>,
    browser_cookie_d: Option<String>,
    user_agent: Option<String>,
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
            http: Client::builder()
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .read_timeout(HTTP_READ_TIMEOUT)
                .build()
                .expect("valid Slack HTTP client configuration"),
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
        let response: StarsListResponse = self
            .post_form("stars.list", &[("limit", "100".to_string())])
            .await?;
        Ok(response.items)
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
            .post_form("files.info", &Self::file_info_params(file_id))
            .await?;
        Ok(response.file)
    }

    fn file_info_params(file_id: &str) -> Vec<(&'static str, String)> {
        vec![("file", file_id.to_string())]
    }

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
        Ok(response.profile)
    }

    pub async fn user_groups(&self) -> Result<Vec<SlackUserGroup>> {
        let response: UserGroupsListResponse = self
            .post_form("usergroups.list", &[("include_users", "true".to_string())])
            .await?;
        Ok(response.usergroups)
    }

    pub async fn download_preview_asset(&self, url: &str) -> Result<DownloadedPreviewAsset> {
        let request = if is_trusted_slack_download_url(url) {
            self.authenticated_request(Method::GET, url)
        } else if is_trusted_avatar_url(url) {
            self.http.get(url)
        } else {
            return Err(SlackError::validation(
                "preview URL is not a trusted Slack asset URL",
            ));
        };
        let response = request
            .send()
            .await
            .context("failed to download Slack preview asset")?
            .error_for_status()
            .context("Slack preview asset returned an HTTP error")?;

        let mime_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| {
                value.starts_with("image/")
                    || matches!(
                        *value,
                        "video/mp4" | "video/webm" | "video/quicktime" | "video/ogg"
                    )
            })
            .ok_or_else(|| {
                SlackError::validation(
                    "Slack attachment preview returned an unsupported content type",
                )
            })?
            .to_string();

        let max_bytes = if mime_type.starts_with("video/") {
            MAX_PREVIEW_VIDEO_BYTES
        } else {
            MAX_PREVIEW_IMAGE_BYTES
        };

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

        Ok(DownloadedPreviewAsset { mime_type, bytes })
    }

    /// Downloads viewable Slack media to `destination` without retaining the
    /// complete response in memory. The destination is replaced atomically
    /// after a successful download and never contains a partial response.
    pub async fn download_media(&self, url: &str, destination: &Path) -> Result<DownloadedMedia> {
        ensure_trusted_slack_download_url(url)?;
        let response = self
            .authenticated_request(Method::GET, url)
            .send()
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
        let response = self
            .authenticated_request(Method::GET, url)
            .send()
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
        thread_ts: Option<&str>,
    ) -> Result<SlackMessage> {
        let mut params = vec![
            ("channel", channel_id.to_string()),
            ("text", text.to_string()),
        ];
        if let Some(thread_ts) = thread_ts {
            params.push(("thread_ts", thread_ts.to_string()));
        }

        let response: PostMessageResponse = self.post_form("chat.postMessage", &params).await?;
        Ok(response.message)
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

    pub async fn mark_read(&self, channel_id: &str, ts: &str) -> Result<()> {
        let _: BasicResponse = self
            .post_form(
                "conversations.mark",
                &[("channel", channel_id.to_string()), ("ts", ts.to_string())],
            )
            .await?;
        Ok(())
    }

    pub async fn upload_file<F>(
        &self,
        channel_id: &str,
        thread_ts: Option<&str>,
        path: &Path,
        initial_comment: Option<&str>,
        progress: F,
    ) -> Result<SlackFile>
    where
        F: Fn(UploadProgressUpdate),
    {
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

        progress(UploadProgressUpdate::new(0.15, "Reading file"));
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;

        progress(UploadProgressUpdate::new(0.35, "Requesting upload URL"));
        let upload: UploadUrlResponse = self
            .post_form(
                "files.getUploadURLExternal",
                &[
                    ("filename", filename.clone()),
                    ("length", bytes.len().to_string()),
                ],
            )
            .await?;

        progress(UploadProgressUpdate::new(0.60, "Uploading file"));
        self.http
            .post(&upload.upload_url)
            .body(bytes)
            .send()
            .await
            .context("failed to upload file bytes to Slack upload URL")?
            .error_for_status()
            .context("Slack upload URL returned an HTTP error")?;

        progress(UploadProgressUpdate::new(0.90, "Completing upload"));
        let files = json!([{ "id": upload.file_id, "title": filename }]).to_string();
        let params = complete_upload_params(files, channel_id, thread_ts, initial_comment);
        let complete: CompleteUploadResponse = self
            .post_form("files.completeUploadExternal", &params)
            .await?;

        progress(UploadProgressUpdate::new(1.0, "Upload complete"));
        complete
            .files
            .into_iter()
            .next()
            .ok_or_else(|| SlackError::validation("Slack did not return uploaded file metadata"))
    }

    async fn post_form<T>(&self, method: &str, params: &[(&str, String)]) -> Result<T>
    where
        T: for<'de> Deserialize<'de> + SlackResponse,
    {
        let url = format!("https://slack.com/api/{method}");
        let mut retries = 0;

        loop {
            let response = self
                .authenticated_request(Method::POST, &url)
                .timeout(API_REQUEST_TIMEOUT)
                .form(params)
                .send()
                .await
                .with_context(|| format!("failed to call Slack method {method}"))?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = retry_after_delay(&response);
                let max_retries = rate_limit_retries_for_method(method);
                if retries >= max_retries {
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
            request = request.header(COOKIE, format!("d={cookie}"));
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

fn complete_upload_params(
    files: String,
    channel_id: &str,
    thread_ts: Option<&str>,
    initial_comment: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut params = vec![("files", files), ("channel_id", channel_id.to_string())];
    if let Some(thread_ts) = thread_ts.filter(|thread_ts| !thread_ts.trim().is_empty()) {
        params.push(("thread_ts", thread_ts.to_string()));
    }
    if let Some(initial_comment) = initial_comment.filter(|comment| !comment.trim().is_empty()) {
        params.push(("initial_comment", initial_comment.to_string()));
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

fn rate_limit_retries_for_method(method: &str) -> usize {
    let _ = method;
    MAX_RATE_LIMIT_RETRIES
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

#[derive(Debug, Clone)]
pub struct DownloadedPreviewAsset {
    pub mime_type: String,
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
    items: Vec<SavedItem>,
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
    profile: SlackUserProfile,
}
impl_slack_response!(UserProfileResponse);

#[derive(Debug, Deserialize)]
struct UserGroupsListResponse {
    ok: bool,
    error: Option<String>,
    usergroups: Vec<SlackUserGroup>,
}
impl_slack_response!(UserGroupsListResponse);

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
    message: SlackMessage,
}
impl_slack_response!(PostMessageResponse);

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
    use super::*;
    use reqwest::header::{AUTHORIZATION, COOKIE, USER_AGENT};

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
            Some("See screenshot"),
        );

        assert!(params.contains(&("channel_id", "C123".to_string())));
        assert!(params.contains(&("thread_ts", "1710000000.000100".to_string())));
        assert!(params.contains(&("initial_comment", "See screenshot".to_string())));
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
    fn conversation_catalog_requests_retry_rate_limits() {
        assert_eq!(
            rate_limit_retries_for_method(CONVERSATIONS_LIST_METHOD),
            MAX_RATE_LIMIT_RETRIES
        );
        assert_eq!(
            rate_limit_retries_for_method(USERS_CONVERSATIONS_METHOD),
            MAX_RATE_LIMIT_RETRIES
        );
        assert_eq!(
            rate_limit_retries_for_method("conversations.history"),
            MAX_RATE_LIMIT_RETRIES
        );
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
    fn browser_session_requests_include_cookie_and_user_agent() {
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
            user_agent: Some("Browser User Agent".to_string()),
        };
        let api = SlackApi::new(token);

        let request = api
            .authenticated_request(reqwest::Method::POST, "https://slack.com/api/auth.test")
            .build()
            .expect("request should build");

        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer xoxc-browser-token")
        );
        assert_eq!(
            request
                .headers()
                .get(COOKIE)
                .and_then(|value| value.to_str().ok()),
            Some("d=xoxd-cookie-value")
        );
        assert_eq!(
            request
                .headers()
                .get(USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some("Browser User Agent")
        );
    }

    #[test]
    fn file_info_request_targets_one_file() {
        assert_eq!(
            SlackApi::file_info_params("F123"),
            vec![("file", "F123".to_string())]
        );
    }

    #[test]
    fn file_info_response_uses_the_existing_file_model() {
        let response: FileInfoResponse = serde_json::from_value(serde_json::json!({
            "ok": true,
            "file": {
                "id": "F123",
                "title": "Design"
            }
        }))
        .expect("file info response should parse");

        assert_eq!(response.file.id.as_deref(), Some("F123"));
        assert_eq!(response.file.display_title(), "Design");
    }
}
