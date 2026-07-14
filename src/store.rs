use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::lock::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::conversation_catalog::ConversationCatalog;
use crate::models::{SlackConversation, SlackMessage, SlackUnreadState};
use crate::thread_catalog::{ThreadCatalog, ThreadRecord};

const CACHE_VERSION: u32 = 1;
const MAX_CACHED_CHANNEL_MESSAGES: usize = 200;
const SEEN_REALTIME_MESSAGE_TS_KEY: &str = "conduit_seen_realtime_message_ts";
const LOCAL_READ_TS_KEY: &str = "conduit_local_read_ts";
const MAX_SEEN_REALTIME_MESSAGES: usize = 256;

#[derive(Clone, Debug)]
pub struct WorkspaceStore {
    directory: PathBuf,
    workspace_key: String,
    update_lock: Arc<Mutex<()>>,
}

impl WorkspaceStore {
    pub fn new(directory: PathBuf, workspace_id: &str) -> Self {
        Self {
            directory,
            workspace_key: cache_key(workspace_id),
            update_lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn load_conversations(&self) -> Result<Option<Vec<SlackConversation>>> {
        Ok(self
            .load_state()
            .await?
            .map(|state| state.conversations)
            .filter(|conversations| !conversations.is_empty()))
    }

    pub async fn load_pending_unread_refresh(&self) -> Result<Vec<String>> {
        Ok(self
            .load_state()
            .await?
            .map(|state| state.pending_unread_refresh)
            .unwrap_or_default())
    }

    pub async fn store_pending_unread_refresh(&self, channel_ids: &[String]) -> Result<()> {
        self.update_state(|state| {
            state.pending_unread_refresh = channel_ids.to_vec();
            state.pending_unread_refresh.sort();
            state.pending_unread_refresh.dedup();
        })
        .await
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn store_conversations(&self, conversations: &[SlackConversation]) -> Result<()> {
        self.update_state(|state| state.conversations = conversations.to_vec())
            .await
    }

    /// Reconciles an authoritative membership response in one locked cache
    /// transaction, so concurrent realtime/read overlays cannot be replaced by
    /// an older read-modify-write cycle.
    pub async fn reconcile_conversations(
        &self,
        fresh: Vec<SlackConversation>,
    ) -> Result<Vec<SlackConversation>> {
        let _guard = self.update_lock.lock().await;
        let mut state = self.load_state_for_update().await;
        if fresh.is_empty() && !state.conversations.is_empty() {
            anyhow::bail!("Slack returned an unexpectedly empty conversation membership snapshot");
        }
        let mut catalog =
            ConversationCatalog::from_cached(std::mem::take(&mut state.conversations));
        let mut snapshot = catalog.begin_membership_snapshot();
        for conversation in fresh {
            snapshot.upsert(conversation);
        }
        catalog.commit_membership_snapshot(snapshot);
        state.conversations = catalog.conversations();
        self.store_state(&state).await?;
        Ok(state.conversations)
    }

    /// Merges one cached conversation without replacing newer unread/read
    /// overlays or the rest of the workspace snapshot.
    pub async fn store_conversation(&self, conversation: &SlackConversation) -> Result<()> {
        if conversation.id.trim().is_empty() {
            return Ok(());
        }

        self.update_state(|state| {
            let mut catalog =
                ConversationCatalog::from_cached(std::mem::take(&mut state.conversations));
            catalog.upsert_metadata(conversation.clone());
            state.conversations = catalog.conversations();
        })
        .await
    }

    pub async fn merge_conversation(&self, conversation: &SlackConversation) -> Result<()> {
        if conversation.id.trim().is_empty() {
            return Ok(());
        }
        self.update_state(|state| {
            let mut catalog =
                ConversationCatalog::from_cached(std::mem::take(&mut state.conversations));
            catalog.upsert_metadata(conversation.clone());
            state.conversations = catalog.conversations();
        })
        .await
    }

    /// Applies an unread-state patch to one cached conversation atomically.
    /// Returns `false` when the state is unknown or the conversation is not in
    /// the cache, allowing callers to decide whether a full snapshot is needed.
    pub async fn apply_conversation_unread_state(
        &self,
        channel_id: &str,
        unread_state: SlackUnreadState,
        server_last_read: Option<&str>,
    ) -> Result<bool> {
        if channel_id.trim().is_empty() || !unread_state.known {
            return Ok(false);
        }

        let _guard = self.update_lock.lock().await;
        let mut state = self.load_state_for_update().await;
        let Some(conversation) = state
            .conversations
            .iter_mut()
            .find(|conversation| conversation.id == channel_id)
        else {
            return Ok(false);
        };
        let newer_local_read = conversation
            .extra
            .get(LOCAL_READ_TS_KEY)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|local| server_last_read.is_none_or(|server| local > server));
        if newer_local_read {
            return Ok(false);
        }
        conversation.apply_unread_state(unread_state);
        self.store_state(&state).await?;
        Ok(true)
    }

    /// Clears cached unread state for one conversation atomically.
    pub async fn clear_conversation_unread_state(
        &self,
        channel_id: &str,
        last_read: &str,
    ) -> Result<bool> {
        if channel_id.trim().is_empty() {
            return Ok(false);
        }

        self.update_conversation(channel_id, |conversation| {
            conversation.clear_unread_activity();
            conversation.extra.insert(
                LOCAL_READ_TS_KEY.to_string(),
                serde_json::Value::String(last_read.to_string()),
            );
        })
        .await
    }

    pub async fn mark_conversation_unread_from_event(
        &self,
        channel_id: &str,
        message_ts: &str,
    ) -> Result<bool> {
        if channel_id.trim().is_empty() || message_ts.trim().is_empty() {
            return Ok(false);
        }

        let _guard = self.update_lock.lock().await;
        let mut state = self.load_state_for_update().await;
        let conversation = if let Some(conversation) = state
            .conversations
            .iter_mut()
            .find(|conversation| conversation.id == channel_id)
        {
            conversation
        } else {
            state.conversations.push(SlackConversation {
                id: channel_id.to_string(),
                ..Default::default()
            });
            state
                .conversations
                .last_mut()
                .expect("inserted conversation should exist")
        };
        if conversation
            .extra
            .get(LOCAL_READ_TS_KEY)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|last_read| message_ts <= last_read)
        {
            return Ok(false);
        }
        let mut seen = conversation
            .extra
            .get(SEEN_REALTIME_MESSAGE_TS_KEY)
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if seen.iter().any(|seen_ts| seen_ts == message_ts) {
            return Ok(false);
        }
        let count = conversation.unread_activity_count().saturating_add(1);
        conversation.apply_unread_state(SlackUnreadState::from_parts(true, true, count));
        seen.push(message_ts.to_string());
        if seen.len() > MAX_SEEN_REALTIME_MESSAGES {
            seen.drain(..seen.len() - MAX_SEEN_REALTIME_MESSAGES);
        }
        conversation.extra.insert(
            SEEN_REALTIME_MESSAGE_TS_KEY.to_string(),
            serde_json::Value::Array(seen.into_iter().map(serde_json::Value::String).collect()),
        );
        self.store_state(&state).await?;
        Ok(true)
    }

    /// Removes one cached conversation without disturbing other catalog data.
    #[allow(dead_code)]
    pub async fn remove_conversation(&self, channel_id: &str) -> Result<bool> {
        if channel_id.trim().is_empty() {
            return Ok(false);
        }

        let _guard = self.update_lock.lock().await;
        let mut state = self.load_state_for_update().await;
        let previous_len = state.conversations.len();
        state
            .conversations
            .retain(|conversation| conversation.id != channel_id);
        if state.conversations.len() == previous_len {
            return Ok(false);
        }
        self.store_state(&state).await?;
        Ok(true)
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
        self.update_state(|state| {
            state.user_names.extend(
                user_names
                    .iter()
                    .filter(|(user_id, display_name)| {
                        !user_id.trim().is_empty() && !display_name.trim().is_empty()
                    })
                    .map(|(user_id, display_name)| (user_id.clone(), display_name.clone())),
            );
        })
        .await
    }

    pub async fn load_custom_emojis(&self) -> Result<HashMap<String, String>> {
        Ok(self
            .load_state()
            .await?
            .map(|state| state.custom_emojis)
            .unwrap_or_default())
    }

    pub async fn store_custom_emojis(&self, emojis: &HashMap<String, String>) -> Result<()> {
        self.update_state(|state| state.custom_emojis = emojis.clone())
            .await
    }

    pub async fn load_history(&self, channel_id: &str) -> Result<Option<Vec<SlackMessage>>> {
        Ok(self
            .load_state()
            .await?
            .and_then(|state| state.channel_histories.get(channel_id).cloned())
            .filter(|messages| !messages.is_empty()))
    }

    pub async fn store_history(&self, channel_id: &str, messages: &[SlackMessage]) -> Result<()> {
        self.update_state(|state| {
            let existing = state
                .channel_histories
                .get(channel_id)
                .cloned()
                .unwrap_or_default();
            state.channel_histories.insert(
                channel_id.to_string(),
                merge_history_pages(&existing, messages),
            );
        })
        .await
    }

    pub async fn store_merged_history(
        &self,
        channel_id: &str,
        messages: &[SlackMessage],
    ) -> Result<()> {
        self.update_state(|state| {
            let existing = state
                .channel_histories
                .get(channel_id)
                .cloned()
                .unwrap_or_default();
            state.channel_histories.insert(
                channel_id.to_string(),
                merge_history_pages(&existing, messages),
            );
        })
        .await
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
        self.update_state(|state| {
            state
                .thread_replies
                .insert(thread_key(channel_id, thread_ts), messages.to_vec());
        })
        .await
    }

    pub async fn store_merged_thread(
        &self,
        channel_id: &str,
        thread_ts: &str,
        messages: &[SlackMessage],
    ) -> Result<Vec<SlackMessage>> {
        let _guard = self.update_lock.lock().await;
        let mut state = self.load_state_for_update().await;
        let key = thread_key(channel_id, thread_ts);
        let existing = state.thread_replies.get(&key).cloned().unwrap_or_default();
        let merged = merge_history_pages(&existing, messages);
        state.thread_replies.insert(key, merged.clone());
        self.store_state(&state).await?;
        Ok(merged)
    }

    #[allow(dead_code)]
    pub async fn load_thread_catalog(&self) -> Result<Vec<ThreadRecord>> {
        Ok(self
            .load_state()
            .await?
            .map(|state| state.thread_catalog)
            .unwrap_or_default())
    }

    #[allow(dead_code)]
    pub async fn store_thread_catalog(&self, records: &[ThreadRecord]) -> Result<()> {
        self.update_state(|state| state.thread_catalog = records.to_vec())
            .await
    }

    pub async fn observe_thread_history(
        &self,
        channel_id: &str,
        messages: &[SlackMessage],
    ) -> Result<()> {
        self.update_state(|state| {
            let mut catalog =
                ThreadCatalog::from_records(std::mem::take(&mut state.thread_catalog));
            catalog.observe_history(channel_id, messages);
            state.thread_catalog = catalog.into_records();
        })
        .await
    }

    pub async fn observe_thread_page(
        &self,
        channel_id: &str,
        root_ts: &str,
        messages: &[SlackMessage],
        complete: bool,
    ) -> Result<()> {
        self.update_state(|state| {
            let mut catalog =
                ThreadCatalog::from_records(std::mem::take(&mut state.thread_catalog));
            catalog.observe_thread(channel_id, root_ts, messages, complete);
            state.thread_catalog = catalog.into_records();
        })
        .await
    }

    pub async fn observe_thread_realtime(
        &self,
        channel_id: &str,
        message: &SlackMessage,
        current_user_id: Option<&str>,
    ) -> Result<()> {
        self.update_state(|state| {
            let mut catalog =
                ThreadCatalog::from_records(std::mem::take(&mut state.thread_catalog));
            catalog.observe_realtime(channel_id, message, current_user_id);
            state.thread_catalog = catalog.into_records();
        })
        .await
    }

    pub async fn mark_thread_read(
        &self,
        channel_id: &str,
        root_ts: &str,
        last_read: &str,
    ) -> Result<()> {
        self.update_state(|state| {
            let mut catalog =
                ThreadCatalog::from_records(std::mem::take(&mut state.thread_catalog));
            catalog.mark_read(channel_id, root_ts, last_read);
            state.thread_catalog = catalog.into_records();
        })
        .await
    }

    async fn update_state(&self, update: impl FnOnce(&mut CachedWorkspaceState)) -> Result<()> {
        let _guard = self.update_lock.lock().await;
        let mut state = self.load_state_for_update().await;
        update(&mut state);
        self.store_state(&state).await
    }

    async fn update_conversation(
        &self,
        channel_id: &str,
        update: impl FnOnce(&mut SlackConversation),
    ) -> Result<bool> {
        let _guard = self.update_lock.lock().await;
        let mut state = self.load_state_for_update().await;
        let Some(conversation) = state
            .conversations
            .iter_mut()
            .find(|conversation| conversation.id == channel_id)
        else {
            return Ok(false);
        };
        update(conversation);
        self.store_state(&state).await?;
        Ok(true)
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
    #[serde(default)]
    thread_catalog: Vec<ThreadRecord>,
    #[serde(default)]
    pending_unread_refresh: Vec<String>,
    #[serde(default)]
    custom_emojis: HashMap<String, String>,
}

impl CachedWorkspaceState {
    fn new() -> Self {
        Self {
            version: CACHE_VERSION,
            conversations: Vec::new(),
            user_names: HashMap::new(),
            channel_histories: HashMap::new(),
            thread_replies: HashMap::new(),
            thread_catalog: Vec::new(),
            pending_unread_refresh: Vec::new(),
            custom_emojis: HashMap::new(),
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
    // Incoming API/realtime data wins for duplicate timestamps while cached
    // messages missing from a bounded or in-flight page remain available.
    let mut messages = page.to_vec();
    messages.extend(existing.iter().cloned());
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

            let emojis = HashMap::from([
                (
                    "party_parrot".to_string(),
                    "https://emoji.example/parrot.gif".to_string(),
                ),
                ("ship_it".to_string(), "alias:rocket".to_string()),
            ]);
            store
                .store_custom_emojis(&emojis)
                .await
                .expect("emoji store failed");
            assert_eq!(
                store.load_custom_emojis().await.expect("emoji load failed"),
                emojis
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_updates_one_conversation_without_replacing_others() {
        let directory = temp_cache_dir("workspace-store-conversation-update");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[
                    SlackConversation {
                        id: "C1".to_string(),
                        name: Some("general".to_string()),
                        ..Default::default()
                    },
                    SlackConversation {
                        id: "C2".to_string(),
                        name: Some("random".to_string()),
                        ..Default::default()
                    },
                ])
                .await
                .expect("conversation store failed");

            store
                .store_conversation(&SlackConversation {
                    id: "C1".to_string(),
                    name: Some("renamed".to_string()),
                    ..Default::default()
                })
                .await
                .expect("conversation update failed");
            store
                .store_conversation(&SlackConversation {
                    id: "C3".to_string(),
                    name: Some("new".to_string()),
                    ..Default::default()
                })
                .await
                .expect("conversation insert failed");

            let conversations = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert_eq!(conversations.len(), 3);
            assert_eq!(
                conversations
                    .iter()
                    .find(|conversation| conversation.id == "C1")
                    .and_then(|conversation| conversation.name.as_deref()),
                Some("renamed")
            );
            assert!(conversations
                .iter()
                .any(|conversation| conversation.id == "C2"));
            assert!(conversations
                .iter()
                .any(|conversation| conversation.id == "C3"));
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn conversation_metadata_updates_preserve_local_read_overlay() {
        let directory = temp_cache_dir("workspace-store-conversation-metadata-overlay");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".to_string(),
                    name: Some("old".to_string()),
                    unread_count: Some(3),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store
                .clear_conversation_unread_state("C1", "20.0")
                .await
                .unwrap();

            let stale = SlackConversation {
                id: "C1".to_string(),
                name: Some("renamed".to_string()),
                unread_count: Some(8),
                ..Default::default()
            };
            store.store_conversation(&stale).await.unwrap();
            store.merge_conversation(&stale).await.unwrap();

            let conversations = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(conversations[0].name.as_deref(), Some("renamed"));
            assert_eq!(conversations[0].unread_activity_count(), 0);
            assert_eq!(
                conversations[0]
                    .extra
                    .get(LOCAL_READ_TS_KEY)
                    .and_then(serde_json::Value::as_str),
                Some("20.0")
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_merges_sparse_enrichment_without_losing_unread_state() {
        let directory = temp_cache_dir("workspace-store-conversation-merge");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "G1".to_string(),
                    is_mpim: Some(true),
                    unread_count: Some(4),
                    ..Default::default()
                }])
                .await
                .expect("conversation store failed");
            let mut enrichment = SlackConversation {
                id: "G1".to_string(),
                is_mpim: Some(true),
                ..Default::default()
            };
            enrichment
                .extra
                .insert("members".to_string(), serde_json::json!(["U1", "U2"]));
            store
                .merge_conversation(&enrichment)
                .await
                .expect("conversation merge failed");

            let conversations = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert_eq!(conversations[0].unread_activity_count(), 4);
            assert_eq!(
                conversations[0].extra.get("members"),
                Some(&serde_json::json!(["U1", "U2"]))
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_patches_and_clears_conversation_unread_state() {
        let directory = temp_cache_dir("workspace-store-conversation-unread");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".to_string(),
                    name: Some("general".to_string()),
                    ..Default::default()
                }])
                .await
                .expect("conversation store failed");

            assert!(store
                .apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(true, true, 7),
                    None
                )
                .await
                .expect("unread update failed"));
            let unread = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert!(unread[0].has_unread_activity());
            assert_eq!(unread[0].unread_activity_count(), 7);

            assert!(store
                .clear_conversation_unread_state("C1", "2.0")
                .await
                .expect("unread clear failed"));
            let cleared = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert!(!cleared[0].has_unread_activity());
            assert_eq!(cleared[0].unread_activity_count(), 0);

            assert!(!store
                .apply_conversation_unread_state(
                    "missing",
                    SlackUnreadState::from_parts(true, true, 1),
                    None,
                )
                .await
                .expect("missing unread update failed"));
            assert!(!store
                .apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(false, true, 1),
                    None,
                )
                .await
                .expect("unknown unread update failed"));
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn realtime_conversation_unread_events_are_idempotent_and_upsert_unknown_ids() {
        let directory = temp_cache_dir("workspace-store-realtime-unread");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            assert!(store
                .mark_conversation_unread_from_event("D1", "1710000001.000001")
                .await
                .expect("first realtime update failed"));
            assert!(!store
                .mark_conversation_unread_from_event("D1", "1710000001.000001")
                .await
                .expect("duplicate realtime update failed"));
            assert!(store
                .mark_conversation_unread_from_event("D1", "1710000002.000001")
                .await
                .expect("second realtime update failed"));

            let conversations = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert_eq!(conversations.len(), 1);
            assert_eq!(conversations[0].id, "D1");
            assert_eq!(conversations[0].unread_activity_count(), 2);
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn local_read_marker_rejects_older_server_and_realtime_updates() {
        let directory = temp_cache_dir("workspace-store-read-ordering");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".to_string(),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store
                .clear_conversation_unread_state("C1", "20.0")
                .await
                .unwrap();
            assert!(!store
                .apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(true, true, 4),
                    Some("10.0"),
                )
                .await
                .unwrap());
            assert!(!store
                .mark_conversation_unread_from_event("C1", "19.0")
                .await
                .unwrap());
            assert!(store
                .mark_conversation_unread_from_event("C1", "21.0")
                .await
                .unwrap());
            let conversations = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(conversations[0].unread_activity_count(), 1);
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn atomic_membership_reconciliation_preserves_unread_overlay_and_pending_work() {
        let directory = temp_cache_dir("workspace-store-atomic-membership");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".to_string(),
                    name: Some("old".to_string()),
                    unread_count: Some(5),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store
                .store_pending_unread_refresh(&["C1".to_string(), "D2".to_string()])
                .await
                .unwrap();
            let committed = store
                .reconcile_conversations(vec![SlackConversation {
                    id: "C1".to_string(),
                    name: Some("renamed".to_string()),
                    ..Default::default()
                }])
                .await
                .unwrap();
            assert_eq!(committed[0].name.as_deref(), Some("renamed"));
            assert_eq!(committed[0].unread_activity_count(), 5);
            assert_eq!(
                store.load_pending_unread_refresh().await.unwrap(),
                vec!["C1".to_string(), "D2".to_string()]
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_serializes_individual_conversation_updates_across_clones() {
        let directory = temp_cache_dir("workspace-store-conversation-concurrent");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let cloned_store = store.clone();
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".to_string(),
                    ..Default::default()
                }])
                .await
                .expect("conversation store failed");

            let (unread_result, insert_result) = futures_util::future::join(
                store.apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(true, true, 3),
                    None,
                ),
                cloned_store.store_conversation(&SlackConversation {
                    id: "C2".to_string(),
                    ..Default::default()
                }),
            )
            .await;
            assert!(unread_result.expect("unread update failed"));
            insert_result.expect("conversation insert failed");

            let conversations = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert_eq!(conversations.len(), 2);
            assert_eq!(
                conversations
                    .iter()
                    .find(|conversation| conversation.id == "C1")
                    .map(SlackConversation::unread_activity_count),
                Some(3)
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_removes_one_conversation() {
        let directory = temp_cache_dir("workspace-store-conversation-remove");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[
                    SlackConversation {
                        id: "C1".to_string(),
                        ..Default::default()
                    },
                    SlackConversation {
                        id: "C2".to_string(),
                        ..Default::default()
                    },
                ])
                .await
                .expect("conversation store failed");

            assert!(store
                .remove_conversation("C1")
                .await
                .expect("conversation removal failed"));
            assert!(!store
                .remove_conversation("C1")
                .await
                .expect("duplicate conversation removal failed"));
            let conversations = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations");
            assert_eq!(conversations.len(), 1);
            assert_eq!(conversations[0].id, "C2");
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
    fn workspace_store_serializes_concurrent_updates_from_clones() {
        let directory = temp_cache_dir("workspace-store-concurrent-updates");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let cloned_store = store.clone();
        let runtime = runtime();

        runtime.block_on(async {
            let conversations = vec![SlackConversation {
                id: "C123".to_string(),
                name: Some("general".to_string()),
                ..Default::default()
            }];
            let messages = vec![SlackMessage {
                ts: "1710000000.000100".to_string(),
                text: Some("cached".to_string()),
                ..Default::default()
            }];

            let (conversations_result, history_result) = futures_util::future::join(
                store.store_conversations(&conversations),
                cloned_store.store_history("C123", &messages),
            )
            .await;
            conversations_result.expect("conversation store failed");
            history_result.expect("history store failed");

            assert_eq!(
                store
                    .load_conversations()
                    .await
                    .expect("conversation load failed")
                    .expect("concurrent conversation update was lost")[0]
                    .id,
                "C123"
            );
            assert_eq!(
                store
                    .load_history("C123")
                    .await
                    .expect("history load failed")
                    .expect("concurrent history update was lost")[0]
                    .body_text(),
                "cached"
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
            assert_eq!(
                messages
                    .iter()
                    .find(|message| message.ts == "1710000200.000000")
                    .and_then(|message| message.text.as_deref()),
                Some("duplicate")
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn stale_history_page_does_not_remove_newer_realtime_message() {
        let directory = temp_cache_dir("workspace-store-realtime-history-race");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_merged_history(
                    "D1",
                    &[SlackMessage {
                        ts: "5.0".to_string(),
                        text: Some("realtime".to_string()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            store
                .store_history(
                    "D1",
                    &[SlackMessage {
                        ts: "4.0".to_string(),
                        text: Some("stale page".to_string()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();

            let messages = store.load_history("D1").await.unwrap().unwrap();
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["5.0", "4.0"]
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

    #[test]
    fn workspace_store_round_trips_thread_catalog() {
        use crate::thread_catalog::ThreadCatalog;

        let directory = temp_cache_dir("workspace-store-thread-catalog");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let mut catalog = ThreadCatalog::default();
            let root = SlackMessage {
                ts: "1710000000.000100".into(),
                reply_count: Some(3),
                subscribed: Some(true),
                unread_count: Some(2),
                last_read: Some("1710000100.000100".into()),
                latest_reply: Some("1710000300.000100".into()),
                ..Default::default()
            };
            catalog.observe_thread("C123", &root.ts.clone(), &[root], false);
            let records = catalog.into_records();
            store
                .store_thread_catalog(&records)
                .await
                .expect("thread catalog store failed");

            assert_eq!(
                store
                    .load_thread_catalog()
                    .await
                    .expect("thread catalog load failed"),
                records
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }
}
