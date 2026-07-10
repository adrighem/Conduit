use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use sha2::{Digest, Sha256};

use crate::auth::{
    browser_session_token_from_env, browser_session_token_from_values, OAuthConfig,
    SlackOAuthClient, TokenStore,
};
use crate::config;
use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SlackConversation, SlackFile, SlackMessage, SlackUnreadState,
    SlackUserGroup, StoredToken,
};
use crate::slack::{DownloadedImage, SlackApi, SlackMessagePage, CHANNEL_HISTORY_PAGE_LIMIT};
use crate::socket_mode::{self, SocketModeDisconnect, SocketModeEvent};
use crate::store::WorkspaceStore;

const CHANNEL_HISTORY_PREFETCH_LIMIT: usize = 12;
const UNREAD_STATE_PRIORITY_REFRESH_LIMIT: usize = 30;
const SOCKET_MODE_INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const SOCKET_MODE_MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);

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
    ConversationsLoadFailed(String),
    ConversationUnreadUpdated {
        channel_id: String,
        unread_state: SlackUnreadState,
    },
    ConversationNotificationCandidate {
        channel_id: String,
        messages: Vec<SlackMessage>,
    },
    HistoryLoaded {
        channel_id: String,
        messages: Vec<SlackMessage>,
        has_more: bool,
        next_cursor: Option<String>,
        append_older: bool,
        cached: bool,
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
    UserNamesLoaded(HashMap<String, String>),
    UserGroupsLoaded {
        names: HashMap<String, String>,
        members: HashMap<String, Vec<String>>,
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
    SocketModeEvent(SocketModeEvent),
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
            let mut socket_mode: Option<tokio::task::JoinHandle<()>> = None;

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
                    socket_mode: &mut socket_mode,
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
    socket_mode: &'a mut Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConversationRefreshMode {
    Background,
}

fn conversation_refresh_mode() -> ConversationRefreshMode {
    ConversationRefreshMode::Background
}

fn cached_dm_user_ids(
    conversations: &[SlackConversation],
    user_cache: &HashMap<String, String>,
) -> Vec<String> {
    let mut user_ids = conversations
        .iter()
        .filter(|conversation| conversation.is_im.unwrap_or(false))
        .filter_map(|conversation| conversation.user.as_deref())
        .filter(|user_id| user_cache.contains_key(*user_id))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    user_ids.sort();
    user_ids.dedup();
    user_ids
}

fn recent_history_preview(mut messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    messages.sort_by(|left, right| right.ts.cmp(&left.ts));
    messages.dedup_by(|left, right| !left.ts.is_empty() && left.ts == right.ts);
    messages.truncate(CHANNEL_HISTORY_PAGE_LIMIT);
    messages
}

#[derive(Debug, Clone)]
struct ChannelHistoryPrefetchCandidate {
    id: String,
    unread: bool,
    unread_count: u64,
    activity_score: f64,
    title: String,
}

fn channel_history_prefetch_candidates(conversations: &[SlackConversation]) -> Vec<String> {
    let mut candidates = conversations
        .iter()
        .filter_map(channel_history_prefetch_candidate)
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .unread
            .cmp(&left.unread)
            .then_with(|| right.unread_count.cmp(&left.unread_count))
            .then_with(|| right.activity_score.total_cmp(&left.activity_score))
            .then_with(|| left.title.cmp(&right.title))
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates.truncate(CHANNEL_HISTORY_PREFETCH_LIMIT);
    candidates
        .into_iter()
        .map(|candidate| candidate.id)
        .collect()
}

fn conversation_unread_refresh_candidates(conversations: &[SlackConversation]) -> Vec<String> {
    let mut candidates = conversations
        .iter()
        .filter(|conversation| !conversation.is_archived.unwrap_or(false))
        .filter(|conversation| !conversation.id.trim().is_empty())
        .map(|conversation| {
            (
                conversation.display_name().to_lowercase(),
                conversation.id.clone(),
            )
        })
        .collect::<Vec<_>>();

    candidates.sort();
    candidates.dedup_by(|left, right| left.1 == right.1);
    candidates
        .into_iter()
        .map(|(_, channel_id)| channel_id)
        .collect()
}

fn channel_history_prefetch_candidate(
    conversation: &SlackConversation,
) -> Option<ChannelHistoryPrefetchCandidate> {
    if conversation.is_archived.unwrap_or(false) {
        return None;
    }

    let is_channel = conversation.is_channel.unwrap_or(false)
        || conversation.is_group.unwrap_or(false)
        || conversation.is_private.unwrap_or(false)
        || conversation.is_im.unwrap_or(false)
        || conversation.is_mpim.unwrap_or(false);
    if !is_channel
        || ((conversation.is_im.unwrap_or(false) || conversation.is_mpim.unwrap_or(false))
            && !conversation.has_unread_activity())
    {
        return None;
    }

    Some(ChannelHistoryPrefetchCandidate {
        id: conversation.id.clone(),
        unread: conversation.has_unread_activity(),
        unread_count: conversation.unread_activity_count(),
        activity_score: conversation_activity_score(conversation),
        title: conversation.display_name().to_lowercase(),
    })
}

fn conversation_activity_score(conversation: &SlackConversation) -> f64 {
    [
        "last_read",
        "updated",
        "updated_at",
        "created",
        "latest",
        "latest_ts",
    ]
    .into_iter()
    .filter_map(|key| conversation.extra.get(key).and_then(slack_numeric_value))
    .fold(0.0, f64::max)
}

fn slack_numeric_value(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(value) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
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
            stop_socket_mode(context);
            context.token_store.clear()?;
            *context.slack = None;
            *context.workspace_store = None;
            context.user_cache.clear();
            context.events.send_event(RuntimeEvent::SignedOut);
        }
        RuntimeCommand::RefreshConversations => {
            crate::debug::log("runtime", "RefreshConversations");
            debug_assert_eq!(
                conversation_refresh_mode(),
                ConversationRefreshMode::Background
            );
            spawn_conversation_refresh(context)?;
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
            store_merged_history(context.workspace_store, &channel_id, &page.messages).await;
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
                store_user_name(context.workspace_store, &user_id, &display_name).await;
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
    load_cached_user_names(context.events, context.workspace_store, context.user_cache).await;
    load_cached_conversations(context.events, context.workspace_store).await;
    debug_assert_eq!(
        conversation_refresh_mode(),
        ConversationRefreshMode::Background
    );
    start_socket_mode(context);
    spawn_conversation_refresh(context)?;
    spawn_user_group_refresh(context)?;
    Ok(())
}

fn start_socket_mode(context: &mut RuntimeContext<'_>) {
    stop_socket_mode(context);

    let Some(app_token) = config::slack_app_token() else {
        crate::debug::log("socket", "SocketModeDisabled reason=missing_app_token");
        return;
    };

    crate::debug::log("socket", "SocketModeStarting");
    let events = context.events.clone();
    let handle = tokio::spawn(run_socket_mode(app_token, events));
    *context.socket_mode = Some(handle);
}

fn stop_socket_mode(context: &mut RuntimeContext<'_>) {
    if let Some(handle) = context.socket_mode.take() {
        handle.abort();
        crate::debug::log("socket", "SocketModeStopped");
    }
}

async fn run_socket_mode(app_token: String, events: Sender<RuntimeEvent>) {
    let mut reconnect_delay = SOCKET_MODE_INITIAL_RECONNECT_DELAY;

    loop {
        let events_for_run = events.clone();
        let result = socket_mode::run_once(&app_token, move |event| {
            events_for_run.send_event(RuntimeEvent::SocketModeEvent(event));
        })
        .await;

        let timing = match result {
            Ok(SocketModeDisconnect::LinkDisabled) => {
                crate::debug::log(
                    "socket",
                    "SocketModeDisconnected reason=link_disabled; retrying until enabled",
                );
                socket_mode_reconnect_timing(
                    reconnect_delay,
                    Some(SocketModeDisconnect::LinkDisabled),
                )
            }
            Ok(disconnect) => {
                crate::debug::log(
                    "socket",
                    &format!("SocketModeDisconnected reason={disconnect:?}"),
                );
                socket_mode_reconnect_timing(reconnect_delay, Some(disconnect))
            }
            Err(error) => {
                crate::debug::log("socket", &format!("SocketModeError error={error:#}"));
                socket_mode_reconnect_timing(reconnect_delay, None)
            }
        };

        reconnect_delay = timing.next_backoff;
        tokio::time::sleep(timing.sleep).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketModeReconnectTiming {
    sleep: Duration,
    next_backoff: Duration,
}

fn socket_mode_reconnect_timing(
    current: Duration,
    disconnect: Option<SocketModeDisconnect>,
) -> SocketModeReconnectTiming {
    if matches!(disconnect, None | Some(SocketModeDisconnect::LinkDisabled)) {
        return SocketModeReconnectTiming {
            sleep: current,
            next_backoff: current
                .saturating_mul(2)
                .min(SOCKET_MODE_MAX_RECONNECT_DELAY),
        };
    }

    SocketModeReconnectTiming {
        sleep: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
        next_backoff: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
    }
}

fn spawn_conversation_refresh(context: &RuntimeContext<'_>) -> Result<()> {
    let api = require_slack(context.slack)?.clone();
    let events = context.events.clone();
    let workspace_store = (*context.workspace_store).clone();
    let cached_user_names = context.user_cache.clone();

    tokio::spawn(async move {
        if let Err(error) = load_conversations_best_effort_with_api(
            &events,
            &api,
            &workspace_store,
            cached_user_names,
        )
        .await
        {
            crate::debug::log(
                "runtime",
                &format!("ConversationsBackgroundRefreshFailed error={error:#}"),
            );
        }
    });

    Ok(())
}

fn spawn_user_group_refresh(context: &RuntimeContext<'_>) -> Result<()> {
    let api = require_slack(context.slack)?.clone();
    let events = context.events.clone();
    let workspace_store = (*context.workspace_store).clone();
    let cached_user_names = context.user_cache.clone();

    tokio::spawn(async move {
        load_user_groups_best_effort_with_api(&events, &api, &workspace_store, cached_user_names)
            .await;
    });

    Ok(())
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

async fn load_user_groups_best_effort_with_api(
    events: &Sender<RuntimeEvent>,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    cached_user_names: HashMap<String, String>,
) {
    let groups = match api.user_groups().await {
        Ok(groups) => groups,
        Err(error) => {
            crate::debug::log("runtime", &format!("UserGroupsLoadFailed error={error:#}"));
            return;
        }
    };

    let (names, members, loaded_user_names) =
        resolve_user_group_display_data(api, groups, cached_user_names).await;

    if !loaded_user_names.is_empty() {
        store_user_names(workspace_store, &loaded_user_names).await;
        events.send_event(RuntimeEvent::UserNamesLoaded(loaded_user_names));
    }

    if !names.is_empty() {
        crate::debug::log(
            "runtime",
            &format!("UserGroupsLoaded count={}", names.len()),
        );
        events.send_event(RuntimeEvent::UserGroupsLoaded { names, members });
    }
}

async fn resolve_user_group_display_data(
    api: &SlackApi,
    groups: Vec<SlackUserGroup>,
    mut known_user_names: HashMap<String, String>,
) -> (
    HashMap<String, String>,
    HashMap<String, Vec<String>>,
    HashMap<String, String>,
) {
    let mut names = HashMap::new();
    let mut members = HashMap::new();
    let mut loaded_user_names = HashMap::new();

    for group in groups {
        if group.id.trim().is_empty() {
            continue;
        }

        names.insert(group.id.clone(), group.mention_label());
        let mut member_names = Vec::new();
        for user_id in group
            .users
            .iter()
            .filter(|user_id| !user_id.trim().is_empty())
        {
            if let Some(display_name) = known_user_names.get(user_id).cloned() {
                member_names.push(display_name);
                continue;
            }

            match api.user_display_name(user_id).await {
                Ok(display_name) => {
                    known_user_names.insert(user_id.clone(), display_name.clone());
                    loaded_user_names.insert(user_id.clone(), display_name.clone());
                    member_names.push(display_name);
                }
                Err(error) => {
                    crate::debug::log(
                        "runtime",
                        &format!("UserGroupMemberNameLoadFailed user_id={user_id} error={error:#}"),
                    );
                    member_names.push(user_id.clone());
                }
            }
        }

        if !member_names.is_empty() {
            member_names.sort();
            member_names.dedup();
            members.insert(group.id, member_names);
        }
    }

    (names, members, loaded_user_names)
}

async fn load_conversations_with_api(
    events: &Sender<RuntimeEvent>,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
) -> Result<Vec<SlackConversation>> {
    events.send_status("Loading conversations");
    let conversations = api.conversations().await?;
    store_conversations(workspace_store, &conversations).await;
    crate::debug::log(
        "runtime",
        &format!("ConversationsLoaded count={}", conversations.len()),
    );
    events.send_event(RuntimeEvent::ConversationsLoaded(conversations.clone()));
    Ok(conversations)
}

async fn load_conversations_best_effort_with_api(
    events: &Sender<RuntimeEvent>,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    cached_user_names: HashMap<String, String>,
) -> Result<()> {
    match load_conversations_with_api(events, api, workspace_store).await {
        Ok(conversations) => {
            let unread_refresh_candidates = conversation_unread_refresh_candidates(&conversations);
            refresh_conversation_unread_states_best_effort(
                events,
                api,
                unread_refresh_candidates
                    .iter()
                    .take(UNREAD_STATE_PRIORITY_REFRESH_LIMIT),
            )
            .await;
            prefetch_channel_histories_best_effort(events, api, workspace_store, &conversations)
                .await;
            refresh_cached_dm_user_names(
                events,
                api,
                workspace_store,
                &conversations,
                &cached_user_names,
            )
            .await;
        }
        Err(error) => handle_conversations_load_error(events, error),
    }
    Ok(())
}

async fn refresh_conversation_unread_states_best_effort<'a>(
    events: &Sender<RuntimeEvent>,
    api: &SlackApi,
    channel_ids: impl IntoIterator<Item = &'a String>,
) {
    for channel_id in channel_ids {
        match api.unread_state(channel_id).await {
            Ok(unread_state) => {
                crate::debug::log(
                    "runtime",
                    &format!(
                        "ConversationUnreadRefreshed channel_id={channel_id} known={} unread={} display_count={}",
                        unread_state.known, unread_state.has_unread, unread_state.display_count
                    ),
                );
                send_conversation_unread_update(events, channel_id, unread_state);
            }
            Err(error) => crate::debug::log(
                "runtime",
                &format!("ConversationUnreadRefreshFailed channel_id={channel_id} error={error:#}"),
            ),
        }
    }
}

async fn prefetch_channel_histories_best_effort(
    events: &Sender<RuntimeEvent>,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    conversations: &[SlackConversation],
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    let channel_ids = channel_history_prefetch_candidates(conversations);
    if channel_ids.is_empty() {
        return;
    }

    crate::debug::log(
        "runtime",
        &format!("ChannelHistoryPrefetchStart count={}", channel_ids.len()),
    );

    for channel_id in channel_ids {
        match store.load_history(&channel_id).await {
            Ok(Some(_)) => {
                crate::debug::log(
                    "runtime",
                    &format!(
                        "ChannelHistoryPrefetchRefreshing channel_id={channel_id} reason=cached"
                    ),
                );
            }
            Ok(None) => {}
            Err(error) => {
                crate::debug::log(
                    "runtime",
                    &format!("ChannelHistoryPrefetchCacheCheckFailed channel_id={channel_id} error={error:#}"),
                );
                continue;
            }
        }

        match api.history(&channel_id).await {
            Ok(page) => {
                send_conversation_unread_update(events, &channel_id, page.unread_state);
                send_conversation_notification_candidate(events, &channel_id, &page.messages);
                crate::debug::log(
                    "runtime",
                    &format!(
                        "ChannelHistoryPrefetched channel_id={channel_id} messages={}",
                        page.messages.len()
                    ),
                );
                store_history(workspace_store, &channel_id, &page.messages).await;
            }
            Err(error) => crate::debug::log(
                "runtime",
                &format!("ChannelHistoryPrefetchFailed channel_id={channel_id} error={error:#}"),
            ),
        }
    }
}

fn send_conversation_notification_candidate(
    events: &Sender<RuntimeEvent>,
    channel_id: &str,
    messages: &[SlackMessage],
) {
    if !messages.is_empty() {
        events.send_event(RuntimeEvent::ConversationNotificationCandidate {
            channel_id: channel_id.to_string(),
            messages: messages.to_vec(),
        });
    }
}

fn send_conversation_unread_update(
    events: &Sender<RuntimeEvent>,
    channel_id: &str,
    unread_state: SlackUnreadState,
) {
    if unread_state.known {
        events.send_event(RuntimeEvent::ConversationUnreadUpdated {
            channel_id: channel_id.to_string(),
            unread_state,
        });
    }
}

async fn load_cached_user_names(
    events: &Sender<RuntimeEvent>,
    workspace_store: &Option<WorkspaceStore>,
    user_cache: &mut HashMap<String, String>,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    match store.load_user_names().await {
        Ok(user_names) if !user_names.is_empty() => {
            crate::debug::log(
                "runtime",
                &format!("CachedUserNamesLoaded count={}", user_names.len()),
            );
            user_cache.extend(user_names.clone());
            events.send_event(RuntimeEvent::UserNamesLoaded(user_names));
        }
        Ok(_) => {}
        Err(error) => crate::debug::log(
            "runtime",
            &format!("CachedUserNamesLoadFailed error={error:#}"),
        ),
    }
}

async fn refresh_cached_dm_user_names(
    events: &Sender<RuntimeEvent>,
    api: &SlackApi,
    workspace_store: &Option<WorkspaceStore>,
    conversations: &[SlackConversation],
    cached_user_names: &HashMap<String, String>,
) {
    let user_ids = cached_dm_user_ids(conversations, cached_user_names);
    if user_ids.is_empty() {
        return;
    }

    let mut refreshed = HashMap::new();
    for user_id in user_ids {
        match api.user_display_name(&user_id).await {
            Ok(display_name) => {
                refreshed.insert(user_id, display_name);
            }
            Err(error) => crate::debug::log(
                "runtime",
                &format!("UserDisplayNameRefreshFailed user_id={user_id} error={error:#}"),
            ),
        }
    }

    if refreshed.is_empty() {
        return;
    }

    store_user_names(workspace_store, &refreshed).await;
    events.send_event(RuntimeEvent::UserNamesLoaded(refreshed));
}

fn handle_conversations_load_error(events: &Sender<RuntimeEvent>, error: anyhow::Error) {
    crate::debug::log(
        "runtime",
        &format!("ConversationsLoadFailed error={error:#}"),
    );
    events.send_event(RuntimeEvent::ConversationsLoadFailed(error.to_string()));
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
        cached: false,
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

async fn store_user_name(
    workspace_store: &Option<WorkspaceStore>,
    user_id: &str,
    display_name: &str,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    if let Err(error) = store.store_user_name(user_id, display_name).await {
        crate::debug::log(
            "runtime",
            &format!("CachedUserNameStoreFailed user_id={user_id} error={error:#}"),
        );
    }
}

async fn store_user_names(
    workspace_store: &Option<WorkspaceStore>,
    user_names: &HashMap<String, String>,
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    if let Err(error) = store.store_user_names(user_names).await {
        crate::debug::log(
            "runtime",
            &format!(
                "CachedUserNamesStoreFailed count={} error={error:#}",
                user_names.len()
            ),
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
            let preview = recent_history_preview(messages);
            crate::debug::log(
                "runtime",
                &format!(
                    "CachedHistoryLoaded channel_id={channel_id} messages={}",
                    preview.len()
                ),
            );
            events.send_event(RuntimeEvent::HistoryLoaded {
                channel_id: channel_id.to_string(),
                messages: preview,
                has_more: false,
                next_cursor: None,
                append_older: false,
                cached: true,
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

async fn store_merged_history(
    workspace_store: &Option<WorkspaceStore>,
    channel_id: &str,
    messages: &[SlackMessage],
) {
    let Some(store) = workspace_store.as_ref() else {
        return;
    };

    if let Err(error) = store.store_merged_history(channel_id, messages).await {
        crate::debug::log(
            "runtime",
            &format!("CachedHistoryMergedStoreFailed channel_id={channel_id} error={error:#}"),
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
    fn conversation_refresh_runs_in_background() {
        assert_eq!(
            conversation_refresh_mode(),
            ConversationRefreshMode::Background
        );
    }

    #[test]
    fn socket_mode_reconnect_timing_backs_off_and_resets_after_socket_disconnects() {
        assert_eq!(
            socket_mode_reconnect_timing(SOCKET_MODE_INITIAL_RECONNECT_DELAY, None),
            SocketModeReconnectTiming {
                sleep: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
                next_backoff: Duration::from_secs(2),
            }
        );
        assert_eq!(
            socket_mode_reconnect_timing(Duration::from_secs(20), None),
            SocketModeReconnectTiming {
                sleep: Duration::from_secs(20),
                next_backoff: SOCKET_MODE_MAX_RECONNECT_DELAY,
            }
        );
        assert_eq!(
            socket_mode_reconnect_timing(
                SOCKET_MODE_MAX_RECONNECT_DELAY,
                Some(SocketModeDisconnect::LinkDisabled),
            ),
            SocketModeReconnectTiming {
                sleep: SOCKET_MODE_MAX_RECONNECT_DELAY,
                next_backoff: SOCKET_MODE_MAX_RECONNECT_DELAY,
            }
        );
        assert_eq!(
            socket_mode_reconnect_timing(
                SOCKET_MODE_MAX_RECONNECT_DELAY,
                Some(SocketModeDisconnect::RefreshRequested),
            ),
            SocketModeReconnectTiming {
                sleep: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
                next_backoff: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
            }
        );
        assert_eq!(
            socket_mode_reconnect_timing(
                Duration::from_secs(20),
                Some(SocketModeDisconnect::Warning),
            ),
            SocketModeReconnectTiming {
                sleep: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
                next_backoff: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
            }
        );
    }

    #[test]
    fn cached_dm_user_ids_selects_only_known_direct_messages() {
        let conversations = vec![
            SlackConversation {
                id: "D123".to_string(),
                user: Some("U123".to_string()),
                is_im: Some(true),
                ..Default::default()
            },
            SlackConversation {
                id: "D999".to_string(),
                user: Some("U999".to_string()),
                is_im: Some(true),
                ..Default::default()
            },
            SlackConversation {
                id: "C123".to_string(),
                user: Some("U123".to_string()),
                is_channel: Some(true),
                ..Default::default()
            },
        ];
        let user_cache = HashMap::from([("U123".to_string(), "Ada".to_string())]);

        assert_eq!(
            cached_dm_user_ids(&conversations, &user_cache),
            vec!["U123"]
        );
    }

    fn channel(id: &str, unread_count: u64, last_read: Option<&str>) -> SlackConversation {
        let mut conversation = SlackConversation {
            id: id.to_string(),
            name: Some(
                id.trim_start_matches("C-")
                    .trim_start_matches('C')
                    .to_string(),
            ),
            is_channel: Some(true),
            unread_count: Some(unread_count),
            ..Default::default()
        };
        if let Some(last_read) = last_read {
            conversation
                .extra
                .insert("last_read".to_string(), serde_json::json!(last_read));
        }
        conversation
    }

    fn private_channel(id: &str, unread_count: u64, last_read: Option<&str>) -> SlackConversation {
        SlackConversation {
            is_channel: Some(false),
            is_group: Some(true),
            is_private: Some(true),
            ..channel(id, unread_count, last_read)
        }
    }

    fn archived_channel(id: &str, unread_count: u64) -> SlackConversation {
        SlackConversation {
            is_archived: Some(true),
            ..channel(id, unread_count, None)
        }
    }

    fn dm(id: &str, unread_count: u64) -> SlackConversation {
        SlackConversation {
            id: id.to_string(),
            user: Some("U123".to_string()),
            is_im: Some(true),
            unread_count: Some(unread_count),
            ..Default::default()
        }
    }

    #[test]
    fn recent_history_preview_keeps_latest_page_only() {
        let count = CHANNEL_HISTORY_PAGE_LIMIT + 5;
        let messages = (0..count)
            .map(|index| SlackMessage {
                ts: format!("1710000{index:03}.000000"),
                text: Some(format!("message {index}")),
                ..Default::default()
            })
            .collect::<Vec<_>>();

        let preview = recent_history_preview(messages);
        let first_ts = format!("1710000{:03}.000000", count - 1);
        let last_ts = format!("1710000{:03}.000000", count - CHANNEL_HISTORY_PAGE_LIMIT);

        assert_eq!(preview.len(), CHANNEL_HISTORY_PAGE_LIMIT);
        assert_eq!(
            preview.first().map(|message| message.ts.as_str()),
            Some(first_ts.as_str())
        );
        assert_eq!(
            preview.last().map(|message| message.ts.as_str()),
            Some(last_ts.as_str())
        );
    }

    #[test]
    fn channel_history_prefetch_candidates_prioritize_unread_and_recent_channels() {
        let mut badgeless_unread = channel("C-badgeless", 0, Some("1710000100.000000"));
        badgeless_unread
            .extra
            .insert("has_unreads".to_string(), serde_json::json!(true));
        let conversations = vec![
            channel("C-old", 0, None),
            dm("D-unread", 99),
            archived_channel("C-archived", 99),
            channel("C-recent", 0, Some("1710000300.000000")),
            channel("C-unread", 4, Some("1710000000.000000")),
            badgeless_unread,
            private_channel("G-private", 0, Some("1710000200.000000")),
        ];

        assert_eq!(
            channel_history_prefetch_candidates(&conversations),
            vec![
                "D-unread",
                "C-unread",
                "C-badgeless",
                "C-recent",
                "G-private",
                "C-old"
            ]
        );
    }

    #[test]
    fn channel_history_prefetch_candidates_are_bounded() {
        let conversations = (0..CHANNEL_HISTORY_PREFETCH_LIMIT + 3)
            .map(|index| channel(&format!("C{index}"), index as u64, None))
            .collect::<Vec<_>>();

        let candidates = channel_history_prefetch_candidates(&conversations);

        assert_eq!(candidates.len(), CHANNEL_HISTORY_PREFETCH_LIMIT);
        assert_eq!(candidates.first().map(String::as_str), Some("C14"));
        assert_eq!(candidates.last().map(String::as_str), Some("C3"));
    }

    #[test]
    fn conversation_unread_refresh_candidates_cover_visible_titles_first() {
        let conversations = vec![
            channel("C-zebra", 0, None),
            archived_channel("C-archived", 10),
            dm("D-ada", 0),
            channel("C-aggregator", 0, None),
            channel("C-127", 0, None),
        ];

        assert_eq!(
            conversation_unread_refresh_candidates(&conversations),
            vec!["C-127", "C-aggregator", "C-zebra", "D-ada"]
        );
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
