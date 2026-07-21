use crate::huddles::model::ActiveHuddle;

const MAX_HUDDLE_LINK_BYTES: usize = 8 * 1024;
const SLACK_HUDDLE_ORIGIN: &str = "https://app.slack.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HuddleFallbackError {
    #[error("invalid Slack workspace identifier")]
    TeamId,
    #[error("invalid Slack conversation identifier")]
    ChannelId,
    #[error("invalid Slack huddle link")]
    SuppliedLink,
}

pub fn external_huddle_url(huddle: &ActiveHuddle) -> Result<String, HuddleFallbackError> {
    if !valid_slack_id(&huddle.team_id, &['T']) {
        return Err(HuddleFallbackError::TeamId);
    }
    if !valid_slack_id(&huddle.channel_id, &['C', 'G', 'D']) {
        return Err(HuddleFallbackError::ChannelId);
    }

    let canonical = format!(
        "{SLACK_HUDDLE_ORIGIN}/huddle/{}/{}",
        huddle.team_id, huddle.channel_id
    );
    if huddle.huddle_link.as_deref().is_some_and(|supplied| {
        !valid_supplied_link(supplied, &huddle.team_id, &huddle.channel_id, &canonical)
    }) {
        return Err(HuddleFallbackError::SuppliedLink);
    }

    Ok(canonical)
}

fn valid_slack_id(value: &str, prefixes: &[char]) -> bool {
    (2..=64).contains(&value.len())
        && value.is_ascii()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
        && value
            .chars()
            .next()
            .is_some_and(|prefix| prefixes.contains(&prefix))
}

fn valid_supplied_link(supplied: &str, team_id: &str, channel_id: &str, canonical: &str) -> bool {
    if supplied.is_empty()
        || supplied.len() > MAX_HUDDLE_LINK_BYTES
        || supplied.trim() != supplied
        || supplied.chars().any(char::is_control)
        // Requiring the canonical spelling also rejects explicit default ports,
        // encoded path separators, user information, and URL normalization tricks.
        || supplied != canonical
    {
        return false;
    }

    let Ok(url) = url::Url::parse(supplied) else {
        return false;
    };
    let expected_path = format!("/huddle/{team_id}/{channel_id}");
    url.scheme() == "https"
        && url.host_str() == Some("app.slack.com")
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.path() == expected_path
        && url.query().is_none()
        && url.fragment().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn huddle(link: Option<&str>) -> ActiveHuddle {
        ActiveHuddle {
            team_id: "T123".to_string(),
            channel_id: "C456".to_string(),
            call_id: "R789".to_string(),
            name: None,
            participant_ids: Vec::new(),
            started_at: None,
            huddle_link: link.map(ToString::to_string),
        }
    }

    #[test]
    fn builds_the_canonical_https_huddle_url_without_a_supplied_link() {
        let url = external_huddle_url(&huddle(None)).unwrap();

        assert_eq!(url, "https://app.slack.com/huddle/T123/C456");
    }

    #[test]
    fn accepts_only_a_supplied_link_for_the_exact_huddle() {
        let exact = huddle(Some("https://app.slack.com/huddle/T123/C456"));
        assert_eq!(
            external_huddle_url(&exact).unwrap(),
            "https://app.slack.com/huddle/T123/C456"
        );

        for link in [
            "https://app.slack.com/huddle/T999/C456",
            "https://app.slack.com/huddle/T123/C999",
            "https://app.slack.com/huddle/T123/C456/",
            "https://app.slack.com/huddle/T123/C456/extra",
        ] {
            assert_eq!(
                external_huddle_url(&huddle(Some(link))),
                Err(HuddleFallbackError::SuppliedLink),
                "accepted {link}"
            );
        }
    }

    #[test]
    fn rejects_non_https_or_non_slack_supplied_links() {
        for link in [
            "http://app.slack.com/huddle/T123/C456",
            "slack://channel?team=T123&id=C456",
            "https://slack.com/huddle/T123/C456",
            "https://app.slack.com.example/huddle/T123/C456",
            "https://example.com/huddle/T123/C456",
        ] {
            assert_eq!(
                external_huddle_url(&huddle(Some(link))),
                Err(HuddleFallbackError::SuppliedLink),
                "accepted {link}"
            );
        }
    }

    #[test]
    fn rejects_authority_query_and_fragment_ambiguities() {
        for link in [
            "",
            " https://app.slack.com/huddle/T123/C456",
            "https://app.slack.com/huddle/T123/C456 ",
            "https://app.slack.com/huddle/T123/C456\n",
            "https://user@app.slack.com/huddle/T123/C456",
            "https://user:password@app.slack.com/huddle/T123/C456",
            "https://app.slack.com:443/huddle/T123/C456",
            "https://app.slack.com:8443/huddle/T123/C456",
            "https://app.slack.com/huddle/T123/C456?join=1",
            "https://app.slack.com/huddle/T123/C456?",
            "https://app.slack.com/huddle/T123/C456#join",
            "https://app.slack.com/huddle/T123/C456#",
        ] {
            assert_eq!(
                external_huddle_url(&huddle(Some(link))),
                Err(HuddleFallbackError::SuppliedLink),
                "accepted {link}"
            );
        }
    }

    #[test]
    fn rejects_encoded_or_normalized_path_separators() {
        for link in [
            "https://app.slack.com/huddle/T123%2FC999/C456",
            "https://app.slack.com/huddle/T123/C456%2FC999",
            "https://app.slack.com/huddle/%54%31%32%33/C456",
            "https://app.slack.com/huddle/T123/%43%34%35%36",
            "https://app.slack.com/huddle//T123/C456",
            "https://app.slack.com/huddle/T123/../C456",
        ] {
            assert_eq!(
                external_huddle_url(&huddle(Some(link))),
                Err(HuddleFallbackError::SuppliedLink),
                "accepted {link}"
            );
        }
    }

    #[test]
    fn rejects_invalid_workspace_and_conversation_identifiers() {
        for team_id in ["", "T", "U123", "T-123", " T123", "T123 "] {
            let mut candidate = huddle(None);
            candidate.team_id = team_id.to_string();
            assert_eq!(
                external_huddle_url(&candidate),
                Err(HuddleFallbackError::TeamId),
                "accepted team id {team_id:?}"
            );
        }
        let mut oversized_team = huddle(None);
        oversized_team.team_id = format!("T{}", "1".repeat(64));
        assert_eq!(
            external_huddle_url(&oversized_team),
            Err(HuddleFallbackError::TeamId)
        );

        for channel_id in ["", "C", "U456", "C-456", " C456", "C456 "] {
            let mut candidate = huddle(None);
            candidate.channel_id = channel_id.to_string();
            assert_eq!(
                external_huddle_url(&candidate),
                Err(HuddleFallbackError::ChannelId),
                "accepted conversation id {channel_id:?}"
            );
        }
        let mut oversized_channel = huddle(None);
        oversized_channel.channel_id = format!("C{}", "1".repeat(64));
        assert_eq!(
            external_huddle_url(&oversized_channel),
            Err(HuddleFallbackError::ChannelId)
        );
    }

    #[test]
    fn supports_slack_channel_group_and_direct_message_identifiers() {
        for channel_id in ["C456", "G456", "D456"] {
            let mut candidate = huddle(None);
            candidate.channel_id = channel_id.to_string();
            assert_eq!(
                external_huddle_url(&candidate).unwrap(),
                format!("https://app.slack.com/huddle/T123/{channel_id}")
            );
        }
    }
}
