use crate::models::SlackMessage;
use crate::slack::{SlackApi, SlackError, SlackMessagePage, CHANNEL_HISTORY_PAGE_LIMIT};
use crate::store::{StoreError, WorkspaceStore};

pub(crate) trait ConversationHistorySlack {
    async fn load_history(&self, channel_id: &str) -> Result<SlackMessagePage, SlackError>;
}

pub(crate) trait ConversationHistoryStore {
    async fn load_history(&self, channel_id: &str)
        -> Result<Option<Vec<SlackMessage>>, StoreError>;

    async fn store_history(
        &self,
        channel_id: &str,
        messages: &[SlackMessage],
    ) -> Result<(), StoreError>;
}

impl ConversationHistorySlack for SlackApi {
    async fn load_history(&self, channel_id: &str) -> Result<SlackMessagePage, SlackError> {
        self.history(channel_id).await
    }
}

impl ConversationHistoryStore for WorkspaceStore {
    async fn load_history(
        &self,
        channel_id: &str,
    ) -> Result<Option<Vec<SlackMessage>>, StoreError> {
        self.load_history(channel_id).await
    }

    async fn store_history(
        &self,
        channel_id: &str,
        messages: &[SlackMessage],
    ) -> Result<(), StoreError> {
        self.store_history(channel_id, messages).await
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConversationHistoryProgress {
    Cached(Vec<SlackMessage>),
    Loading,
    CacheReadFailed,
    CacheWriteFailed,
}

pub(crate) struct ConversationHistoryService<'a, Slack, Store> {
    slack: &'a Slack,
    store: Option<&'a Store>,
}

impl<'a, Slack, Store> ConversationHistoryService<'a, Slack, Store>
where
    Slack: ConversationHistorySlack,
    Store: ConversationHistoryStore,
{
    pub(crate) fn new(slack: &'a Slack, store: Option<&'a Store>) -> Self {
        Self { slack, store }
    }

    pub(crate) async fn load(
        &self,
        channel_id: &str,
        mut progress: impl FnMut(ConversationHistoryProgress),
    ) -> Result<SlackMessagePage, SlackError> {
        if let Some(store) = self.store {
            match store.load_history(channel_id).await {
                Ok(Some(messages)) if !messages.is_empty() => progress(
                    ConversationHistoryProgress::Cached(recent_history_preview(messages)),
                ),
                Ok(_) => {}
                Err(_) => progress(ConversationHistoryProgress::CacheReadFailed),
            }
        }

        progress(ConversationHistoryProgress::Loading);
        let page = self.slack.load_history(channel_id).await?;

        if let Some(store) = self.store {
            if store
                .store_history(channel_id, &page.messages)
                .await
                .is_err()
            {
                progress(ConversationHistoryProgress::CacheWriteFailed);
            }
        }

        Ok(page)
    }
}

fn recent_history_preview(mut messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    messages.sort_by(|left, right| right.ts.cmp(&left.ts));
    messages.dedup_by(|left, right| !left.ts.is_empty() && left.ts == right.ts);
    messages.truncate(CHANNEL_HISTORY_PAGE_LIMIT);
    messages
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::models::{SlackMessage, SlackUnreadState};
    use crate::slack::{SlackError, SlackMessagePage, CHANNEL_HISTORY_PAGE_LIMIT};
    use crate::store::StoreError;

    #[derive(Default)]
    struct FakeSlack {
        requested_channels: Mutex<Vec<String>>,
    }

    impl ConversationHistorySlack for FakeSlack {
        async fn load_history(&self, channel_id: &str) -> Result<SlackMessagePage, SlackError> {
            self.requested_channels
                .lock()
                .unwrap()
                .push(channel_id.to_string());
            Ok(SlackMessagePage {
                messages: vec![message("3", "fresh")],
                has_more: true,
                next_cursor: Some("next".into()),
                unread_state: SlackUnreadState::default(),
            })
        }
    }

    struct FakeStore {
        cached: Vec<SlackMessage>,
        stored: Mutex<Vec<(String, Vec<SlackMessage>)>>,
    }

    impl ConversationHistoryStore for FakeStore {
        async fn load_history(
            &self,
            _channel_id: &str,
        ) -> Result<Option<Vec<SlackMessage>>, StoreError> {
            Ok(Some(self.cached.clone()))
        }

        async fn store_history(
            &self,
            channel_id: &str,
            messages: &[SlackMessage],
        ) -> Result<(), StoreError> {
            self.stored
                .lock()
                .unwrap()
                .push((channel_id.to_string(), messages.to_vec()));
            Ok(())
        }
    }

    struct FailingStore;

    impl ConversationHistoryStore for FailingStore {
        async fn load_history(
            &self,
            _channel_id: &str,
        ) -> Result<Option<Vec<SlackMessage>>, StoreError> {
            Err(StoreError::Io(std::io::Error::other("cache unavailable")))
        }

        async fn store_history(
            &self,
            _channel_id: &str,
            _messages: &[SlackMessage],
        ) -> Result<(), StoreError> {
            Err(StoreError::Io(std::io::Error::other("cache unavailable")))
        }
    }

    fn message(ts: &str, text: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.to_string(),
            text: Some(text.to_string()),
            ..SlackMessage::default()
        }
    }

    #[test]
    fn service_emits_cached_preview_before_loading_fresh_history() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let slack = FakeSlack::default();
            let cached = (0..CHANNEL_HISTORY_PAGE_LIMIT + 2)
                .map(|index| message(&format!("{index:02}"), "cached"))
                .collect();
            let store = FakeStore {
                cached,
                stored: Mutex::new(Vec::new()),
            };
            let service = ConversationHistoryService::new(&slack, Some(&store));
            let mut progress = Vec::new();

            let page = service
                .load("C1", |update| progress.push(update))
                .await
                .unwrap();

            assert_eq!(page.messages, vec![message("3", "fresh")]);
            assert!(matches!(
                progress[0],
                ConversationHistoryProgress::Cached(_)
            ));
            let ConversationHistoryProgress::Cached(preview) = &progress[0] else {
                unreachable!();
            };
            assert_eq!(preview.len(), CHANNEL_HISTORY_PAGE_LIMIT);
            assert_eq!(preview.first().unwrap().ts, "31");
            assert_eq!(progress[1], ConversationHistoryProgress::Loading);
            assert_eq!(slack.requested_channels.lock().unwrap().as_slice(), &["C1"]);
            assert_eq!(store.stored.lock().unwrap()[0].0, "C1");
            assert_eq!(store.stored.lock().unwrap()[0].1, page.messages);
        });
    }

    #[test]
    fn cache_failures_are_reported_without_hiding_fresh_history() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let slack = FakeSlack::default();
            let store = FailingStore;
            let service = ConversationHistoryService::new(&slack, Some(&store));
            let mut progress = Vec::new();

            let page = service
                .load("C1", |update| progress.push(update))
                .await
                .unwrap();

            assert_eq!(page.messages, vec![message("3", "fresh")]);
            assert_eq!(
                progress,
                vec![
                    ConversationHistoryProgress::CacheReadFailed,
                    ConversationHistoryProgress::Loading,
                    ConversationHistoryProgress::CacheWriteFailed,
                ]
            );
        });
    }
}
