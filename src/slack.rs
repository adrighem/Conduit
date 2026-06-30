use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::header::{CONTENT_TYPE, COOKIE, RETRY_AFTER, USER_AGENT};
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use serde_json::json;

use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SlackConversation, SlackFile, SlackMessage, SlackUser,
    StoredToken,
};

const MAX_UPLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_PREVIEW_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RATE_LIMIT_RETRIES: usize = 2;
const DEFAULT_RETRY_AFTER_SECONDS: u64 = 1;
const MAX_RETRY_AFTER_SECONDS: u64 = 30;
const HISTORY_PAGE_LIMIT: &str = "50";
const DEFAULT_DEBUG_CONVERSATION_PROPERTY_LIMIT: usize = 20;
const DEBUG_CONVERSATION_PROPERTIES_ENV: &str = "CONDUIT_DEBUG_CONVERSATION_PROPERTIES";
const CONVERSATIONS_LIST_METHOD: &str = "conversations.list";
const USERS_CONVERSATIONS_METHOD: &str = "users.conversations";
const READ_MARKER_SCOPES: [&str; 4] = ["channels:write", "groups:write", "im:write", "mpim:write"];

#[derive(Clone)]
pub struct SlackApi {
    http: Client,
    access_token: String,
    scopes: HashSet<String>,
    browser_cookie_d: Option<String>,
    user_agent: Option<String>,
}

impl SlackApi {
    pub fn new(token: StoredToken) -> Self {
        let scopes = token_scope_set(token.scope.as_deref());
        Self {
            http: Client::new(),
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
                ("exclude_archived", "true".to_string()),
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

    pub fn can_mark_read(&self) -> bool {
        READ_MARKER_SCOPES
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
        let mut params = vec![
            ("channel", channel_id.to_string()),
            ("limit", HISTORY_PAGE_LIMIT.to_string()),
        ];
        if let Some(cursor) = cursor.filter(|cursor| !cursor.trim().is_empty()) {
            params.push(("cursor", cursor.to_string()));
        }

        let response: HistoryResponse = self.post_form("conversations.history", &params).await?;
        Ok(SlackMessagePage::from_response(
            response,
            std::convert::identity,
        ))
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
            ("limit", HISTORY_PAGE_LIMIT.to_string()),
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

    pub async fn search_messages(&self, query: &str) -> Result<Vec<SearchMatch>> {
        let response: SearchResponse = self
            .post_form(
                "search.messages",
                &[
                    ("query", query.to_string()),
                    ("count", "40".to_string()),
                    ("page", "1".to_string()),
                ],
            )
            .await?;
        Ok(response.messages.matches)
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

    pub async fn user_display_name(&self, user_id: &str) -> Result<String> {
        let response: UserInfoResponse = self
            .post_form("users.info", &[("user", user_id.to_string())])
            .await?;
        Ok(response
            .user
            .display_name()
            .unwrap_or_else(|| user_id.to_string()))
    }

    pub async fn download_image(&self, url: &str) -> Result<DownloadedImage> {
        let response = self
            .authenticated_request(Method::GET, url)
            .send()
            .await
            .context("failed to download Slack image preview")?
            .error_for_status()
            .context("Slack image preview returned an HTTP error")?;

        let mime_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| value.starts_with("image/"))
            .ok_or_else(|| anyhow!("Slack image preview did not return an image content type"))?
            .to_string();

        if response
            .content_length()
            .is_some_and(|length| length > MAX_PREVIEW_IMAGE_BYTES as u64)
        {
            return Err(anyhow!("Slack image preview is larger than 8 MiB"));
        }

        let bytes = response
            .bytes()
            .await
            .context("failed to read Slack image preview bytes")?;
        if bytes.len() > MAX_PREVIEW_IMAGE_BYTES {
            return Err(anyhow!("Slack image preview is larger than 8 MiB"));
        }

        Ok(DownloadedImage {
            mime_type,
            bytes: bytes.to_vec(),
        })
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
            .ok_or_else(|| anyhow!("file path has no valid filename"))?
            .to_string();
        let metadata = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.len() > MAX_UPLOAD_BYTES {
            return Err(anyhow!("{} is larger than 1 GiB", path.display()));
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
        let mut params = vec![("files", files), ("channel_id", channel_id.to_string())];
        if let Some(initial_comment) = initial_comment.filter(|comment| !comment.trim().is_empty())
        {
            params.push(("initial_comment", initial_comment.to_string()));
        }
        let complete: CompleteUploadResponse = self
            .post_form("files.completeUploadExternal", &params)
            .await?;

        progress(UploadProgressUpdate::new(1.0, "Upload complete"));
        complete
            .files
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Slack did not return uploaded file metadata"))
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
                    return Err(anyhow!(
                        "Slack method {method} was rate limited; try again shortly"
                    ));
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
    if matches!(
        method,
        CONVERSATIONS_LIST_METHOD | USERS_CONVERSATIONS_METHOD
    ) {
        0
    } else {
        MAX_RATE_LIMIT_RETRIES
    }
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
}

impl SlackMessagePage {
    fn from_response(
        response: HistoryResponse,
        normalize_messages: impl FnOnce(Vec<SlackMessage>) -> Vec<SlackMessage>,
    ) -> Self {
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
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadedImage {
    pub mime_type: String,
    pub bytes: Vec<u8>,
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
            Err(anyhow!(
                "Slack method {method} failed: {}",
                self.error().unwrap_or("unknown_error")
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
struct HistoryResponse {
    ok: bool,
    error: Option<String>,
    messages: Vec<SlackMessage>,
    has_more: Option<bool>,
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
struct UserInfoResponse {
    ok: bool,
    error: Option<String>,
    user: SlackUser,
}
impl_slack_response!(UserInfoResponse);

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

    fn message(ts: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.to_string(),
            ..Default::default()
        }
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
        assert_eq!(retry_after_seconds("120"), MAX_RETRY_AFTER_SECONDS);
    }

    #[test]
    fn conversations_list_rate_limits_fail_fast() {
        assert_eq!(rate_limit_retries_for_method(CONVERSATIONS_LIST_METHOD), 0);
        assert_eq!(rate_limit_retries_for_method(USERS_CONVERSATIONS_METHOD), 0);
        assert_eq!(
            rate_limit_retries_for_method("conversations.history"),
            MAX_RATE_LIMIT_RETRIES
        );
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
}
