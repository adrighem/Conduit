use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use sha2::{Digest, Sha256};

use crate::auth::{
    browser_session_token_from_env, browser_session_token_from_values, OAuthConfig,
    SlackOAuthClient, TokenStore,
};
use crate::config;
use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SlackConversation, SlackFile, SlackMessage, StoredToken,
};
use crate::slack::{DownloadedImage, SlackApi, SlackMessagePage};
use crate::store::WorkspaceStore;

#[derive(Debug)]
pub enum RuntimeCommand {
    LoadStoredToken,
    StartOAuth {
        client_id: String,
        debug_auth: bool,
    },
    StartBrowserSession {
        xoxc_token: String,
        xoxd_token: String,
        user_agent: Option<String>,
    },
    SignOut,
    RefreshConversations,
    LoadHistory {
        channel_id: String,
    },
    LoadOlderHistory {
        channel_id: String,
        cursor: String,
    },
    LoadThread {
        channel_id: String,
        ts: String,
    },
    LoadOlderThread {
        channel_id: String,
        ts: String,
        cursor: String,
    },
    SearchMessages {
        query: String,
    },
    LoadFiles,
    LoadSavedItems,
    LoadUser {
        user_id: String,
    },
    LoadImageAsset {
        key: String,
        url: String,
    },
    PostMessage {
        channel_id: String,
        text: String,
        thread_ts: Option<String>,
    },
    SetReaction {
        channel_id: String,
        ts: String,
        name: String,
        add: bool,
        thread_ts: Option<String>,
    },
    SetSaved {
        channel_id: String,
        ts: String,
        add: bool,
        thread_ts: Option<String>,
    },
    UploadFile {
        channel_id: String,
        path: PathBuf,
        initial_comment: Option<String>,
    },
}

#[derive(Debug)]
pub enum RuntimeEvent {
    Status(String),
    Error(String),
    SignedOut,
    Authenticated(AuthInfo),
    ConversationsLoaded(Vec<SlackConversation>),
    HistoryLoaded {
        channel_id: String,
        messages: Vec<SlackMessage>,
        has_more: bool,
        next_cursor: Option<String>,
        append_older: bool,
    },
    ThreadLoaded {
        channel_id: String,
        ts: String,
        messages: Vec<SlackMessage>,
        has_more: bool,
        next_cursor: Option<String>,
        append_older: bool,
    },
    SearchLoaded(Vec<SearchMatch>),
    FilesLoaded(Vec<SlackFile>),
    SavedItemsLoaded(Vec<SavedItem>),
    UserLoaded {
        user_id: String,
        display_name: String,
    },
    ImageAssetLoaded {
        key: String,
        data_uri: String,
    },
    ImageAssetFailed {
        key: String,
    },
    MessagePosted {
        channel_id: String,
        message: Box<SlackMessage>,
    },
    ReactionUpdated {
        channel_id: String,
        thread_ts: Option<String>,
    },
    SavedUpdated {
        channel_id: String,
        saved: bool,
        thread_ts: Option<String>,
    },
    FileUploadProgress {
        fraction: f64,
        label: String,
    },
    FileUploaded(String),
}

#[derive(Clone, Debug)]
pub struct AppRuntime {
    commands: Sender<RuntimeCommand>,
}

#[derive(Clone, Debug)]
struct ImageAssetCache {
    directory: PathBuf,
}

impl ImageAssetCache {
    fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    async fn load(&self, key: &str) -> Result<Option<String>> {
        let path = self.path_for_key(key);
        match tokio::fs::read_to_string(&path).await {
            Ok(data_uri) if data_uri.starts_with("data:image/") => Ok(Some(data_uri)),
            Ok(_) => Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error)
                .with_context(|| format!("failed to read cached image {}", path.display())),
        }
    }

    async fn store(&self, key: &str, data_uri: &str) -> Result<()> {
        tokio::fs::create_dir_all(&self.directory)
            .await
            .with_context(|| {
                format!(
                    "failed to create image cache directory {}",
                    self.directory.display()
                )
            })?;

        let path = self.path_for_key(key);
        tokio::fs::write(&path, data_uri)
            .await
            .with_context(|| format!("failed to write cached image {}", path.display()))
    }

    fn path_for_key(&self, key: &str) -> PathBuf {
        self.directory
            .join(format!("{}.data-uri", image_asset_cache_key(key)))
    }
}

fn image_asset_cache_key(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn image_data_uri(image: DownloadedImage) -> String {
    format!(
        "data:{};base64,{}",
        image.mime_type,
        BASE64.encode(image.bytes)
    )
}

impl AppRuntime {
    pub fn start(events: Sender<RuntimeEvent>) -> Self {
        let (commands, receiver) = mpsc::channel::<RuntimeCommand>();

        thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = events.send(RuntimeEvent::Error(format!(
                        "Failed to start background runtime: {error}"
                    )));
                    return;
                }
            };

            let token_store = TokenStore;
            let oauth = SlackOAuthClient::new();
            let image_cache = ImageAssetCache::new(config::image_asset_cache_dir());
            let mut slack: Option<SlackApi> = None;
            let mut workspace_store: Option<WorkspaceStore> = None;
            let mut user_cache = HashMap::new();
            let mut read_marks = HashMap::new();

            while let Ok(command) = receiver.recv() {
                let mut context = RuntimeContext {
                    events: &events,
                    token_store: &token_store,
                    oauth: &oauth,
                    image_cache: &image_cache,
                    slack: &mut slack,
                    workspace_store: &mut workspace_store,
                    user_cache: &mut user_cache,
                    read_marks: &mut read_marks,
                };
                let result = runtime.block_on(handle_command(command, &mut context));
                if let Err(error) = result {
                    let _ = events.send(RuntimeEvent::Error(error.to_string()));
                }
            }
        });

        Self { commands }
    }

    pub fn send(&self, command: RuntimeCommand) {
        let _ = self.commands.send(command);
    }
}

struct RuntimeContext<'a> {
    events: &'a Sender<RuntimeEvent>,
    token_store: &'a TokenStore,
    oauth: &'a SlackOAuthClient,
    image_cache: &'a ImageAssetCache,
    slack: &'a mut Option<SlackApi>,
    workspace_store: &'a mut Option<WorkspaceStore>,
    user_cache: &'a mut HashMap<String, String>,
    read_marks: &'a mut HashMap<String, String>,
}

async fn handle_command(command: RuntimeCommand, context: &mut RuntimeContext<'_>) -> Result<()> {
    match command {
        RuntimeCommand::LoadStoredToken => {
            crate::debug::log("runtime", "LoadStoredToken");
            context.events.send_status("Checking secure storage");
            if let Some(mut token) = context.token_store.load()? {
                if token.should_refresh() {
                    context.events.send_status("Refreshing Slack session");
                    token = context.oauth.refresh(&token).await?;
                    context.token_store.save(&token)?;
                }
                connect_and_load_workspace(context, token).await?;
            } else if let Some(token) = browser_session_token_from_env()? {
                context
                    .events
                    .send_status("Importing Slack browser session");
                let auth = connect_with_token(context.events, context.slack, token.clone()).await?;
                context.token_store.save(&token)?;
                load_workspace_after_auth(context, &auth).await?;
            } else {
                context.events.send_event(RuntimeEvent::SignedOut);
            }
        }
        RuntimeCommand::StartOAuth {
            client_id,
            debug_auth,
        } => {
            context.events.send_status("Opening Slack authorization");
            let token = context
                .oauth
                .authenticate(OAuthConfig::new(client_id), debug_auth)
                .await?;
            context.token_store.save(&token)?;
            connect_and_load_workspace(context, token).await?;
        }
        RuntimeCommand::StartBrowserSession {
            xoxc_token,
            xoxd_token,
            user_agent,
        } => {
            context
                .events
                .send_status("Validating Slack browser session");
            let token =
                browser_session_token_from_values(Some(xoxc_token), Some(xoxd_token), user_agent)?
                    .ok_or_else(|| anyhow!("Enter XOXC and XOXD tokens"))?;
            let auth = connect_with_token(context.events, context.slack, token.clone()).await?;
            context.token_store.save(&token)?;
            load_workspace_after_auth(context, &auth).await?;
        }
        RuntimeCommand::SignOut => {
            context.token_store.clear()?;
            *context.slack = None;
            *context.workspace_store = None;
            context.user_cache.clear();
            context.events.send_event(RuntimeEvent::SignedOut);
        }
        RuntimeCommand::RefreshConversations => {
            crate::debug::log("runtime", "RefreshConversations");
            load_conversations(context.events, context.slack, context.workspace_store).await?;
        }
        RuntimeCommand::LoadHistory { channel_id } => {
            let api = require_slack(context.slack)?;
            crate::debug::log("runtime", &format!("LoadHistory channel_id={channel_id}"));
            load_cached_history(context.events, context.workspace_store, &channel_id).await;
            context.events.send_status("Loading conversation");
            let page = api.history(&channel_id).await?;
            store_history(context.workspace_store, &channel_id, &page.messages).await;
            mark_history_read_best_effort(api, context.read_marks, &channel_id, &page.messages)
                .await;
            crate::debug::log(
                "runtime",
                &format!(
                    "HistoryLoaded channel_id={channel_id} messages={} has_more={} next_cursor={}",
                    page.messages.len(),
                    page.has_more,
                    page.next_cursor.is_some()
                ),
            );
            send_history_loaded(context.events, channel_id, page, false);
        }
        RuntimeCommand::LoadOlderHistory { channel_id, cursor } => {
            let api = require_slack(context.slack)?;
            crate::debug::log(
                "runtime",
                &format!("LoadOlderHistory channel_id={channel_id}"),
            );
            context.events.send_status("Loading older messages");
            let page = api.history_page(&channel_id, Some(&cursor)).await?;
            send_history_loaded(context.events, channel_id, page, true);
        }
        RuntimeCommand::LoadThread { channel_id, ts } => {
            let api = require_slack(context.slack)?;
            load_cached_thread(context.events, context.workspace_store, &channel_id, &ts).await;
            context.events.send_status("Loading thread");
            let page = api.thread_replies(&channel_id, &ts).await?;
            store_thread(context.workspace_store, &channel_id, &ts, &page.messages).await;
            send_thread_loaded(context.events, channel_id, ts, page, false);
        }
        RuntimeCommand::LoadOlderThread {
            channel_id,
            ts,
            cursor,
        } => {
            let api = require_slack(context.slack)?;
            crate::debug::log(
                "runtime",
                &format!("LoadOlderThread channel_id={channel_id} ts={ts}"),
            );
            context.events.send_status("Loading more replies");
            let page = api
                .thread_replies_page(&channel_id, &ts, Some(&cursor))
                .await?;
            send_thread_loaded(context.events, channel_id, ts, page, true);
        }
        RuntimeCommand::SearchMessages { query } => {
            let api = require_slack(context.slack)?;
            let results = api.search_messages(&query).await?;
            context
                .events
                .send_event(RuntimeEvent::SearchLoaded(results));
        }
        RuntimeCommand::LoadFiles => {
            let api = require_slack(context.slack)?;
            let files = api.files().await?;
            context.events.send_event(RuntimeEvent::FilesLoaded(files));
        }
        RuntimeCommand::LoadSavedItems => {
            let api = require_slack(context.slack)?;
            let items = api.saved_items().await?;
            context
                .events
                .send_event(RuntimeEvent::SavedItemsLoaded(items));
        }
        RuntimeCommand::LoadUser { user_id } => {
            if let Some(display_name) = context.user_cache.get(&user_id).cloned() {
                context.events.send_event(RuntimeEvent::UserLoaded {
                    user_id,
                    display_name,
                });
            } else {
                let api = require_slack(context.slack)?;
                let display_name = api.user_display_name(&user_id).await?;
                context
                    .user_cache
                    .insert(user_id.clone(), display_name.clone());
                context.events.send_event(RuntimeEvent::UserLoaded {
                    user_id,
                    display_name,
                });
            }
        }
        RuntimeCommand::LoadImageAsset { key, url } => {
            let api = require_slack(context.slack)?;
            crate::debug::log(
                "runtime",
                &format!("LoadImageAsset key={}", crate::debug::url_for_log(&key)),
            );
            match context.image_cache.load(&key).await {
                Ok(Some(data_uri)) => {
                    crate::debug::log(
                        "runtime",
                        &format!("ImageAssetCacheHit key={}", crate::debug::url_for_log(&key)),
                    );
                    context
                        .events
                        .send_event(RuntimeEvent::ImageAssetLoaded { key, data_uri });
                    return Ok(());
                }
                Ok(None) => {}
                Err(error) => crate::debug::log(
                    "runtime",
                    &format!(
                        "ImageAssetCacheReadFailed key={} error={error:#}",
                        crate::debug::url_for_log(&key)
                    ),
                ),
            }

            match api.download_image(&url).await {
                Ok(image) => {
                    crate::debug::log(
                        "runtime",
                        &format!(
                            "ImageAssetLoaded key={} mime_type={} bytes={}",
                            crate::debug::url_for_log(&key),
                            image.mime_type,
                            image.bytes.len()
                        ),
                    );
                    let data_uri = image_data_uri(image);
                    if let Err(error) = context.image_cache.store(&key, &data_uri).await {
                        crate::debug::log(
                            "runtime",
                            &format!(
                                "ImageAssetCacheWriteFailed key={} error={error:#}",
                                crate::debug::url_for_log(&key)
                            ),
                        );
                    }
                    context
                        .events
                        .send_event(RuntimeEvent::ImageAssetLoaded { key, data_uri });
                }
                Err(error) => {
                    crate::debug::log(
                        "runtime",
                        &format!(
                            "ImageAssetFailed key={} error={error:#}",
                            crate::debug::url_for_log(&key)
                        ),
                    );
                    context
                        .events
                        .send_event(RuntimeEvent::ImageAssetFailed { key });
                }
            }
        }
        RuntimeCommand::PostMessage {
            channel_id,
            text,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            let message = api
                .post_message(&channel_id, &text, thread_ts.as_deref())
                .await?;
            context.events.send_event(RuntimeEvent::MessagePosted {
                channel_id,
                message: Box::new(message),
            });
        }
        RuntimeCommand::SetReaction {
            channel_id,
            ts,
            name,
            add,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            api.set_reaction(&channel_id, &ts, &name, add).await?;
            context.events.send_event(RuntimeEvent::ReactionUpdated {
                channel_id,
                thread_ts,
            });
        }
        RuntimeCommand::SetSaved {
            channel_id,
            ts,
            add,
            thread_ts,
        } => {
            let api = require_slack(context.slack)?;
            api.set_saved(&channel_id, &ts, add).await?;
            context.events.send_event(RuntimeEvent::SavedUpdated {
                channel_id,
                saved: add,
                thread_ts,
            });
        }
        RuntimeCommand::UploadFile {
            channel_id,
            path,
            initial_comment,
        } => {
            let api = require_slack(context.slack)?;
            context.events.send_event(RuntimeEvent::FileUploadProgress {
                fraction: 0.05,
                label: "Preparing upload".to_string(),
            });
            let progress_events = context.events.clone();
            let file = api
                .upload_file(
                    &channel_id,
                    &path,
                    initial_comment.as_deref(),
                    move |update| {
                        progress_events.send_event(RuntimeEvent::FileUploadProgress {
                            fraction: update.fraction,
                            label: update.label,
                        });
                    },
                )
                .await?;
            let label = file
                .title
                .or(file.name)
                .or(file.id)
                .unwrap_or_else(|| "file".to_string());
            context.events.send_event(RuntimeEvent::FileUploaded(label));
        }
    }

    Ok(())
}

async fn connect_and_load_workspace(
    context: &mut RuntimeContext<'_>,
    token: StoredToken,
) -> Result<()> {
    let auth = connect_with_token(context.events, context.slack, token).await?;
    load_workspace_after_auth(context, &auth).await
}

async fn load_workspace_after_auth(
    context: &mut RuntimeContext<'_>,
    auth: &AuthInfo,
) -> Result<()> {
    *context.workspace_store = Some(WorkspaceStore::new(
        config::state_cache_dir(),
        &workspace_store_id(auth),
    ));
    context.user_cache.clear();
    load_cached_conversations(context.events, context.workspace_store).await;
    load_conversations(context.events, context.slack, context.workspace_store).await
}

async fn connect_with_token(
    events: &Sender<RuntimeEvent>,
    slack: &mut Option<SlackApi>,
    token: StoredToken,
) -> Result<AuthInfo> {
    let token_team = token.team_name.clone().or(token.team_id.clone());
    let token_team_id = token.team_id.clone();
    let token_user = token.user_id.clone();
    let api = SlackApi::new(token);
    let mut auth = api.auth_test().await?;
    auth.team = auth.team.or(token_team);
    auth.team_id = auth.team_id.or(token_team_id);
    auth.user_id = auth.user_id.or(token_user);
    crate::debug::log(
        "runtime",
        &format!(
            "Authenticated team={} user_id={}",
            auth.team.as_deref().unwrap_or("<unknown>"),
            auth.user_id.as_deref().unwrap_or("<unknown>")
        ),
    );
    *slack = Some(api);
    events.send_event(RuntimeEvent::Authenticated(auth.clone()));
    Ok(auth)
}

async fn load_conversations(
    events: &Sender<RuntimeEvent>,
    slack: &mut Option<SlackApi>,
    workspace_store: &Option<WorkspaceStore>,
) -> Result<()> {
    let api = require_slack(slack)?;
    events.send_status("Loading conversations");
    let conversations = api.conversations().await?;
    store_conversations(workspace_store, &conversations).await;
    crate::debug::log(
        "runtime",
        &format!("ConversationsLoaded count={}", conversations.len()),
    );
    events.send_event(RuntimeEvent::ConversationsLoaded(conversations));
    Ok(())
}

fn send_history_loaded(
    events: &Sender<RuntimeEvent>,
    channel_id: String,
    page: SlackMessagePage,
    append_older: bool,
) {
    events.send_event(RuntimeEvent::HistoryLoaded {
        channel_id,
        messages: page.messages,
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        append_older,
    });
}

fn send_thread_loaded(
    events: &Sender<RuntimeEvent>,
    channel_id: String,
    ts: String,
    page: SlackMessagePage,
    append_older: bool,
) {
    events.send_event(RuntimeEvent::ThreadLoaded {
        channel_id,
        ts,
        messages: page.messages,
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        append_older,
    });
}

async fn mark_history_read_best_effort(
    api: &SlackApi,
    read_marks: &mut HashMap<String, String>,
    channel_id: &str,
    messages: &[SlackMessage],
) {
    let Some(latest_ts) = SlackMessage::latest_ts(messages.iter()) else {
        return;
    };

    if read_marks
        .get(channel_id)
        .is_some_and(|marked_ts| marked_ts >= &latest_ts)
    {
        return;
    }

    if !api.can_mark_read() {
        crate::debug::log(
            "runtime",
            &format!("MarkReadSkipped channel_id={channel_id} reason=missing_token_scope"),
        );
        return;
    }

    match api.mark_read(channel_id, &latest_ts).await {
        Ok(()) => crate::debug::log(
            "runtime",
            &format!("MarkRead channel_id={channel_id} ts={latest_ts}"),
        ),
        Err(error) => crate::debug::log(
            "runtime",
            &format!("MarkReadFailed channel_id={channel_id} ts={latest_ts} error={error:#}"),
        ),
    }

    read_marks.insert(channel_id.to_string(), latest_ts);
}

fn workspace_store_id(auth: &AuthInfo) -> String {
    let team = auth
        .team_id
        .as_deref()
        .or(auth.team.as_deref())
        .or(auth.url.as_deref())
        .unwrap_or("unknown-team");
    let user = auth.user_id.as_deref().unwrap_or("unknown-user");
    format!("{team}:{user}")
}

async fn load_cached_conversations(
    events: &Sender<RuntimeEvent>,
    workspace_store: &Option<WorkspaceStore>,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    match store.load_conversations().await {
        Ok(Some(conversations)) => {
            crate::debug::log(
                "runtime",
                &format!("CachedConversationsLoaded count={}", conversations.len()),
            );
            events.send_event(RuntimeEvent::ConversationsLoaded(conversations));
        }
        Ok(None) => {}
        Err(error) => crate::debug::log(
            "runtime",
            &format!("CachedConversationsLoadFailed error={error:#}"),
        ),
    }
}

async fn store_conversations(
    workspace_store: &Option<WorkspaceStore>,
    conversations: &[SlackConversation],
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    if let Err(error) = store.store_conversations(conversations).await {
        crate::debug::log(
            "runtime",
            &format!("CachedConversationsStoreFailed error={error:#}"),
        );
    }
}

async fn load_cached_history(
    events: &Sender<RuntimeEvent>,
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    match store.load_history(channel_id).await {
        Ok(Some(messages)) => {
            crate::debug::log(
                "runtime",
                &format!(
                    "CachedHistoryLoaded channel_id={channel_id} messages={}",
                    messages.len()
                ),
            );
            events.send_event(RuntimeEvent::HistoryLoaded {
                channel_id: channel_id.to_string(),
                messages,
                has_more: false,
                next_cursor: None,
                append_older: false,
            });
        }
        Ok(None) => {}
        Err(error) => crate::debug::log(
            "runtime",
            &format!("CachedHistoryLoadFailed channel_id={channel_id} error={error:#}"),
        ),
    }
}

async fn store_history(
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
    messages: &[SlackMessage],
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    if let Err(error) = store.store_history(channel_id, messages).await {
        crate::debug::log(
            "runtime",
            &format!("CachedHistoryStoreFailed channel_id={channel_id} error={error:#}"),
        );
    }
}

async fn load_cached_thread(
    events: &Sender<RuntimeEvent>,
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
    thread_ts: &str,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    match store.load_thread(channel_id, thread_ts).await {
        Ok(Some(messages)) => {
            crate::debug::log(
                "runtime",
                &format!(
                    "CachedThreadLoaded channel_id={channel_id} ts={thread_ts} messages={}",
                    messages.len()
                ),
            );
            events.send_event(RuntimeEvent::ThreadLoaded {
                channel_id: channel_id.to_string(),
                ts: thread_ts.to_string(),
                messages,
                has_more: false,
                next_cursor: None,
                append_older: false,
            });
        }
        Ok(None) => {}
        Err(error) => crate::debug::log(
            "runtime",
            &format!(
                "CachedThreadLoadFailed channel_id={channel_id} ts={thread_ts} error={error:#}"
            ),
        ),
    }
}

async fn store_thread(
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
    thread_ts: &str,
    messages: &[SlackMessage],
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    if let Err(error) = store.store_thread(channel_id, thread_ts, messages).await {
        crate::debug::log(
            "runtime",
            &format!(
                "CachedThreadStoreFailed channel_id={channel_id} ts={thread_ts} error={error:#}"
            ),
        );
    }
}

fn require_slack(slack: &Option<SlackApi>) -> Result<&SlackApi> {
    slack.as_ref().context("No Slack workspace is available")
}

trait EventSenderExt {
    fn send_status(&self, status: &str);
    fn send_event(&self, event: RuntimeEvent);
}

impl EventSenderExt for Sender<RuntimeEvent> {
    fn send_status(&self, status: &str) {
        self.send_event(RuntimeEvent::Status(status.to_string()));
    }

    fn send_event(&self, event: RuntimeEvent) {
        let _ = self.send(event);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn image_asset_cache_key_is_stable_hex_digest() {
        assert_eq!(
            image_asset_cache_key("https://files.example/image.png"),
            "7db09e79cb28f1be72da3c1449cd42619e048f148310325cc2c8f55cd713aa0e"
        );
    }

    #[test]
    fn workspace_store_id_uses_team_and_user_identity() {
        let auth = AuthInfo {
            team: Some("Example".to_string()),
            team_id: Some("T123".to_string()),
            user_id: Some("U123".to_string()),
            ..Default::default()
        };

        assert_eq!(workspace_store_id(&auth), "T123:U123");
    }

    #[test]
    fn image_asset_cache_round_trips_data_uri() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-image-cache-test-{}-{unique}",
            std::process::id()
        ));
        let cache = ImageAssetCache::new(directory.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime");

        runtime.block_on(async {
            assert_eq!(
                cache
                    .load("https://files.example/image.png")
                    .await
                    .expect("cache load failed"),
                None
            );

            cache
                .store(
                    "https://files.example/image.png",
                    "data:image/png;base64,abc",
                )
                .await
                .expect("cache store failed");

            assert_eq!(
                cache
                    .load("https://files.example/image.png")
                    .await
                    .expect("cache load failed")
                    .as_deref(),
                Some("data:image/png;base64,abc")
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }
}
