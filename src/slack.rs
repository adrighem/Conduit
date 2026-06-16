use std::path::Path;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;

use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SlackConversation, SlackFile, SlackMessage, StoredToken,
};

#[derive(Clone)]
pub struct SlackApi {
    http: Client,
    token: StoredToken,
}

impl SlackApi {
    pub fn new(token: StoredToken) -> Self {
        Self {
            http: Client::new(),
            token,
        }
    }

    pub async fn auth_test(&self) -> Result<AuthInfo> {
        let response: AuthTestResponse = self.post_form("auth.test", &[]).await?;
        Ok(AuthInfo {
            team: response.team,
            team_id: response.team_id.or_else(|| self.token.team_id.clone()),
            user: response.user,
            user_id: response.user_id.or_else(|| self.token.user_id.clone()),
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

    pub async fn upload_file(&self, channel_id: &str, path: &Path) -> Result<SlackFile> {
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("file path has no valid filename"))?
            .to_string();
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;

        let upload: UploadUrlResponse = self
            .post_form(
                "files.getUploadURLExternal",
                &[
                    ("filename", filename.clone()),
                    ("length", bytes.len().to_string()),
                ],
            )
            .await?;

        self.http
            .post(&upload.upload_url)
            .body(bytes)
            .send()
            .await
            .context("failed to upload file bytes to Slack upload URL")?
            .error_for_status()
            .context("Slack upload URL returned an HTTP error")?;

        let files = json!([{ "id": upload.file_id, "title": filename }]).to_string();
        let complete: CompleteUploadResponse = self
            .post_form(
                "files.completeUploadExternal",
                &[("files", files), ("channel_id", channel_id.to_string())],
            )
            .await?;

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
            .bearer_auth(&self.token.access_token)
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
