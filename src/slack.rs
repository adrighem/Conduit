use std::path::Path;

use anyhow::{anyhow, Context, Result};
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SlackConversation, SlackFile, SlackMessage, SlackUser,
    StoredToken,
};

const MAX_UPLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_PREVIEW_IMAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct SlackApi {
    http: Client,
    access_token: String,
}

impl SlackApi {
    pub fn new(token: StoredToken) -> Self {
        Self {
            http: Client::new(),
            access_token: token.access_token,
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
                self.post_form("conversations.list", &params).await?;
            conversations.extend(response.channels);

            cursor = response
                .response_metadata
                .and_then(|metadata| metadata.next_cursor);
            if cursor.as_deref().unwrap_or_default().is_empty() {
                break;
            }
        }

        conversations.sort_by_key(|conversation| conversation.display_name().to_lowercase());
        Ok(conversations)
    }

    pub async fn history(&self, channel_id: &str) -> Result<Vec<SlackMessage>> {
        let response: HistoryResponse = self
            .post_form(
                "conversations.history",
                &[
                    ("channel", channel_id.to_string()),
                    ("limit", "15".to_string()),
                ],
            )
            .await?;
        Ok(response.messages)
    }

    pub async fn thread_replies(&self, channel_id: &str, ts: &str) -> Result<Vec<SlackMessage>> {
        let response: HistoryResponse = self
            .post_form(
                "conversations.replies",
                &[
                    ("channel", channel_id.to_string()),
                    ("ts", ts.to_string()),
                    ("limit", "15".to_string()),
                ],
            )
            .await?;
        Ok(response.messages)
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
            .http
            .get(url)
            .bearer_auth(&self.access_token)
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
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.access_token)
            .form(params)
            .send()
            .await
            .with_context(|| format!("failed to call Slack method {method}"))?
            .error_for_status()
            .with_context(|| format!("Slack method {method} returned an HTTP error"))?
            .json::<T>()
            .await
            .with_context(|| format!("failed to parse Slack method {method} response"))?;

        response.into_result(method)
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
