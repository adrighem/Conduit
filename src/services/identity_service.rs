use crate::models::{SlackUser, SlackUserProfile, SlackUserStatus};
use crate::slack::{SlackApi, SlackError};

#[allow(dead_code)]
pub(crate) trait IdentitySlack {
    async fn user(&self, user_id: &str) -> Result<SlackUser, SlackError>;
    async fn user_profile(&self, user_id: &str) -> Result<SlackUserProfile, SlackError>;
    async fn set_current_user_status(
        &self,
        status: &SlackUserStatus,
    ) -> Result<SlackUserProfile, SlackError>;
    async fn clear_current_user_status(&self) -> Result<SlackUserProfile, SlackError>;
}

impl IdentitySlack for SlackApi {
    async fn user(&self, user_id: &str) -> Result<SlackUser, SlackError> {
        self.user(user_id).await
    }

    async fn user_profile(&self, user_id: &str) -> Result<SlackUserProfile, SlackError> {
        self.user_profile(user_id).await
    }

    async fn set_current_user_status(
        &self,
        status: &SlackUserStatus,
    ) -> Result<SlackUserProfile, SlackError> {
        self.set_current_user_status(status).await
    }

    async fn clear_current_user_status(&self) -> Result<SlackUserProfile, SlackError> {
        let cleared = SlackUserStatus::default();
        self.set_current_user_status(&cleared).await
    }
}

#[allow(dead_code)]
pub(crate) struct IdentityService<'a, Slack> {
    slack: &'a Slack,
}

#[allow(dead_code)]
impl<'a, Slack> IdentityService<'a, Slack>
where
    Slack: IdentitySlack,
{
    pub(crate) fn new(slack: &'a Slack) -> Self {
        Self { slack }
    }

    pub(crate) async fn fetch_user(&self, user_id: &str) -> Result<SlackUser, SlackError> {
        self.slack.user(user_id).await
    }

    pub(crate) async fn fetch_user_profile(
        &self,
        user_id: &str,
    ) -> Result<SlackUserProfile, SlackError> {
        self.slack.user_profile(user_id).await
    }

    pub(crate) async fn set_custom_status(
        &self,
        status: &SlackUserStatus,
    ) -> Result<SlackUserProfile, SlackError> {
        self.slack.set_current_user_status(status).await
    }

    pub(crate) async fn clear_custom_status(&self) -> Result<SlackUserProfile, SlackError> {
        self.slack.clear_current_user_status().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockIdentitySlack;

    impl IdentitySlack for MockIdentitySlack {
        async fn user(&self, user_id: &str) -> Result<SlackUser, SlackError> {
            Ok(SlackUser {
                id: Some(user_id.to_string()),
                name: Some("testuser".to_string()),
                real_name: Some("Test User".to_string()),
                is_bot: Some(false),
                deleted: Some(false),
                ..SlackUser::default()
            })
        }

        async fn user_profile(&self, _user_id: &str) -> Result<SlackUserProfile, SlackError> {
            Ok(SlackUserProfile {
                status_text: Some("Working remotely".to_string()),
                status_emoji: Some(":house:".to_string()),
                status_expiration: Some(1700000000),
                ..SlackUserProfile::default()
            })
        }

        async fn set_current_user_status(
            &self,
            status: &SlackUserStatus,
        ) -> Result<SlackUserProfile, SlackError> {
            Ok(SlackUserProfile {
                status_text: Some(status.text.clone()),
                status_emoji: Some(format!(":{}:", status.emoji_name())),
                status_expiration: Some(status.expiration),
                ..SlackUserProfile::default()
            })
        }

        async fn clear_current_user_status(&self) -> Result<SlackUserProfile, SlackError> {
            Ok(SlackUserProfile::default())
        }
    }

    #[test]
    fn identity_service_fetches_user_and_profile() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let slack = MockIdentitySlack;
            let service = IdentityService::new(&slack);

            let user = service.fetch_user("U123").await.unwrap();
            assert_eq!(user.id.as_deref(), Some("U123"));
            assert_eq!(user.name.as_deref(), Some("testuser"));

            let profile = service.fetch_user_profile("U123").await.unwrap();
            assert_eq!(profile.status_text.as_deref(), Some("Working remotely"));
            assert_eq!(profile.status_emoji.as_deref(), Some(":house:"));

            let updated = service
                .set_custom_status(&SlackUserStatus {
                    text: "Focusing".to_string(),
                    emoji: ":dart:".to_string(),
                    expiration: 0,
                })
                .await
                .unwrap();
            assert_eq!(updated.status_text.as_deref(), Some("Focusing"));
        });
    }
}
