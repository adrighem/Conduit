use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::models::{SlackConversation, SlackMessage};

const CACHE_VERSION: u32 = 1;
const MAX_CACHED_CHANNEL_MESSAGES: usize = 200;

#[derive(Clone, Debug)]
pub struct WorkspaceStore {
    directory: PathBuf,
    workspace_key: String,
}

impl WorkspaceStore {
    pub fn new(directory: PathBuf, workspace_id: &str) -> Self {
        Self {
            directory,
            workspace_key: cache_key(workspace_id),
        }
    }

    pub async fn load_conversations(&self) -> Result<Option<Vec<SlackConversation>>> {
        Ok(self
            .load_state()
            .await?
            .map(|state| state.conversations)
            .filter(|conversations| !conversations.is_empty()))
    }

    pub async fn store_conversations(&self, conversations: &[SlackConversation]) -> Result<()> {
        let mut state = self.load_state_for_update().await;
        state.conversations = conversations.to_vec();
        self.store_state(&state).await
    }

    pub async fn load_user_names(&self) -> Result<HashMap<String, String>> {
        Ok(self
            .load_state()
            .await?
            .map(|state| state.user_names)
            .unwrap_or_default())
    }

    pub async fn store_user_name(&self, user_id: &str, display_name: &str) -> Result<()> {
        let mut names = HashMap::new();
        names.insert(user_id.to_string(), display_name.to_string());
        self.store_user_names(&names).await
    }

    pub async fn store_user_names(&self, user_names: &HashMap<String, String>) -> Result<()> {
        let mut state = self.load_state_for_update().await;
        state.user_names.extend(
            user_names
                .iter()
                .filter(|(user_id, display_name)| {
                    !user_id.trim().is_empty() && !display_name.trim().is_empty()
                })
                .map(|(user_id, display_name)| (user_id.clone(), display_name.clone())),
        );
        self.store_state(&state).await
    }

    pub async fn load_history(&self, channel_id: &str) -> Result<Option<Vec<SlackMessage>>> {
        Ok(self
            .load_state()
            .await?
            .and_then(|state| state.channel_histories.get(channel_id).cloned())
            .filter(|messages| !messages.is_empty()))
    }

    pub async fn store_history(&self, channel_id: &str, messages: &[SlackMessage]) -> Result<()> {
        let mut state = self.load_state_for_update().await;
        state
            .channel_histories
            .insert(channel_id.to_string(), pruned_history(messages.to_vec()));
        self.store_state(&state).await
    }

    pub async fn store_merged_history(
        &self,
        channel_id: &str,
        messages: &[SlackMessage],
    ) -> Result<()> {
        let mut state = self.load_state_for_update().await;
        let existing = state
            .channel_histories
            .get(channel_id)
            .cloned()
            .unwrap_or_default();
        state.channel_histories.insert(
            channel_id.to_string(),
            merge_history_pages(&existing, messages),
        );
        self.store_state(&state).await
    }

    pub async fn load_thread(
        &self,
        channel_id: &str,
        thread_ts: &str,
    ) -> Result<Option<Vec<SlackMessage>>> {
        let key = thread_key(channel_id, thread_ts);
        Ok(self
            .load_state()
            .await?
            .and_then(|state| state.thread_replies.get(&key).cloned())
            .filter(|messages| !messages.is_empty()))
    }

    pub async fn store_thread(
        &self,
        channel_id: &str,
        thread_ts: &str,
        messages: &[SlackMessage],
    ) -> Result<()> {
        let mut state = self.load_state_for_update().await;
        state
            .thread_replies
            .insert(thread_key(channel_id, thread_ts), messages.to_vec());
        self.store_state(&state).await
    }

    async fn load_state(&self) -> Result<Option<CachedWorkspaceState>> {
        let path = self.path();
        let data = match tokio::fs::read_to_string(&path).await {
            Ok(data) => data,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read state cache {}", path.display()));
            }
        };

        let state: CachedWorkspaceState = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse state cache {}", path.display()))?;
        if state.version == CACHE_VERSION {
            Ok(Some(state))
        } else {
            Ok(None)
        }
    }

    async fn load_state_for_update(&self) -> CachedWorkspaceState {
        match self.load_state().await {
            Ok(Some(state)) => state,
            Ok(None) => CachedWorkspaceState::new(),
            Err(error) => {
                crate::debug::log(
                    "store",
                    &format!("StateCacheUpdateDiscardedUnreadableExistingState error={error:#}"),
                );
                CachedWorkspaceState::new()
            }
        }
    }

    async fn store_state(&self, state: &CachedWorkspaceState) -> Result<()> {
        tokio::fs::create_dir_all(&self.directory)
            .await
            .with_context(|| {
                format!(
                    "failed to create state cache directory {}",
                    self.directory.display()
                )
            })?;

        let path = self.path();
        let tmp_path = path.with_extension("json.tmp");
        let serialized =
            serde_json::to_string_pretty(state).context("failed to serialize state")?;
        tokio::fs::write(&tmp_path, serialized)
            .await
            .with_context(|| format!("failed to write state cache {}", tmp_path.display()))?;
        tokio::fs::rename(&tmp_path, &path)
            .await
            .with_context(|| format!("failed to replace state cache {}", path.display()))
    }

    fn path(&self) -> PathBuf {
        self.directory.join(format!("{}.json", self.workspace_key))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedWorkspaceState {
    version: u32,
    #[serde(default)]
    conversations: Vec<SlackConversation>,
    #[serde(default)]
    user_names: HashMap<String, String>,
    #[serde(default)]
    channel_histories: HashMap<String, Vec<SlackMessage>>,
    #[serde(default)]
    thread_replies: HashMap<String, Vec<SlackMessage>>,
}

impl CachedWorkspaceState {
    fn new() -> Self {
        Self {
            version: CACHE_VERSION,
            conversations: Vec::new(),
            user_names: HashMap::new(),
            channel_histories: HashMap::new(),
            thread_replies: HashMap::new(),
        }
    }
}

fn cache_key(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn thread_key(channel_id: &str, thread_ts: &str) -> String {
    format!("{channel_id}:{thread_ts}")
}

fn merge_history_pages(existing: &[SlackMessage], page: &[SlackMessage]) -> Vec<SlackMessage> {
    let mut messages = existing.to_vec();
    messages.extend(page.iter().cloned());
    pruned_history(messages)
}

fn pruned_history(mut messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    messages.sort_by(|left, right| right.ts.cmp(&left.ts));
    messages.dedup_by(|left, right| !left.ts.is_empty() && left.ts == right.ts);
    messages.truncate(MAX_CACHED_CHANNEL_MESSAGES);
    messages
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_cache_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("conduit-{name}-{}-{unique}", std::process::id()))
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build test runtime")
    }

    #[test]
    fn workspace_store_round_trips_cached_snapshots() {
        let directory = temp_cache_dir("workspace-store");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            assert!(store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .is_none());

            let conversations = vec![SlackConversation {
                id: "C123".to_string(),
                name: Some("general".to_string()),
                is_channel: Some(true),
                ..Default::default()
            }];
            store
                .store_conversations(&conversations)
                .await
                .expect("conversation store failed");
            assert_eq!(
                store
                    .load_conversations()
                    .await
                    .expect("conversation load failed")
                    .expect("missing cached conversations")[0]
                    .id,
                "C123"
            );

            let messages = vec![SlackMessage {
                ts: "1710000000.000100".to_string(),
                text: Some("cached".to_string()),
                ..Default::default()
            }];
            store
                .store_history("C123", &messages)
                .await
                .expect("history store failed");
            assert_eq!(
                store
                    .load_history("C123")
                    .await
                    .expect("history load failed")
                    .expect("missing cached history")[0]
                    .body_text(),
                "cached"
            );

            store
                .store_thread("C123", "1710000000.000100", &messages)
                .await
                .expect("thread store failed");
            assert_eq!(
                store
                    .load_thread("C123", "1710000000.000100")
                    .await
                    .expect("thread load failed")
                    .expect("missing cached thread")[0]
                    .ts,
                "1710000000.000100"
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_round_trips_user_names() {
        let directory = temp_cache_dir("workspace-store-user-names");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            assert!(store
                .load_user_names()
                .await
                .expect("user name load failed")
                .is_empty());

            store
                .store_user_name("U123", "Ada Lovelace")
                .await
                .expect("user name store failed");

            assert_eq!(
                store
                    .load_user_names()
                    .await
                    .expect("user name load failed")
                    .get("U123")
                    .map(String::as_str),
                Some("Ada Lovelace")
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_cache_key_does_not_expose_workspace_identity() {
        let key = cache_key("T123:U123");

        assert_eq!(key.len(), 64);
        assert!(!key.contains("T123"));
        assert!(!key.contains("U123"));
    }

    #[test]
    fn workspace_store_replaces_invalid_cache_on_next_write() {
        let directory = temp_cache_dir("workspace-store-invalid");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        std::fs::create_dir_all(&directory).expect("failed to create cache dir");
        std::fs::write(store.path(), "not json").expect("failed to write invalid cache");

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C123".to_string(),
                    ..Default::default()
                }])
                .await
                .expect("conversation store failed");

            assert_eq!(
                store
                    .load_conversations()
                    .await
                    .expect("conversation load failed")
                    .expect("missing cached conversations")[0]
                    .id,
                "C123"
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_merges_paged_history_newest_first() {
        let directory = temp_cache_dir("workspace-store-merged-history");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_history(
                    "C123",
                    &[
                        SlackMessage {
                            ts: "1710000300.000000".to_string(),
                            text: Some("new".to_string()),
                            ..Default::default()
                        },
                        SlackMessage {
                            ts: "1710000200.000000".to_string(),
                            text: Some("middle".to_string()),
                            ..Default::default()
                        },
                    ],
                )
                .await
                .expect("history store failed");

            store
                .store_merged_history(
                    "C123",
                    &[
                        SlackMessage {
                            ts: "1710000200.000000".to_string(),
                            text: Some("duplicate".to_string()),
                            ..Default::default()
                        },
                        SlackMessage {
                            ts: "1710000100.000000".to_string(),
                            text: Some("old".to_string()),
                            ..Default::default()
                        },
                    ],
                )
                .await
                .expect("merged history store failed");

            let messages = store
                .load_history("C123")
                .await
                .expect("history load failed")
                .expect("missing cached history");
            let timestamps = messages
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>();

            assert_eq!(
                timestamps,
                vec![
                    "1710000300.000000",
                    "1710000200.000000",
                    "1710000100.000000"
                ]
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_prunes_cached_history_to_recent_bound() {
        let directory = temp_cache_dir("workspace-store-pruned-history");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let messages = (0..=MAX_CACHED_CHANNEL_MESSAGES)
                .map(|index| SlackMessage {
                    ts: format!("1710000{:03}.000000", MAX_CACHED_CHANNEL_MESSAGES - index),
                    text: Some(format!("message {index}")),
                    ..Default::default()
                })
                .collect::<Vec<_>>();

            store
                .store_history("C123", &messages)
                .await
                .expect("history store failed");

            let cached = store
                .load_history("C123")
                .await
                .expect("history load failed")
                .expect("missing cached history");

            assert_eq!(cached.len(), MAX_CACHED_CHANNEL_MESSAGES);
            assert_eq!(cached[0].ts, "1710000200.000000");
            assert_eq!(
                cached.last().map(|message| message.ts.as_str()),
                Some("1710000001.000000")
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }
}
