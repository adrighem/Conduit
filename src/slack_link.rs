pub const MAX_SLACK_URI_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackUri {
    team_id: Option<String>,
    target: SlackUriTarget,
}

impl SlackUri {
    pub fn team_id(&self) -> Option<&str> {
        self.team_id.as_deref()
    }

    pub fn target(&self) -> &SlackUriTarget {
        &self.target
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackUriTarget {
    Open,
    Channel(String),
    User(String),
    File {
        file_id: String,
        action: SlackFileAction,
    },
    App {
        app_id: String,
        tab: Option<SlackAppTab>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackAppTab {
    Home,
    About,
    Messages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackFileAction {
    View,
    Share,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SlackUriParseError {
    #[error("invalid Slack URI")]
    Invalid,
    #[error("unsupported external URI")]
    Unsupported,
}

pub fn parse_slack_uri(value: &str) -> Result<SlackUri, SlackUriParseError> {
    if value.len() > MAX_SLACK_URI_BYTES {
        return Err(SlackUriParseError::Invalid);
    }
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(
            if value
                .get(..value.len().min("slack:".len()))
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("slack:"))
            {
                SlackUriParseError::Invalid
            } else {
                SlackUriParseError::Unsupported
            },
        );
    }

    let url = url::Url::parse(value).map_err(|_| {
        if value
            .get(..value.len().min("slack:".len()))
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("slack:"))
        {
            SlackUriParseError::Invalid
        } else {
            SlackUriParseError::Unsupported
        }
    })?;
    if url.scheme() != "slack" {
        return Err(SlackUriParseError::Unsupported);
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !matches!(url.path(), "" | "/")
        || url.fragment().is_some()
    {
        return Err(SlackUriParseError::Invalid);
    }

    let action = url.host_str().ok_or(SlackUriParseError::Invalid)?;
    let team_id = single_query_parameter(&url, "team")?;
    let id = single_query_parameter(&url, "id")?;
    let tab = single_query_parameter(&url, "tab")?;
    if let Some(team_id) = team_id.as_deref() {
        validate_id(team_id, &['T'])?;
    }

    let target = match action {
        "open" => SlackUriTarget::Open,
        "channel" => {
            require_team(&team_id)?;
            let id = require_id(id, &['C', 'G', 'D'])?;
            SlackUriTarget::Channel(id)
        }
        "user" => {
            require_team(&team_id)?;
            let id = require_id(id, &['U', 'W'])?;
            SlackUriTarget::User(id)
        }
        "file" | "share-file" => {
            require_team(&team_id)?;
            let file_id = require_id(id, &['F'])?;
            SlackUriTarget::File {
                file_id,
                action: if action == "file" {
                    SlackFileAction::View
                } else {
                    SlackFileAction::Share
                },
            }
        }
        "app" => {
            require_team(&team_id)?;
            let app_id = require_id(id, &['A'])?;
            let tab = tab
                .as_deref()
                .map(|tab| match tab {
                    "home" => Ok(SlackAppTab::Home),
                    "about" => Ok(SlackAppTab::About),
                    "messages" => Ok(SlackAppTab::Messages),
                    _ => Err(SlackUriParseError::Invalid),
                })
                .transpose()?;
            SlackUriTarget::App { app_id, tab }
        }
        _ => SlackUriTarget::Open,
    };

    Ok(SlackUri { team_id, target })
}

fn single_query_parameter(
    url: &url::Url,
    name: &str,
) -> Result<Option<String>, SlackUriParseError> {
    let mut values = url
        .query_pairs()
        .filter_map(|(key, value)| (key == name).then(|| value.into_owned()));
    let value = values.next();
    if values.next().is_some() || value.as_deref().is_some_and(str::is_empty) {
        return Err(SlackUriParseError::Invalid);
    }
    Ok(value)
}

fn require_team(team_id: &Option<String>) -> Result<(), SlackUriParseError> {
    team_id
        .as_ref()
        .map(|_| ())
        .ok_or(SlackUriParseError::Invalid)
}

fn require_id(value: Option<String>, prefixes: &[char]) -> Result<String, SlackUriParseError> {
    let value = value.ok_or(SlackUriParseError::Invalid)?;
    validate_id(&value, prefixes)?;
    Ok(value)
}

fn validate_id(value: &str, prefixes: &[char]) -> Result<(), SlackUriParseError> {
    if value.len() < 2
        || value.len() > 64
        || !value.is_ascii()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
        || !value
            .chars()
            .next()
            .is_some_and(|prefix| prefixes.contains(&prefix))
    {
        return Err(SlackUriParseError::Invalid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: &str) -> SlackUri {
        parse_slack_uri(value).unwrap_or_else(|error| panic!("{value}: {error}"))
    }

    #[test]
    fn parses_every_documented_slack_uri_target() {
        let cases = [
            ("slack://open", SlackUriTarget::Open),
            ("slack://open?team=T123", SlackUriTarget::Open),
            (
                "slack://channel?team=T123&id=C456",
                SlackUriTarget::Channel("C456".into()),
            ),
            (
                "slack://channel?id=G456&team=T123",
                SlackUriTarget::Channel("G456".into()),
            ),
            (
                "slack://channel?team=T123&id=D456",
                SlackUriTarget::Channel("D456".into()),
            ),
            (
                "slack://user?team=T123&id=U456",
                SlackUriTarget::User("U456".into()),
            ),
            (
                "slack://user?team=T123&id=W456",
                SlackUriTarget::User("W456".into()),
            ),
            (
                "slack://file?team=T123&id=F456",
                SlackUriTarget::File {
                    file_id: "F456".into(),
                    action: SlackFileAction::View,
                },
            ),
            (
                "slack://share-file?team=T123&id=F456",
                SlackUriTarget::File {
                    file_id: "F456".into(),
                    action: SlackFileAction::Share,
                },
            ),
            (
                "slack://app?team=T123&id=A456",
                SlackUriTarget::App {
                    app_id: "A456".into(),
                    tab: None,
                },
            ),
            (
                "slack://app?id=A456&tab=home&team=T123",
                SlackUriTarget::App {
                    app_id: "A456".into(),
                    tab: Some(SlackAppTab::Home),
                },
            ),
            (
                "slack://app?tab=about&team=T123&id=A456",
                SlackUriTarget::App {
                    app_id: "A456".into(),
                    tab: Some(SlackAppTab::About),
                },
            ),
            (
                "slack://app?team=T123&id=A456&tab=messages",
                SlackUriTarget::App {
                    app_id: "A456".into(),
                    tab: Some(SlackAppTab::Messages),
                },
            ),
        ];

        for (value, expected) in cases {
            let uri = parse(value);
            assert_eq!(uri.target(), &expected, "{value}");
        }
        assert_eq!(parse("slack://open").team_id(), None);
        assert_eq!(parse("slack://open?team=T123").team_id(), Some("T123"));
    }

    #[test]
    fn ignores_unknown_parameters_for_forward_compatibility() {
        let uri = parse("slack://channel?future=value&id=C456&team=T123");

        assert_eq!(uri.team_id(), Some("T123"));
        assert_eq!(uri.target(), &SlackUriTarget::Channel("C456".into()));
    }

    #[test]
    fn unknown_actions_follow_slacks_open_fallback() {
        let uri = parse("slack://future-action?team=T123&id=C456");

        assert_eq!(uri.team_id(), Some("T123"));
        assert_eq!(uri.target(), &SlackUriTarget::Open);
    }

    #[test]
    fn rejects_non_slack_malformed_and_unsafe_uris() {
        let invalid = [
            "slack:open",
            "slack:///open",
            "slack://user:secret@channel?team=T123&id=C456",
            "slack://channel:42?team=T123&id=C456",
            "slack://channel/path?team=T123&id=C456",
            "slack://channel?team=T123&id=C456#fragment",
            "slack://channel?team=T123&id=C%2F456",
            "slack://channel?team=T123&id=C456&id=C789",
            "slack://channel?team=T123&team=T789&id=C456",
        ];

        for value in invalid {
            assert_eq!(
                parse_slack_uri(value),
                Err(SlackUriParseError::Invalid),
                "{value}"
            );
        }

        for value in ["", "not a URI", "https://slack.com/", "file:///tmp/link"] {
            assert_eq!(
                parse_slack_uri(value),
                Err(SlackUriParseError::Unsupported),
                "{value}"
            );
        }
    }

    #[test]
    fn validates_required_parameters_and_identifier_types() {
        let invalid = [
            "slack://open?team=X123",
            "slack://channel?id=C456",
            "slack://channel?team=T123",
            "slack://channel?team=T123&id=U456",
            "slack://channel?team=T123&id=C-456",
            "slack://user?team=T123&id=C456",
            "slack://file?team=T123&id=A456",
            "slack://share-file?team=T123&id=F-456",
            "slack://app?team=T123&id=F456",
            "slack://app?team=T123&id=A456&tab=invalid",
        ];

        for value in invalid {
            assert_eq!(
                parse_slack_uri(value),
                Err(SlackUriParseError::Invalid),
                "{value}"
            );
        }
    }

    #[test]
    fn rejects_oversized_uris() {
        let oversized = format!("slack://open?padding={}", "a".repeat(MAX_SLACK_URI_BYTES));

        assert_eq!(
            parse_slack_uri(&oversized),
            Err(SlackUriParseError::Invalid)
        );
    }
}
