use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::lock::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::conversation_catalog::ConversationCatalog;
use crate::models::{SlackConversation, SlackMessage, SlackUnreadState, SlackUserStatus};
use crate::thread_catalog::{ThreadCatalog, ThreadRecord};

pub(crate) const CACHE_VERSION: u32 = 1;
const DATABASE_SCHEMA_VERSION: u32 = 2;
const DATABASE_FILENAME: &str = "state.sqlite3";
const MAX_CACHED_CHANNEL_MESSAGES: usize = 200;
const SEEN_REALTIME_MESSAGE_TS_KEY: &str = "conduit_seen_realtime_message_ts";
const LOCAL_READ_TS_KEY: &str = "conduit_local_read_ts";
const MAX_SEEN_REALTIME_MESSAGES: usize = 256;

pub(crate) type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoreErrorCategory {
    LocalIo,
    TemporarilyUnavailable,
    CorruptData,
    IncompatibleSchema,
    RejectedUpdate,
    Unexpected,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    #[error("{message}")]
    RejectedUpdate { message: String },
    #[error("workspace database schema {found} is newer than supported schema {supported}")]
    IncompatibleSchema { found: u32, supported: u32 },
    #[error("derived workspace cache is invalid: {message}")]
    InvalidDerivedCache { message: String },
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl StoreError {
    fn rejected_update(message: impl Into<String>) -> Self {
        Self::RejectedUpdate {
            message: message.into(),
        }
    }

    fn incompatible_schema(found: u32, supported: u32) -> Self {
        Self::IncompatibleSchema { found, supported }
    }

    fn invalid_derived_cache(message: impl Into<String>) -> Self {
        Self::InvalidDerivedCache {
            message: message.into(),
        }
    }

    pub(crate) fn category(&self) -> StoreErrorCategory {
        match self {
            Self::RejectedUpdate { .. } => StoreErrorCategory::RejectedUpdate,
            Self::IncompatibleSchema { .. } => StoreErrorCategory::IncompatibleSchema,
            Self::InvalidDerivedCache { .. } => StoreErrorCategory::CorruptData,
            Self::Database(error) => classify_database_error(error),
            Self::Io(_) => StoreErrorCategory::LocalIo,
            Self::Json(_) => StoreErrorCategory::CorruptData,
            Self::Other(error) => classify_wrapped_store_error(error),
        }
    }
}

fn classify_database_error(error: &rusqlite::Error) -> StoreErrorCategory {
    let rusqlite::Error::SqliteFailure(details, _) = error else {
        return StoreErrorCategory::Unexpected;
    };
    match details.code {
        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
            StoreErrorCategory::TemporarilyUnavailable
        }
        rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
            StoreErrorCategory::CorruptData
        }
        rusqlite::ErrorCode::CannotOpen
        | rusqlite::ErrorCode::DiskFull
        | rusqlite::ErrorCode::PermissionDenied
        | rusqlite::ErrorCode::ReadOnly
        | rusqlite::ErrorCode::SystemIoFailure => StoreErrorCategory::LocalIo,
        _ => StoreErrorCategory::Unexpected,
    }
}

fn classify_wrapped_store_error(error: &anyhow::Error) -> StoreErrorCategory {
    for source in error.chain() {
        if let Some(database) = source.downcast_ref::<rusqlite::Error>() {
            return classify_database_error(database);
        }
        if source.downcast_ref::<std::io::Error>().is_some() {
            return StoreErrorCategory::LocalIo;
        }
        if source.downcast_ref::<serde_json::Error>().is_some() {
            return StoreErrorCategory::CorruptData;
        }
    }
    StoreErrorCategory::Unexpected
}

#[derive(Clone, Debug)]
pub struct WorkspaceStore {
    directory: PathBuf,
    workspace_id: String,
    workspace_key: String,
    update_lock: Arc<Mutex<()>>,
}

enum ConversationRowMutation<R> {
    Unchanged(R),
    Upsert(SlackConversation, R),
    Delete(R),
}

impl WorkspaceStore {
    pub fn new(directory: PathBuf, workspace_id: &str) -> Self {
        Self {
            directory,
            workspace_id: workspace_id.to_string(),
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

    /// Records the opaque workspace identity needed by desktop integrations,
    /// including when an older cache is opened while offline.
    pub async fn ensure_workspace_identity(&self) -> Result<()> {
        let _guard = self.update_lock.lock().await;
        let state = self.load_state_for_update().await?;
        self.store_state_with_activation(&state, true).await
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
        let mut state = self.load_state_for_update().await?;
        if fresh.is_empty() && !state.conversations.is_empty() {
            return Err(StoreError::rejected_update(
                "Slack returned an unexpectedly empty conversation membership snapshot",
            ));
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

        let incoming = conversation.clone();
        self.mutate_conversation_row(&conversation.id, move |existing| {
            let mut catalog = ConversationCatalog::from_cached(existing);
            catalog.upsert_metadata(incoming);
            let conversation = catalog
                .conversations()
                .into_iter()
                .next()
                .expect("metadata upsert should produce a conversation");
            ConversationRowMutation::Upsert(conversation, ())
        })
        .await
    }

    pub async fn merge_conversation(&self, conversation: &SlackConversation) -> Result<()> {
        self.store_conversation(conversation).await
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

        let server_last_read = server_last_read.map(str::to_string);
        self.mutate_conversation_row(channel_id, move |conversation| {
            let Some(mut conversation) = conversation else {
                return ConversationRowMutation::Unchanged(false);
            };
            let newer_local_read = conversation
                .extra
                .get(LOCAL_READ_TS_KEY)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|local| {
                    server_last_read
                        .as_deref()
                        .is_none_or(|server| local > server)
                });
            if newer_local_read {
                return ConversationRowMutation::Unchanged(false);
            }
            conversation.apply_unread_state(unread_state);
            ConversationRowMutation::Upsert(conversation, true)
        })
        .await
    }

    /// Advances one cached conversation's read cursor without assuming that
    /// messages newer than the supplied cursor have been read.
    pub async fn advance_conversation_read_cursor(
        &self,
        channel_id: &str,
        last_read: &str,
    ) -> Result<bool> {
        if channel_id.trim().is_empty() {
            return Ok(false);
        }

        let last_read = last_read.to_string();
        self.update_conversation(channel_id, move |conversation| {
            let reached_latest = conversation
                .latest_message_ts()
                .is_none_or(|latest| latest <= last_read.as_str());
            if reached_latest {
                conversation.clear_unread_activity();
            }
            conversation.extra.insert(
                "last_read".to_string(),
                serde_json::Value::String(last_read.clone()),
            );
            conversation.extra.insert(
                LOCAL_READ_TS_KEY.to_string(),
                serde_json::Value::String(last_read),
            );
        })
        .await
    }

    pub async fn clear_conversation_unread_state(
        &self,
        channel_id: &str,
        last_read: &str,
    ) -> Result<bool> {
        self.advance_conversation_read_cursor(channel_id, last_read)
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

        let channel_id = channel_id.to_string();
        let inserted_channel_id = channel_id.clone();
        let message_ts = message_ts.to_string();
        self.mutate_conversation_row(&channel_id, move |conversation| {
            let mut conversation = conversation.unwrap_or_else(|| SlackConversation {
                id: inserted_channel_id,
                ..Default::default()
            });
            if conversation
                .extra
                .get(LOCAL_READ_TS_KEY)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|last_read| message_ts.as_str() <= last_read)
            {
                return ConversationRowMutation::Unchanged(false);
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
            if seen.iter().any(|seen_ts| seen_ts == &message_ts) {
                return ConversationRowMutation::Unchanged(false);
            }
            let count = conversation.unread_activity_count().saturating_add(1);
            conversation.apply_unread_state(SlackUnreadState::from_parts(true, true, count));
            seen.push(message_ts);
            if seen.len() > MAX_SEEN_REALTIME_MESSAGES {
                seen.drain(..seen.len() - MAX_SEEN_REALTIME_MESSAGES);
            }
            conversation.extra.insert(
                SEEN_REALTIME_MESSAGE_TS_KEY.to_string(),
                serde_json::Value::Array(seen.into_iter().map(serde_json::Value::String).collect()),
            );
            ConversationRowMutation::Upsert(conversation, true)
        })
        .await
    }

    /// Removes one cached conversation without disturbing other catalog data.
    #[allow(dead_code)]
    pub async fn remove_conversation(&self, channel_id: &str) -> Result<bool> {
        if channel_id.trim().is_empty() {
            return Ok(false);
        }

        self.mutate_conversation_row(channel_id, |conversation| {
            if conversation.is_some() {
                ConversationRowMutation::Delete(true)
            } else {
                ConversationRowMutation::Unchanged(false)
            }
        })
        .await
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

    pub async fn load_user_full_names(&self) -> Result<HashMap<String, String>> {
        Ok(self
            .load_state()
            .await?
            .map(|state| state.user_full_names)
            .unwrap_or_default())
    }

    pub async fn store_user_full_names(
        &self,
        user_full_names: &HashMap<String, String>,
    ) -> Result<()> {
        self.update_state(|state| {
            state.user_full_names.extend(
                user_full_names
                    .iter()
                    .filter(|(user_id, full_name)| {
                        !user_id.trim().is_empty() && !full_name.trim().is_empty()
                    })
                    .map(|(user_id, full_name)| (user_id.clone(), full_name.clone())),
            );
        })
        .await
    }

    pub async fn load_user_avatar_urls(&self) -> Result<HashMap<String, String>> {
        Ok(self
            .load_state()
            .await?
            .map(|state| state.user_avatar_urls)
            .unwrap_or_default())
    }

    pub async fn store_user_avatar_urls(
        &self,
        avatar_urls: &HashMap<String, String>,
    ) -> Result<()> {
        self.update_state(|state| {
            state.user_avatar_urls.extend(
                avatar_urls
                    .iter()
                    .filter(|(user_id, url)| !user_id.trim().is_empty() && !url.trim().is_empty())
                    .map(|(user_id, url)| (user_id.clone(), url.clone())),
            );
        })
        .await
    }

    pub async fn load_user_search_aliases(&self) -> Result<HashMap<String, Vec<String>>> {
        Ok(self
            .load_state()
            .await?
            .map(|state| state.user_search_aliases)
            .unwrap_or_default())
    }

    pub async fn store_user_search_aliases(
        &self,
        aliases: &HashMap<String, Vec<String>>,
    ) -> Result<()> {
        self.update_state(|state| {
            state.user_search_aliases = aliases
                .iter()
                .filter(|(user_id, aliases)| {
                    !user_id.trim().is_empty()
                        && aliases.iter().any(|alias| !alias.trim().is_empty())
                })
                .map(|(user_id, aliases)| (user_id.clone(), aliases.clone()))
                .collect();
        })
        .await
    }

    pub async fn load_user_statuses(&self) -> Result<HashMap<String, SlackUserStatus>> {
        Ok(self
            .load_state()
            .await?
            .map(|state| state.user_statuses)
            .unwrap_or_default())
    }

    pub async fn store_user_statuses(
        &self,
        statuses: &HashMap<String, SlackUserStatus>,
    ) -> Result<()> {
        self.update_state(|state| state.user_statuses = statuses.clone())
            .await
    }

    pub async fn store_user_status(
        &self,
        user_id: &str,
        status: Option<SlackUserStatus>,
    ) -> Result<()> {
        self.update_state(|state| match status {
            Some(status) => {
                state.user_statuses.insert(user_id.to_string(), status);
            }
            None => {
                state.user_statuses.remove(user_id);
            }
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
            .map(channel_timeline_messages)
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
                merge_channel_history_pages(&existing, messages),
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
                merge_channel_history_pages(&existing, messages),
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
            let key = thread_key(channel_id, thread_ts);
            let existing = state.thread_replies.get(&key).cloned().unwrap_or_default();
            state
                .thread_replies
                .insert(key, merge_history_pages(&existing, messages));
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
        let mut state = self.load_state_for_update().await?;
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
        let mut state = self.load_state_for_update().await?;
        state.workspace_id = self.workspace_id.clone();
        update(&mut state);
        self.store_state(&state).await
    }

    async fn update_conversation(
        &self,
        channel_id: &str,
        update: impl FnOnce(&mut SlackConversation) + Send + 'static,
    ) -> Result<bool> {
        self.mutate_conversation_row(channel_id, move |conversation| {
            let Some(mut conversation) = conversation else {
                return ConversationRowMutation::Unchanged(false);
            };
            update(&mut conversation);
            ConversationRowMutation::Upsert(conversation, true)
        })
        .await
    }

    async fn mutate_conversation_row<R, F>(&self, channel_id: &str, update: F) -> Result<R>
    where
        R: Send + 'static,
        F: FnOnce(Option<SlackConversation>) -> ConversationRowMutation<R> + Send + 'static,
    {
        // Startup and realtime sync can apply thousands of isolated conversation patches.
        // Keep those mutations row-scoped instead of rebuilding every cached workspace item.
        let _guard = self.update_lock.lock().await;
        let directory = self.directory.clone();
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let channel_id = channel_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_database(&directory)?;
            migrate_legacy_workspace(&mut connection, &directory, &workspace_key, &workspace_id)?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing = load_sqlite_conversation(&transaction, &workspace_key, &channel_id)?;
            match update(existing) {
                ConversationRowMutation::Unchanged(result) => {
                    transaction.rollback()?;
                    Ok(result)
                }
                ConversationRowMutation::Upsert(conversation, result) => {
                    upsert_sqlite_conversation(
                        &transaction,
                        &workspace_key,
                        &workspace_id,
                        &conversation,
                    )?;
                    transaction.commit()?;
                    Ok(result)
                }
                ConversationRowMutation::Delete(result) => {
                    transaction.execute(
                        "DELETE FROM workspace_items
                         WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = ?2",
                        params![workspace_key, channel_id],
                    )?;
                    transaction.commit()?;
                    Ok(result)
                }
            }
        })
        .await
        .context("workspace conversation cache writer stopped unexpectedly")?
    }

    async fn load_state(&self) -> Result<Option<CachedWorkspaceState>> {
        let directory = self.directory.clone();
        let workspace_key = self.workspace_key.clone();
        let workspace_id = self.workspace_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut connection = open_database(&directory)?;
            migrate_legacy_workspace(&mut connection, &directory, &workspace_key, &workspace_id)?;
            match load_sqlite_state(&connection, &workspace_key) {
                Err(error) if error.category() == StoreErrorCategory::CorruptData => {
                    drop(connection);
                    recreate_derived_cache(&directory)?;
                    let _ = open_database(&directory)?;
                    Ok(None)
                }
                result => result,
            }
        })
        .await
        .context("workspace cache reader stopped unexpectedly")?;
        if let Err(error) = &result {
            crate::debug::log(
                "store",
                &format!("WorkspaceCacheReadFailed category={:?}", error.category()),
            );
        }
        result
    }

    async fn load_state_for_update(&self) -> Result<CachedWorkspaceState> {
        let mut state = self
            .load_state()
            .await?
            .unwrap_or_else(CachedWorkspaceState::new);
        state.workspace_id = self.workspace_id.clone();
        Ok(state)
    }

    async fn store_state(&self, state: &CachedWorkspaceState) -> Result<()> {
        self.store_state_with_activation(state, false).await
    }

    async fn store_state_with_activation(
        &self,
        state: &CachedWorkspaceState,
        activate: bool,
    ) -> Result<()> {
        let directory = self.directory.clone();
        let workspace_key = self.workspace_key.clone();
        let state = state.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = open_database(&directory)?;
            store_sqlite_state(&mut connection, &workspace_key, &state, activate)
        })
        .await
        .context("workspace cache writer stopped unexpectedly")?
    }

    #[cfg(test)]
    fn path(&self) -> PathBuf {
        self.directory.join(format!("{}.json", self.workspace_key))
    }

    #[cfg(test)]
    fn database_path(&self) -> PathBuf {
        database_path(&self.directory)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedWorkspaceState {
    version: u32,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    conversations: Vec<SlackConversation>,
    #[serde(default)]
    user_names: HashMap<String, String>,
    #[serde(default)]
    user_full_names: HashMap<String, String>,
    #[serde(default)]
    user_avatar_urls: HashMap<String, String>,
    #[serde(default)]
    user_search_aliases: HashMap<String, Vec<String>>,
    #[serde(default)]
    user_statuses: HashMap<String, SlackUserStatus>,
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
            workspace_id: String::new(),
            conversations: Vec::new(),
            user_names: HashMap::new(),
            user_full_names: HashMap::new(),
            user_avatar_urls: HashMap::new(),
            user_search_aliases: HashMap::new(),
            user_statuses: HashMap::new(),
            channel_histories: HashMap::new(),
            thread_replies: HashMap::new(),
            thread_catalog: Vec::new(),
            pending_unread_refresh: Vec::new(),
            custom_emojis: HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct SearchProviderState {
    pub(crate) workspace_id: String,
    pub(crate) conversations: Vec<SlackConversation>,
    pub(crate) user_names: HashMap<String, String>,
    pub(crate) user_full_names: HashMap<String, String>,
    pub(crate) user_search_aliases: HashMap<String, Vec<String>>,
}

pub(crate) fn load_active_search_state(directory: &Path) -> Result<Option<SearchProviderState>> {
    let mut connection = open_database(directory)?;
    migrate_legacy_active_workspace(&mut connection, directory)?;
    let workspace_key = connection
        .query_row(
            "SELECT active_workspace_key FROM app_state WHERE singleton = 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let Some(workspace_key) = workspace_key else {
        return Ok(None);
    };
    match load_sqlite_search_state(&connection, &workspace_key) {
        Err(error) if error.category() == StoreErrorCategory::CorruptData => {
            drop(connection);
            recreate_derived_cache(directory)?;
            let _ = open_database(directory)?;
            Ok(None)
        }
        result => result,
    }
}

pub(crate) fn clear_active_workspace(directory: &Path) -> Result<()> {
    if !database_path(directory).exists() {
        let _ = std::fs::remove_file(directory.join("active-workspace"));
        return Ok(());
    }
    let connection = open_database(directory)?;
    connection.execute(
        "UPDATE app_state SET active_workspace_key = NULL WHERE singleton = 1",
        [],
    )?;
    let _ = std::fs::remove_file(directory.join("active-workspace"));
    Ok(())
}

fn database_path(directory: &Path) -> PathBuf {
    directory.join(DATABASE_FILENAME)
}

fn open_database(directory: &Path) -> Result<Connection> {
    std::fs::create_dir_all(directory).with_context(|| {
        format!(
            "failed to create state cache directory {}",
            directory.display()
        )
    })?;
    match open_database_once(directory) {
        Err(error) if error.category() == StoreErrorCategory::CorruptData => {
            recreate_derived_cache(directory)?;
            open_database_once(directory)
        }
        result => result,
    }
}

fn open_database_once(directory: &Path) -> Result<Connection> {
    let connection = Connection::open(database_path(directory)).with_context(|| {
        format!(
            "failed to open workspace database in {}",
            directory.display()
        )
    })?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let schema_version =
        connection.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
    if schema_version > DATABASE_SCHEMA_VERSION {
        return Err(StoreError::incompatible_schema(
            schema_version,
            DATABASE_SCHEMA_VERSION,
        ));
    }
    if let Err(error) = connection.execute_batch(&format!(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS workspaces (
             workspace_key TEXT PRIMARY KEY,
             workspace_id TEXT NOT NULL
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS app_state (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             active_workspace_key TEXT REFERENCES workspaces(workspace_key)
         );
         INSERT OR IGNORE INTO app_state(singleton, active_workspace_key) VALUES (1, NULL);
         CREATE TABLE IF NOT EXISTS workspace_items (
             workspace_key TEXT NOT NULL REFERENCES workspaces(workspace_key) ON DELETE CASCADE,
             kind TEXT NOT NULL,
             item_key TEXT NOT NULL,
             payload_json TEXT NOT NULL,
             PRIMARY KEY (workspace_key, kind, item_key)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS sync_metadata (
             workspace_key TEXT NOT NULL REFERENCES workspaces(workspace_key) ON DELETE CASCADE,
             operation TEXT NOT NULL,
             target TEXT NOT NULL,
             refreshed_at_ms INTEGER,
             retry_count INTEGER NOT NULL DEFAULT 0,
             retry_after_ms INTEGER,
             PRIMARY KEY (workspace_key, operation, target)
         ) WITHOUT ROWID;
         PRAGMA user_version = {DATABASE_SCHEMA_VERSION};"
    )) {
        if schema_version < DATABASE_SCHEMA_VERSION {
            return Err(StoreError::invalid_derived_cache(format!(
                "schema migration from v{schema_version} failed: {error}"
            )));
        }
        return Err(error.into());
    }
    validate_schema_v2(&connection)?;
    Ok(connection)
}

fn validate_schema_v2(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(sync_metadata)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let expected = [
        "workspace_key",
        "operation",
        "target",
        "refreshed_at_ms",
        "retry_count",
        "retry_after_ms",
    ];
    if columns.iter().map(String::as_str).ne(expected) {
        return Err(StoreError::invalid_derived_cache(
            "schema-v2 sync metadata columns do not match",
        ));
    }
    Ok(())
}

fn recreate_derived_cache(directory: &Path) -> Result<()> {
    let database = database_path(directory);
    for path in [
        database.clone(),
        sqlite_sidecar_path(&database, "-wal"),
        sqlite_sidecar_path(&database, "-shm"),
    ] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn sqlite_sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn load_sqlite_state(
    connection: &Connection,
    workspace_key: &str,
) -> Result<Option<CachedWorkspaceState>> {
    let workspace_id = connection
        .query_row(
            "SELECT workspace_id FROM workspaces WHERE workspace_key = ?1",
            [workspace_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(workspace_id) = workspace_id else {
        return Ok(None);
    };

    let mut state = CachedWorkspaceState::new();
    state.workspace_id = workspace_id;
    let mut statement = connection.prepare(
        "SELECT kind, item_key, payload_json
         FROM workspace_items WHERE workspace_key = ?1 ORDER BY kind, item_key",
    )?;
    let rows = statement.query_map([workspace_key], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (kind, item_key, payload) = row?;
        match kind.as_str() {
            "conversation" => state
                .conversations
                .push(serde_json::from_str(&payload).context("invalid cached conversation")?),
            "user_name" => {
                state.user_names.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user name")?,
                );
            }
            "user_full_name" => {
                state.user_full_names.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user full name")?,
                );
            }
            "user_avatar_url" => {
                state.user_avatar_urls.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user avatar URL")?,
                );
            }
            "user_aliases" => {
                state.user_search_aliases.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user aliases")?,
                );
            }
            "user_status" => {
                state.user_statuses.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user status")?,
                );
            }
            "channel_history" => {
                state.channel_histories.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached channel history")?,
                );
            }
            "thread_replies" => {
                state.thread_replies.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached thread replies")?,
                );
            }
            "thread_record" => state
                .thread_catalog
                .push(serde_json::from_str(&payload).context("invalid cached thread record")?),
            "pending_unread" => state.pending_unread_refresh.push(item_key),
            "custom_emoji" => {
                state.custom_emojis.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached custom emoji")?,
                );
            }
            _ => {}
        }
    }
    Ok(Some(state))
}

fn load_sqlite_conversation(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    channel_id: &str,
) -> Result<Option<SlackConversation>> {
    let payload = transaction
        .query_row(
            "SELECT payload_json
             FROM workspace_items
             WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = ?2",
            params![workspace_key, channel_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let conversation = payload
        .map(|payload| {
            serde_json::from_str::<SlackConversation>(&payload)
                .context("invalid cached conversation")
        })
        .transpose()
        .map_err(StoreError::from)?;
    Ok(conversation.filter(|conversation| conversation.id == channel_id))
}

fn upsert_sqlite_conversation(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    workspace_id: &str,
    conversation: &SlackConversation,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO workspaces(workspace_key, workspace_id) VALUES (?1, ?2)
         ON CONFLICT(workspace_key) DO UPDATE SET workspace_id = excluded.workspace_id",
        params![workspace_key, workspace_id],
    )?;
    let conversation = conversation_for_cache(conversation);
    let payload = serde_json::to_string(&conversation)
        .context("failed to serialize cached workspace item")?;
    transaction.execute(
        "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
         VALUES (?1, 'conversation', ?2, ?3)
         ON CONFLICT(workspace_key, kind, item_key)
         DO UPDATE SET payload_json = excluded.payload_json",
        params![workspace_key, conversation.id, payload],
    )?;
    Ok(())
}

fn load_sqlite_search_state(
    connection: &Connection,
    workspace_key: &str,
) -> Result<Option<SearchProviderState>> {
    let workspace_id = connection
        .query_row(
            "SELECT workspace_id FROM workspaces WHERE workspace_key = ?1",
            [workspace_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(workspace_id) = workspace_id else {
        return Ok(None);
    };

    let mut state = SearchProviderState {
        workspace_id,
        conversations: Vec::new(),
        user_names: HashMap::new(),
        user_full_names: HashMap::new(),
        user_search_aliases: HashMap::new(),
    };
    let mut statement = connection.prepare(
        "SELECT kind, item_key, payload_json
         FROM workspace_items
         WHERE workspace_key = ?1
           AND kind IN ('conversation', 'user_name', 'user_full_name', 'user_aliases')
         ORDER BY kind, item_key",
    )?;
    let rows = statement.query_map([workspace_key], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (kind, item_key, payload) = row?;
        match kind.as_str() {
            "conversation" => state
                .conversations
                .push(serde_json::from_str(&payload).context("invalid cached conversation")?),
            "user_name" => {
                state.user_names.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user name")?,
                );
            }
            "user_full_name" => {
                state.user_full_names.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user full name")?,
                );
            }
            "user_aliases" => {
                state.user_search_aliases.insert(
                    item_key,
                    serde_json::from_str(&payload).context("invalid cached user aliases")?,
                );
            }
            _ => unreachable!("search-state query returned an unexpected item kind"),
        }
    }
    Ok(Some(state))
}

fn store_sqlite_state(
    connection: &mut Connection,
    workspace_key: &str,
    state: &CachedWorkspaceState,
    activate: bool,
) -> Result<()> {
    let desired = state_items(state)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "INSERT INTO workspaces(workspace_key, workspace_id) VALUES (?1, ?2)
         ON CONFLICT(workspace_key) DO UPDATE SET workspace_id = excluded.workspace_id",
        params![workspace_key, state.workspace_id],
    )?;
    sync_state_items(&transaction, workspace_key, desired)?;
    if activate {
        transaction.execute(
            "UPDATE app_state SET active_workspace_key = ?1 WHERE singleton = 1",
            [workspace_key],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn sync_state_items(
    transaction: &Transaction<'_>,
    workspace_key: &str,
    desired: HashMap<(String, String), String>,
) -> Result<()> {
    let mut current = HashMap::new();
    {
        let mut statement = transaction.prepare(
            "SELECT kind, item_key, payload_json FROM workspace_items WHERE workspace_key = ?1",
        )?;
        let rows = statement.query_map([workspace_key], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (key, payload) = row?;
            current.insert(key, payload);
        }
    }

    for ((kind, item_key), _) in current
        .iter()
        .filter(|(key, _)| !desired.contains_key(*key))
    {
        transaction.execute(
            "DELETE FROM workspace_items
             WHERE workspace_key = ?1 AND kind = ?2 AND item_key = ?3",
            params![workspace_key, kind, item_key],
        )?;
    }
    for ((kind, item_key), payload) in desired {
        if current.get(&(kind.clone(), item_key.clone())) == Some(&payload) {
            continue;
        }
        transaction.execute(
            "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(workspace_key, kind, item_key)
             DO UPDATE SET payload_json = excluded.payload_json",
            params![workspace_key, kind, item_key, payload],
        )?;
    }
    Ok(())
}

fn state_items(state: &CachedWorkspaceState) -> Result<HashMap<(String, String), String>> {
    let mut items = HashMap::new();
    for conversation in &state.conversations {
        let conversation = conversation_for_cache(conversation);
        insert_state_item(
            &mut items,
            "conversation",
            conversation.id.clone(),
            &conversation,
        )?;
    }
    for (key, value) in &state.user_names {
        insert_state_item(&mut items, "user_name", key.clone(), value)?;
    }
    for (key, value) in &state.user_full_names {
        insert_state_item(&mut items, "user_full_name", key.clone(), value)?;
    }
    for (key, value) in &state.user_avatar_urls {
        insert_state_item(&mut items, "user_avatar_url", key.clone(), value)?;
    }
    for (key, value) in &state.user_search_aliases {
        insert_state_item(&mut items, "user_aliases", key.clone(), value)?;
    }
    for (key, value) in &state.user_statuses {
        insert_state_item(&mut items, "user_status", key.clone(), value)?;
    }
    for (key, value) in &state.channel_histories {
        insert_state_item(&mut items, "channel_history", key.clone(), value)?;
    }
    for (key, value) in &state.thread_replies {
        insert_state_item(&mut items, "thread_replies", key.clone(), value)?;
    }
    for record in &state.thread_catalog {
        insert_state_item(
            &mut items,
            "thread_record",
            thread_key(&record.key.channel_id, &record.key.root_ts),
            record,
        )?;
    }
    for key in &state.pending_unread_refresh {
        insert_state_item(&mut items, "pending_unread", key.clone(), &())?;
    }
    for (key, value) in &state.custom_emojis {
        insert_state_item(&mut items, "custom_emoji", key.clone(), value)?;
    }
    Ok(items)
}

fn conversation_for_cache(conversation: &SlackConversation) -> SlackConversation {
    let mut cached = conversation.clone();
    let remove_empty_properties = cached
        .extra
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
        .is_some_and(|properties| {
            properties.remove("huddles");
            properties.is_empty()
        });
    if remove_empty_properties {
        cached.extra.remove("properties");
    }
    cached
}

fn insert_state_item<T: Serialize + ?Sized>(
    items: &mut HashMap<(String, String), String>,
    kind: &str,
    key: String,
    value: &T,
) -> Result<()> {
    items.insert(
        (kind.to_string(), key),
        serde_json::to_string(value).context("failed to serialize cached workspace item")?,
    );
    Ok(())
}

fn migrate_legacy_workspace(
    connection: &mut Connection,
    directory: &Path,
    workspace_key: &str,
    workspace_id: &str,
) -> Result<()> {
    let exists = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_key = ?1)",
        [workspace_key],
        |row| row.get::<_, bool>(0),
    )?;
    if exists {
        return Ok(());
    }
    let Some(mut state) = read_legacy_state(directory, workspace_key)? else {
        return Ok(());
    };
    state.workspace_id = workspace_id.to_string();
    store_sqlite_state(connection, workspace_key, &state, false)?;
    remove_legacy_workspace_files(directory, workspace_key);
    Ok(())
}

fn migrate_legacy_active_workspace(connection: &mut Connection, directory: &Path) -> Result<()> {
    let active = connection
        .query_row(
            "SELECT active_workspace_key FROM app_state WHERE singleton = 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    if active.is_some() {
        return Ok(());
    }

    let marked = std::fs::read_to_string(directory.join("active-workspace"))
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| is_workspace_key(key))
        .and_then(|key| {
            read_legacy_state(directory, &key)
                .ok()
                .flatten()
                .filter(|state| !state.workspace_id.trim().is_empty())
                .map(|state| (key, state))
        });
    let candidate = if let Some(marked) = marked {
        Some(marked)
    } else {
        let mut candidates = legacy_states(directory)?;
        (candidates.len() == 1).then(|| candidates.remove(0))
    };
    if let Some((workspace_key, state)) = candidate {
        store_sqlite_state(connection, &workspace_key, &state, true)?;
        remove_legacy_workspace_files(directory, &workspace_key);
        let _ = std::fs::remove_file(directory.join("active-workspace"));
    }
    Ok(())
}

fn remove_legacy_workspace_files(directory: &Path, workspace_key: &str) {
    let _ = std::fs::remove_file(directory.join(format!("{workspace_key}.json")));
    let _ = std::fs::remove_file(directory.join(format!("{workspace_key}.search.json")));
}

fn legacy_states(directory: &Path) -> Result<Vec<(String, CachedWorkspaceState)>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut states = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(ToString::to_string) else {
            continue;
        };
        let Some(key) = name.strip_suffix(".json") else {
            continue;
        };
        if !is_workspace_key(key) {
            continue;
        }
        if let Some(state) = read_legacy_state(directory, key)? {
            if !state.workspace_id.trim().is_empty() {
                states.push((key.to_string(), state));
            }
        }
    }
    Ok(states)
}

fn read_legacy_state(
    directory: &Path,
    workspace_key: &str,
) -> Result<Option<CachedWorkspaceState>> {
    let path = directory.join(format!("{workspace_key}.json"));
    let data = match std::fs::read_to_string(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let state = match serde_json::from_str::<CachedWorkspaceState>(&data) {
        Ok(state) if state.version == CACHE_VERSION => state,
        Ok(_) | Err(_) => return Ok(None),
    };
    Ok(Some(state))
}

fn is_workspace_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
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

fn merge_channel_history_pages(
    existing: &[SlackMessage],
    page: &[SlackMessage],
) -> Vec<SlackMessage> {
    channel_timeline_messages(merge_history_pages(existing, page))
}

fn channel_timeline_messages(messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    pruned_history(
        messages
            .into_iter()
            .filter(SlackMessage::belongs_in_channel_timeline)
            .collect(),
    )
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

    #[test]
    fn store_errors_classify_recovery_relevant_failures() {
        let rejected = StoreError::rejected_update("empty membership snapshot");
        let schema = StoreError::incompatible_schema(2, 1);
        let corrupt = StoreError::from(serde_json::from_str::<serde_json::Value>("{").unwrap_err());
        let local_io = StoreError::from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cache is read-only",
        ));

        assert_eq!(rejected.category(), StoreErrorCategory::RejectedUpdate);
        assert_eq!(schema.category(), StoreErrorCategory::IncompatibleSchema);
        assert_eq!(corrupt.category(), StoreErrorCategory::CorruptData);
        assert_eq!(local_io.category(), StoreErrorCategory::LocalIo);
    }

    #[test]
    fn store_errors_preserve_database_sources() {
        let error = StoreError::from(rusqlite::Error::InvalidQuery);

        assert_eq!(error.category(), StoreErrorCategory::Unexpected);
        assert!(matches!(
            error,
            StoreError::Database(rusqlite::Error::InvalidQuery)
        ));
    }

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
                    .load_state()
                    .await
                    .expect("workspace state load failed")
                    .expect("missing cached workspace state")
                    .workspace_id,
                "T123:U123"
            );
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
    fn schema_v1_migrates_to_v2_without_losing_keyed_payloads() {
        let directory = temp_cache_dir("workspace-schema-v2-migration");
        std::fs::create_dir_all(&directory).unwrap();
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let connection = Connection::open(store.database_path()).unwrap();
        connection
            .execute_batch(
                "PRAGMA user_version = 1;
                 CREATE TABLE workspaces (
                     workspace_key TEXT PRIMARY KEY,
                     workspace_id TEXT NOT NULL
                 ) WITHOUT ROWID;
                 CREATE TABLE app_state (
                     singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                     active_workspace_key TEXT REFERENCES workspaces(workspace_key)
                 );
                 INSERT INTO app_state(singleton, active_workspace_key) VALUES (1, NULL);
                 CREATE TABLE workspace_items (
                     workspace_key TEXT NOT NULL REFERENCES workspaces(workspace_key) ON DELETE CASCADE,
                     kind TEXT NOT NULL,
                     item_key TEXT NOT NULL,
                     payload_json TEXT NOT NULL,
                     PRIMARY KEY (workspace_key, kind, item_key)
                 ) WITHOUT ROWID;",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO workspaces(workspace_key, workspace_id) VALUES (?1, 'T123:U123')",
                [&store.workspace_key],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
                 VALUES (?1, 'conversation', 'C1', ?2)",
                params![
                    &store.workspace_key,
                    serde_json::to_string(&SlackConversation {
                        id: "C1".into(),
                        name: Some("general".into()),
                        ..Default::default()
                    })
                    .unwrap()
                ],
            )
            .unwrap();
        drop(connection);

        let conversations = runtime()
            .block_on(store.load_conversations())
            .unwrap()
            .unwrap();
        assert_eq!(conversations[0].id, "C1");

        let connection = Connection::open(store.database_path()).unwrap();
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let metadata_columns: Vec<String> = connection
            .prepare("PRAGMA table_info(sync_metadata)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            metadata_columns,
            [
                "workspace_key",
                "operation",
                "target",
                "refreshed_at_ms",
                "retry_count",
                "retry_after_ms"
            ]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn corrupt_database_is_recreated_as_an_empty_v2_cache() {
        let directory = temp_cache_dir("workspace-corrupt-database-reset");
        std::fs::create_dir_all(&directory).unwrap();
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        std::fs::write(store.database_path(), b"not a sqlite database").unwrap();

        assert!(runtime()
            .block_on(store.load_conversations())
            .unwrap()
            .is_none());
        let connection = Connection::open(store.database_path()).unwrap();
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn failed_v1_metadata_migration_recreates_only_the_derived_cache() {
        let directory = temp_cache_dir("workspace-failed-v1-migration-reset");
        std::fs::create_dir_all(&directory).unwrap();
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let connection = Connection::open(store.database_path()).unwrap();
        connection
            .execute_batch(
                "PRAGMA user_version = 1;
                 CREATE TABLE sync_metadata (broken TEXT);",
            )
            .unwrap();
        drop(connection);
        let credentials_sentinel = directory.join("credentials-are-external");
        let drafts_sentinel = directory.join("drafts-are-external");
        std::fs::write(&credentials_sentinel, "preserve").unwrap();
        std::fs::write(&drafts_sentinel, "preserve").unwrap();

        assert!(runtime()
            .block_on(store.load_conversations())
            .unwrap()
            .is_none());
        assert_eq!(
            std::fs::read_to_string(credentials_sentinel).unwrap(),
            "preserve"
        );
        assert_eq!(
            std::fs::read_to_string(drafts_sentinel).unwrap(),
            "preserve"
        );
        let connection = Connection::open(store.database_path()).unwrap();
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn malformed_keyed_payload_resets_the_workspace_cache() {
        let directory = temp_cache_dir("workspace-malformed-payload-reset");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }])
                .await
                .unwrap();
        });
        let connection = Connection::open(store.database_path()).unwrap();
        connection
            .execute(
                "UPDATE workspace_items SET payload_json = '{broken'
                 WHERE workspace_key = ?1 AND kind = 'conversation'",
                [&store.workspace_key],
            )
            .unwrap();
        drop(connection);

        assert!(runtime()
            .block_on(store.load_conversations())
            .unwrap()
            .is_none());
        let remaining: u32 = Connection::open(store.database_path())
            .unwrap()
            .query_row("SELECT count(*) FROM workspace_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_does_not_persist_ephemeral_huddle_metadata() {
        let directory = temp_cache_dir("workspace-huddle-privacy");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");

        runtime().block_on(async {
            let conversation: SlackConversation = serde_json::from_value(serde_json::json!({
                "id": "C123",
                "name": "general",
                "properties": {
                    "huddles": {
                        "id": "R_PRIVATE",
                        "participants": ["U_PRIVATE"]
                    },
                    "canvas": { "enabled": true }
                }
            }))
            .unwrap();
            assert!(conversation.has_huddle_metadata());

            store
                .store_conversations(std::slice::from_ref(&conversation))
                .await
                .expect("conversation snapshot store failed");
            store
                .store_conversation(&conversation)
                .await
                .expect("conversation row store failed");

            let cached = store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversation");
            assert!(!cached[0].has_huddle_metadata());
            assert_eq!(
                cached[0]
                    .extra
                    .get("properties")
                    .and_then(|value| value.get("canvas"))
                    .and_then(|value| value.get("enabled"))
                    .and_then(serde_json::Value::as_bool),
                Some(true)
            );

            let connection = Connection::open(store.database_path()).unwrap();
            let payload: String = connection
                .query_row(
                    "SELECT payload_json FROM workspace_items
                     WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C123'",
                    [&store.workspace_key],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!payload.contains("R_PRIVATE"));
            assert!(!payload.contains("U_PRIVATE"));
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn ensuring_workspace_identity_upgrades_an_existing_cache() {
        let directory = temp_cache_dir("workspace-store-identity-upgrade");
        std::fs::create_dir_all(&directory).unwrap();
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        std::fs::write(
            store.path(),
            r#"{"version":1,"conversations":[{"id":"D1","is_im":true}]}"#,
        )
        .unwrap();

        runtime()
            .block_on(store.ensure_workspace_identity())
            .expect("workspace identity upgrade failed");

        let state = runtime()
            .block_on(store.load_state())
            .unwrap()
            .expect("missing upgraded state");
        assert_eq!(state.workspace_id, "T123:U123");
        assert_eq!(state.conversations[0].id, "D1");
        assert!(!store.path().exists());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn workspace_store_exposes_a_lightweight_active_search_snapshot() {
        let directory = temp_cache_dir("workspace-search-index");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    is_channel: Some(true),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store
                .store_history(
                    "C1",
                    &[SlackMessage {
                        ts: "1.0".into(),
                        text: Some("private message body".into()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            store.ensure_workspace_identity().await.unwrap();
        });

        let search_state = load_active_search_state(&directory).unwrap().unwrap();
        assert_eq!(search_state.workspace_id, "T123:U123");
        assert_eq!(search_state.conversations[0].id, "C1");
        assert!(store.database_path().exists());

        let connection = Connection::open(store.database_path()).unwrap();
        let stored_private_body: bool = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM workspace_items
                    WHERE workspace_key = ?1 AND kind = 'channel_history'
                      AND payload_json LIKE '%private message body%'
                )",
                [&store.workspace_key],
                |row| row.get(0),
            )
            .unwrap();
        assert!(stored_private_body);

        connection
            .execute(
                "UPDATE workspace_items SET payload_json = 'not valid JSON'
                 WHERE workspace_key = ?1 AND kind = 'channel_history'",
                [&store.workspace_key],
            )
            .unwrap();
        let search_state = load_active_search_state(&directory).unwrap().unwrap();
        assert_eq!(search_state.conversations[0].id, "C1");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn clearing_the_active_workspace_preserves_its_cached_state() {
        let directory = temp_cache_dir("workspace-clear-active");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        runtime().block_on(async {
            store
                .store_conversations(&[SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }])
                .await
                .unwrap();
            store.ensure_workspace_identity().await.unwrap();
        });

        clear_active_workspace(&directory).unwrap();
        runtime().block_on(async {
            store
                .store_history(
                    "C1",
                    &[SlackMessage {
                        ts: "1.0".into(),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
        });

        assert!(load_active_search_state(&directory).unwrap().is_none());
        let cached = runtime().block_on(store.load_state()).unwrap().unwrap();
        assert_eq!(cached.conversations[0].id, "C1");
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
    fn conversation_row_mutations_ignore_unrelated_corrupt_rows() {
        let directory = temp_cache_dir("workspace-store-conversation-row-update");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[
                    SlackConversation {
                        id: "C1".to_string(),
                        name: Some("old".to_string()),
                        unread_count: Some(3),
                        ..Default::default()
                    },
                    SlackConversation {
                        id: "C2".to_string(),
                        name: Some("unrelated".to_string()),
                        ..Default::default()
                    },
                ])
                .await
                .expect("conversation store failed");

            let connection = Connection::open(store.database_path()).unwrap();
            connection
                .execute(
                    "UPDATE workspace_items SET payload_json = '{broken'
                     WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C2'",
                    [&store.workspace_key],
                )
                .unwrap();
            drop(connection);

            assert!(store
                .clear_conversation_unread_state("C1", "20.0")
                .await
                .expect("read update failed"));
            store
                .merge_conversation(&SlackConversation {
                    id: "C1".to_string(),
                    name: Some("renamed".to_string()),
                    unread_count: Some(8),
                    ..Default::default()
                })
                .await
                .expect("metadata update read an unrelated row");
            assert!(!store
                .apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(true, true, 4),
                    Some("10.0"),
                )
                .await
                .expect("stale unread update failed"));
            assert!(store
                .mark_conversation_unread_from_event("C1", "21.0")
                .await
                .expect("realtime update read an unrelated row"));
        });

        let connection = Connection::open(store.database_path()).unwrap();
        let updated_payload: String = connection
            .query_row(
                "SELECT payload_json FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C1'",
                [&store.workspace_key],
                |row| row.get(0),
            )
            .unwrap();
        let updated: SlackConversation = serde_json::from_str(&updated_payload).unwrap();
        assert_eq!(updated.name.as_deref(), Some("renamed"));
        assert_eq!(updated.unread_activity_count(), 1);
        assert_eq!(
            updated
                .extra
                .get(LOCAL_READ_TS_KEY)
                .and_then(serde_json::Value::as_str),
            Some("20.0")
        );
        let unrelated_payload: String = connection
            .query_row(
                "SELECT payload_json FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C2'",
                [&store.workspace_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unrelated_payload, "{broken");

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn conversation_row_mutations_do_not_follow_mismatched_payload_ids() {
        let directory = temp_cache_dir("workspace-store-conversation-row-id-mismatch");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_conversations(&[
                    SlackConversation {
                        id: "C0".to_string(),
                        name: Some("untouched".to_string()),
                        unread_count: Some(2),
                        ..Default::default()
                    },
                    SlackConversation {
                        id: "C1".to_string(),
                        name: Some("original".to_string()),
                        ..Default::default()
                    },
                ])
                .await
                .expect("conversation store failed");

            let mismatched = serde_json::to_string(&SlackConversation {
                id: "C0".to_string(),
                name: Some("mismatched".to_string()),
                unread_count: Some(99),
                ..Default::default()
            })
            .unwrap();
            let replace_c1_payload = |payload: &str| {
                let connection = Connection::open(store.database_path()).unwrap();
                connection
                    .execute(
                        "UPDATE workspace_items SET payload_json = ?1
                         WHERE workspace_key = ?2 AND kind = 'conversation' AND item_key = 'C1'",
                        params![payload, &store.workspace_key],
                    )
                    .unwrap();
            };
            replace_c1_payload(&mismatched);

            assert!(!store
                .apply_conversation_unread_state(
                    "C1",
                    SlackUnreadState::from_parts(true, true, 7),
                    None,
                )
                .await
                .expect("mismatched unread update failed"));
            assert!(!store
                .clear_conversation_unread_state("C1", "20.0")
                .await
                .expect("mismatched read update failed"));

            store
                .store_conversation(&SlackConversation {
                    id: "C1".to_string(),
                    name: Some("metadata repaired".to_string()),
                    ..Default::default()
                })
                .await
                .expect("metadata repair failed");
            let repaired = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C0")
                    .and_then(|conversation| conversation.name.as_deref()),
                Some("untouched")
            );
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C0")
                    .map(SlackConversation::unread_activity_count),
                Some(2)
            );
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C1")
                    .and_then(|conversation| conversation.name.as_deref()),
                Some("metadata repaired")
            );

            replace_c1_payload(&mismatched);
            assert!(store
                .mark_conversation_unread_from_event("C1", "21.0")
                .await
                .expect("realtime repair failed"));
            let repaired = store.load_conversations().await.unwrap().unwrap();
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C0")
                    .and_then(|conversation| conversation.name.as_deref()),
                Some("untouched")
            );
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C0")
                    .map(SlackConversation::unread_activity_count),
                Some(2)
            );
            assert_eq!(
                repaired
                    .iter()
                    .find(|conversation| conversation.id == "C1")
                    .map(SlackConversation::unread_activity_count),
                Some(1)
            );
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

            store
                .store_user_full_names(&HashMap::from([(
                    "U123".to_string(),
                    "Augusta Ada King".to_string(),
                )]))
                .await
                .expect("user full name store failed");
            assert_eq!(
                store
                    .load_user_full_names()
                    .await
                    .expect("user full name load failed")
                    .get("U123")
                    .map(String::as_str),
                Some("Augusta Ada King")
            );

            let avatar_urls = HashMap::from([(
                "U123".to_string(),
                "https://avatars.slack-edge.com/ada.png".to_string(),
            )]);
            store
                .store_user_avatar_urls(&avatar_urls)
                .await
                .expect("user avatar URL store failed");
            assert_eq!(
                store
                    .load_user_avatar_urls()
                    .await
                    .expect("user avatar URL load failed"),
                avatar_urls
            );

            let aliases = HashMap::from([(
                "U123".to_string(),
                vec!["Ada".to_string(), "Ada Lovelace".to_string()],
            )]);
            store
                .store_user_search_aliases(&aliases)
                .await
                .expect("user search alias store failed");
            assert_eq!(
                store
                    .load_user_search_aliases()
                    .await
                    .expect("user search alias load failed"),
                aliases
            );

            let status = SlackUserStatus {
                text: "In a meeting".to_string(),
                emoji: ":calendar:".to_string(),
                expiration: 200,
            };
            store
                .store_user_status("U123", Some(status.clone()))
                .await
                .expect("user status store failed");
            assert_eq!(
                store
                    .load_user_statuses()
                    .await
                    .expect("user status load failed")
                    .get("U123"),
                Some(&status)
            );
            store
                .store_user_status("U123", None)
                .await
                .expect("user status removal failed");
            assert!(store
                .load_user_statuses()
                .await
                .expect("user status load failed")
                .is_empty());
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
    fn channel_history_filters_thread_replies_but_keeps_broadcasts() {
        let directory = temp_cache_dir("workspace-store-thread-routing");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            let root = SlackMessage {
                ts: "1.0".into(),
                thread_ts: Some("1.0".into()),
                ..Default::default()
            };
            let reply = SlackMessage {
                ts: "2.0".into(),
                thread_ts: Some("1.0".into()),
                ..Default::default()
            };
            let mut broadcast = reply.clone();
            broadcast.ts = "3.0".into();
            broadcast.subtype = Some("thread_broadcast".into());

            store
                .store_merged_history("C1", &[root.clone(), reply.clone(), broadcast.clone()])
                .await
                .unwrap();
            assert_eq!(
                store
                    .load_history("C1")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["3.0", "1.0"]
            );

            // Loading also sanitizes caches written by older Conduit versions.
            store
                .update_state(|state| {
                    state
                        .channel_histories
                        .insert("C2".into(), vec![root, reply, broadcast]);
                })
                .await
                .unwrap();
            assert_eq!(
                store
                    .load_history("C2")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["3.0", "1.0"]
            );
        });

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn stale_thread_snapshot_keeps_newer_realtime_reply() {
        let directory = temp_cache_dir("workspace-store-realtime-thread-race");
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let runtime = runtime();

        runtime.block_on(async {
            store
                .store_merged_thread(
                    "C1",
                    "1.0",
                    &[SlackMessage {
                        ts: "2.0".into(),
                        thread_ts: Some("1.0".into()),
                        text: Some("realtime reply".into()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();
            store
                .store_thread(
                    "C1",
                    "1.0",
                    &[SlackMessage {
                        ts: "1.0".into(),
                        text: Some("stale parent".into()),
                        ..Default::default()
                    }],
                )
                .await
                .unwrap();

            assert_eq!(
                store
                    .load_thread("C1", "1.0")
                    .await
                    .unwrap()
                    .unwrap()
                    .iter()
                    .map(|message| message.ts.as_str())
                    .collect::<Vec<_>>(),
                vec!["2.0", "1.0"]
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
