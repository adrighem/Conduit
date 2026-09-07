use std::collections::HashMap;

use crate::models::{SlackConversation, SlackUser};
use crate::slack::{SlackApi, SlackError};

#[allow(dead_code)]
pub(crate) trait WorkspaceSlack {
    async fn conversations(&self) -> Result<Vec<SlackConversation>, SlackError>;
    async fn discover_conversations(&self) -> Result<Vec<SlackConversation>, SlackError>;
    async fn users(&self) -> Result<Vec<SlackUser>, SlackError>;
    async fn custom_emojis(&self) -> Result<HashMap<String, String>, SlackError>;
    async fn join_conversation(&self, channel_id: &str) -> Result<SlackConversation, SlackError>;
    async fn leave_conversation(&self, channel_id: &str) -> Result<(), SlackError>;
    async fn open_direct_message(&self, user_id: &str) -> Result<SlackConversation, SlackError>;
    async fn open_direct_message_with_users(
        &self,
        user_ids: &[String],
    ) -> Result<SlackConversation, SlackError>;
    async fn create_channel(
        &self,
        name: &str,
        is_private: bool,
    ) -> Result<SlackConversation, SlackError>;
    async fn invite_to_channel(
        &self,
        channel_id: &str,
        user_ids: &[String],
    ) -> Result<SlackConversation, SlackError>;
}

impl WorkspaceSlack for SlackApi {
    async fn conversations(&self) -> Result<Vec<SlackConversation>, SlackError> {
        self.conversations().await
    }

    async fn discover_conversations(&self) -> Result<Vec<SlackConversation>, SlackError> {
        self.discover_conversations().await
    }

    async fn users(&self) -> Result<Vec<SlackUser>, SlackError> {
        self.users().await
    }

    async fn custom_emojis(&self) -> Result<HashMap<String, String>, SlackError> {
        self.custom_emojis().await
    }

    async fn join_conversation(&self, channel_id: &str) -> Result<SlackConversation, SlackError> {
        self.join_conversation(channel_id).await
    }

    async fn leave_conversation(&self, channel_id: &str) -> Result<(), SlackError> {
        self.leave_conversation(channel_id).await
    }

    async fn open_direct_message(&self, user_id: &str) -> Result<SlackConversation, SlackError> {
        self.open_direct_message(user_id).await
    }

    async fn open_direct_message_with_users(
        &self,
        user_ids: &[String],
    ) -> Result<SlackConversation, SlackError> {
        self.open_direct_message_with_users(user_ids).await
    }

    async fn create_channel(
        &self,
        name: &str,
        is_private: bool,
    ) -> Result<SlackConversation, SlackError> {
        self.create_channel(name, is_private).await
    }

    async fn invite_to_channel(
        &self,
        channel_id: &str,
        user_ids: &[String],
    ) -> Result<SlackConversation, SlackError> {
        self.invite_to_channel(channel_id, user_ids).await
    }
}

#[allow(dead_code)]
pub(crate) struct WorkspaceService<'a, Slack> {
    slack: &'a Slack,
}

#[allow(dead_code)]
impl<'a, Slack> WorkspaceService<'a, Slack>
where
    Slack: WorkspaceSlack,
{
    pub(crate) fn new(slack: &'a Slack) -> Self {
        Self { slack }
    }

    pub(crate) async fn list_conversations(&self) -> Result<Vec<SlackConversation>, SlackError> {
        self.slack.conversations().await
    }

    pub(crate) async fn discover_conversations(
        &self,
    ) -> Result<Vec<SlackConversation>, SlackError> {
        self.slack.discover_conversations().await
    }

    pub(crate) async fn list_users(&self) -> Result<Vec<SlackUser>, SlackError> {
        self.slack.users().await
    }

    pub(crate) async fn load_custom_emojis(&self) -> Result<HashMap<String, String>, SlackError> {
        self.slack.custom_emojis().await
    }

    pub(crate) async fn join_conversation(
        &self,
        channel_id: &str,
    ) -> Result<SlackConversation, SlackError> {
        self.slack.join_conversation(channel_id).await
    }

    pub(crate) async fn leave_conversation(&self, channel_id: &str) -> Result<(), SlackError> {
        self.slack.leave_conversation(channel_id).await
    }

    pub(crate) async fn open_direct_message(
        &self,
        user_id: &str,
    ) -> Result<SlackConversation, SlackError> {
        self.slack.open_direct_message(user_id).await
    }

    pub(crate) async fn open_group_direct_message(
        &self,
        user_ids: &[String],
    ) -> Result<SlackConversation, SlackError> {
        self.slack.open_direct_message_with_users(user_ids).await
    }

    pub(crate) async fn create_channel(
        &self,
        name: &str,
        is_private: bool,
    ) -> Result<SlackConversation, SlackError> {
        self.slack.create_channel(name, is_private).await
    }

    pub(crate) async fn invite_to_channel(
        &self,
        channel_id: &str,
        user_ids: &[String],
    ) -> Result<SlackConversation, SlackError> {
        self.slack.invite_to_channel(channel_id, user_ids).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockWorkspaceSlack;

    impl WorkspaceSlack for MockWorkspaceSlack {
        async fn conversations(&self) -> Result<Vec<SlackConversation>, SlackError> {
            Ok(vec![SlackConversation {
                id: "C123".to_string(),
                name: Some("general".to_string()),
                is_channel: Some(true),
                ..SlackConversation::default()
            }])
        }

        async fn discover_conversations(&self) -> Result<Vec<SlackConversation>, SlackError> {
            Ok(vec![])
        }

        async fn users(&self) -> Result<Vec<SlackUser>, SlackError> {
            Ok(vec![SlackUser {
                id: Some("U123".to_string()),
                name: Some("testuser".to_string()),
                real_name: Some("Test User".to_string()),
                ..SlackUser::default()
            }])
        }

        async fn custom_emojis(&self) -> Result<HashMap<String, String>, SlackError> {
            let mut emojis = HashMap::new();
            emojis.insert(
                "parrot".to_string(),
                "http://example.com/parrot.gif".to_string(),
            );
            Ok(emojis)
        }

        async fn join_conversation(
            &self,
            channel_id: &str,
        ) -> Result<SlackConversation, SlackError> {
            Ok(SlackConversation {
                id: channel_id.to_string(),
                name: Some("joined".to_string()),
                is_channel: Some(true),
                ..SlackConversation::default()
            })
        }

        async fn leave_conversation(&self, _channel_id: &str) -> Result<(), SlackError> {
            Ok(())
        }

        async fn open_direct_message(
            &self,
            _user_id: &str,
        ) -> Result<SlackConversation, SlackError> {
            Ok(SlackConversation {
                id: "D123".to_string(),
                name: Some("dm".to_string()),
                is_im: Some(true),
                ..SlackConversation::default()
            })
        }

        async fn open_direct_message_with_users(
            &self,
            _user_ids: &[String],
        ) -> Result<SlackConversation, SlackError> {
            Ok(SlackConversation {
                id: "G123".to_string(),
                name: Some("mpdm".to_string()),
                is_mpim: Some(true),
                ..SlackConversation::default()
            })
        }

        async fn create_channel(
            &self,
            name: &str,
            is_private: bool,
        ) -> Result<SlackConversation, SlackError> {
            Ok(SlackConversation {
                id: "C_NEW".to_string(),
                name: Some(name.to_string()),
                is_channel: Some(!is_private),
                is_private: Some(is_private),
                ..SlackConversation::default()
            })
        }

        async fn invite_to_channel(
            &self,
            channel_id: &str,
            _user_ids: &[String],
        ) -> Result<SlackConversation, SlackError> {
            Ok(SlackConversation {
                id: channel_id.to_string(),
                name: Some("invited".to_string()),
                is_channel: Some(true),
                ..SlackConversation::default()
            })
        }
    }

    #[test]
    fn workspace_service_lists_and_joins_channels() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let slack = MockWorkspaceSlack;
            let service = WorkspaceService::new(&slack);

            let convs = service.list_conversations().await.unwrap();
            assert_eq!(convs.len(), 1);
            assert_eq!(convs[0].name.as_deref(), Some("general"));

            let joined = service.join_conversation("C999").await.unwrap();
            assert_eq!(joined.id, "C999");
            assert_eq!(joined.name.as_deref(), Some("joined"));

            let users = service.list_users().await.unwrap();
            assert_eq!(users.len(), 1);
            assert_eq!(users[0].name.as_deref(), Some("testuser"));
        });
    }
}
