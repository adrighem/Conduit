use crate::models::SlackMessage;
#[cfg(test)]
use crate::slack::CHANNEL_HISTORY_PAGE_LIMIT;
use crate::slack::{SlackApi, SlackError, SlackMessagePage};
use crate::store::{StoreError, WorkspaceStore};

pub(crate) trait ConversationHistorySlack {
    async fn load_history(&self, channel_id: &str) -> Result<SlackMessagePage, SlackError>;
}

pub(crate) trait ConversationHistoryStore {
    async fn load_history(&self, channel_id: &str)
        -> Result<Option<Vec<SlackMessage>>, StoreError>;
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

    pub(crate) async fn load_cached(
        &self,
        channel_id: &str,
    ) -> Result<Option<Vec<SlackMessage>>, StoreError> {
        let Some(store) = self.store else {
            return Ok(None);
        };
        store.load_history(channel_id).await
    }

    pub(crate) async fn fetch(&self, channel_id: &str) -> Result<SlackMessagePage, SlackError> {
        self.slack.load_history(channel_id).await
    }
}

#[cfg(test)]
pub(crate) fn recent_history_preview(mut messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
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
    }

    impl ConversationHistoryStore for FakeStore {
        async fn load_history(
            &self,
            _channel_id: &str,
        ) -> Result<Option<Vec<SlackMessage>>, StoreError> {
            Ok(Some(self.cached.clone()))
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
    }

    fn message(ts: &str, text: &str) -> SlackMessage {
        SlackMessage {
            ts: ts.to_string(),
            text: Some(text.to_string()),
            ..SlackMessage::default()
        }
    }

    #[test]
    fn service_splits_full_cache_read_from_network_fetch_without_store_write() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let slack = FakeSlack::default();
            let cached = (0..CHANNEL_HISTORY_PAGE_LIMIT + 2)
                .map(|index| message(&format!("{index:02}"), "cached"))
                .collect::<Vec<_>>();
            let store = FakeStore {
                cached: cached.clone(),
            };
            let service = ConversationHistoryService::new(&slack, Some(&store));

            assert_eq!(
                service.load_cached("C1").await.unwrap(),
                Some(cached),
                "cache hydration must retain the full stored history"
            );
            let page = service.fetch("C1").await.unwrap();

            assert_eq!(page.messages, vec![message("3", "fresh")]);
            assert_eq!(slack.requested_channels.lock().unwrap().as_slice(), &["C1"]);
        });
    }

    #[test]
    fn recent_history_preview_sorts_deduplicates_and_caps_full_cache() {
        let mut cached = (0..CHANNEL_HISTORY_PAGE_LIMIT + 2)
            .map(|index| message(&format!("{index:02}"), "cached"))
            .collect::<Vec<_>>();
        cached.push(message("10", "duplicate"));

        let preview = recent_history_preview(cached);

        assert_eq!(preview.len(), CHANNEL_HISTORY_PAGE_LIMIT);
        assert_eq!(preview.first().unwrap().ts, "31");
        assert_eq!(
            preview.iter().filter(|message| message.ts == "10").count(),
            1
        );
    }

    #[test]
    fn cache_failure_does_not_prevent_fresh_history_fetch() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let slack = FakeSlack::default();
            let store = FailingStore;
            let service = ConversationHistoryService::new(&slack, Some(&store));

            let cache_error = service.load_cached("C1").await.unwrap_err();
            let page = service.fetch("C1").await.unwrap();

            assert_eq!(page.messages, vec![message("3", "fresh")]);
            assert_eq!(
                cache_error.category(),
                crate::store::StoreErrorCategory::LocalIo
            );
            assert_eq!(slack.requested_channels.lock().unwrap().as_slice(), &["C1"]);
        });
    }
}
