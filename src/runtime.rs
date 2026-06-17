use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

use crate::auth::{OAuthConfig, SlackOAuthClient, TokenStore};
use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SlackConversation, SlackMessage, StoredToken,
};
use crate::slack::SlackApi;

#[derive(Debug)]
pub enum RuntimeCommand {
    LoadStoredToken,
    StartOAuth {
        client_id: String,
        debug_auth: bool,
    },
    SignOut,
    RefreshConversations,
    LoadHistory {
        channel_id: String,
    },
    LoadThread {
        channel_id: String,
        ts: String,
    },
    SearchMessages {
        query: String,
    },
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
    },
    ThreadLoaded {
        channel_id: String,
        ts: String,
        messages: Vec<SlackMessage>,
    },
    SearchLoaded(Vec<SearchMatch>),
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
        message: SlackMessage,
    },
    ReactionUpdated {
        channel_id: String,
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
            let mut slack: Option<SlackApi> = None;
            let mut user_cache = HashMap::new();

            while let Ok(command) = receiver.recv() {
                let result = runtime.block_on(handle_command(
                    command,
                    &events,
                    &token_store,
                    &oauth,
                    &mut slack,
                    &mut user_cache,
                ));
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

async fn handle_command(
    command: RuntimeCommand,
    events: &Sender<RuntimeEvent>,
    token_store: &TokenStore,
    oauth: &SlackOAuthClient,
    slack: &mut Option<SlackApi>,
    user_cache: &mut HashMap<String, String>,
) -> Result<()> {
    match command {
        RuntimeCommand::LoadStoredToken => {
            events.send_status("Checking secure storage");
            if let Some(mut token) = token_store.load()? {
                if token.should_refresh() {
                    events.send_status("Refreshing Slack session");
                    token = oauth.refresh(&token).await?;
                    token_store.save(&token)?;
                }
                connect_with_token(events, slack, token).await?;
                user_cache.clear();
                load_conversations(events, slack).await?;
            } else {
                events.send_event(RuntimeEvent::SignedOut);
            }
        }
        RuntimeCommand::StartOAuth {
            client_id,
            debug_auth,
        } => {
            events.send_status("Opening Slack authorization");
            let token = oauth
                .authenticate(OAuthConfig::new(client_id), debug_auth)
                .await?;
            token_store.save(&token)?;
            connect_with_token(events, slack, token).await?;
            user_cache.clear();
            load_conversations(events, slack).await?;
        }
        RuntimeCommand::SignOut => {
            token_store.clear()?;
            *slack = None;
            user_cache.clear();
            events.send_event(RuntimeEvent::SignedOut);
        }
        RuntimeCommand::RefreshConversations => {
            load_conversations(events, slack).await?;
        }
        RuntimeCommand::LoadHistory { channel_id } => {
            let api = require_slack(slack)?;
            events.send_status("Loading conversation");
            let messages = api.history(&channel_id).await?;
            events.send_event(RuntimeEvent::HistoryLoaded {
                channel_id,
                messages,
            });
        }
        RuntimeCommand::LoadThread { channel_id, ts } => {
            let api = require_slack(slack)?;
            events.send_status("Loading thread");
            let messages = api.thread_replies(&channel_id, &ts).await?;
            events.send_event(RuntimeEvent::ThreadLoaded {
                channel_id,
                ts,
                messages,
            });
        }
        RuntimeCommand::SearchMessages { query } => {
            let api = require_slack(slack)?;
            let results = api.search_messages(&query).await?;
            events.send_event(RuntimeEvent::SearchLoaded(results));
        }
        RuntimeCommand::LoadSavedItems => {
            let api = require_slack(slack)?;
            let items = api.saved_items().await?;
            events.send_event(RuntimeEvent::SavedItemsLoaded(items));
        }
        RuntimeCommand::LoadUser { user_id } => {
            if let Some(display_name) = user_cache.get(&user_id).cloned() {
                events.send_event(RuntimeEvent::UserLoaded {
                    user_id,
                    display_name,
                });
            } else {
                let api = require_slack(slack)?;
                let display_name = api.user_display_name(&user_id).await?;
                user_cache.insert(user_id.clone(), display_name.clone());
                events.send_event(RuntimeEvent::UserLoaded {
                    user_id,
                    display_name,
                });
            }
        }
        RuntimeCommand::LoadImageAsset { key, url } => {
            let api = require_slack(slack)?;
            match api.download_image(&url).await {
                Ok(image) => {
                    let data_uri = format!(
                        "data:{};base64,{}",
                        image.mime_type,
                        BASE64.encode(image.bytes)
                    );
                    events.send_event(RuntimeEvent::ImageAssetLoaded { key, data_uri });
                }
                Err(_) => {
                    events.send_event(RuntimeEvent::ImageAssetFailed { key });
                }
            }
        }
        RuntimeCommand::PostMessage {
            channel_id,
            text,
            thread_ts,
        } => {
            let api = require_slack(slack)?;
            let message = api
                .post_message(&channel_id, &text, thread_ts.as_deref())
                .await?;
            events.send_event(RuntimeEvent::MessagePosted {
                channel_id,
                message,
            });
        }
        RuntimeCommand::SetReaction {
            channel_id,
            ts,
            name,
            add,
            thread_ts,
        } => {
            let api = require_slack(slack)?;
            api.set_reaction(&channel_id, &ts, &name, add).await?;
            events.send_event(RuntimeEvent::ReactionUpdated {
                channel_id,
                thread_ts,
            });
        }
        RuntimeCommand::UploadFile {
            channel_id,
            path,
            initial_comment,
        } => {
            let api = require_slack(slack)?;
            events.send_event(RuntimeEvent::FileUploadProgress {
                fraction: 0.05,
                label: "Preparing upload".to_string(),
            });
            let progress_events = events.clone();
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
            events.send_event(RuntimeEvent::FileUploaded(label));
        }
    }

    Ok(())
}

async fn connect_with_token(
    events: &Sender<RuntimeEvent>,
    slack: &mut Option<SlackApi>,
    token: StoredToken,
) -> Result<()> {
    let token_team = token.team_name.clone().or(token.team_id.clone());
    let token_user = token.user_id.clone();
    let api = SlackApi::new(token);
    let mut auth = api.auth_test().await?;
    auth.team = auth.team.or(token_team);
    auth.user_id = auth.user_id.or(token_user);
    *slack = Some(api);
    events.send_event(RuntimeEvent::Authenticated(auth));
    Ok(())
}

async fn load_conversations(
    events: &Sender<RuntimeEvent>,
    slack: &mut Option<SlackApi>,
) -> Result<()> {
    let api = require_slack(slack)?;
    events.send_status("Loading conversations");
    let conversations = api.conversations().await?;
    events.send_event(RuntimeEvent::ConversationsLoaded(conversations));
    Ok(())
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
