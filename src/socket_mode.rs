use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{Sink, SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::models::SlackMessage;

const CONNECTIONS_OPEN_URL: &str = "https://slack.com/api/apps.connections.open";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketModeDisconnect {
    ConnectionClosed,
    RefreshRequested,
    Warning,
    LinkDisabled,
    Unknown,
}

impl SocketModeDisconnect {
    fn from_reason(reason: Option<&str>) -> Self {
        match reason {
            Some("refresh_requested") => Self::RefreshRequested,
            Some("warning") => Self::Warning,
            Some("link_disabled") => Self::LinkDisabled,
            Some(_) => Self::Unknown,
            None => Self::ConnectionClosed,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SocketModeEvent {
    Message(Box<SocketModeMessageEvent>),
    Reaction(SocketModeReactionEvent),
    RefreshConversations,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketModeMessageKind {
    Posted,
    Changed,
    Deleted,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SocketModeMessageEvent {
    pub channel_id: String,
    pub message: SlackMessage,
    pub kind: SocketModeMessageKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SocketModeReactionEvent {
    pub channel_id: String,
    pub ts: String,
    pub name: String,
    pub user_id: String,
    pub added: bool,
}

pub async fn run_once(
    app_token: &str,
    mut handle_event: impl FnMut(SocketModeEvent),
) -> Result<SocketModeDisconnect> {
    let url = SocketModeApi::new(app_token).open_connection().await?;
    let (mut socket, _) = connect_async(&url)
        .await
        .context("failed to connect Slack Socket Mode WebSocket")?;

    while let Some(message) = socket.next().await {
        match message.context("failed to read Slack Socket Mode WebSocket message")? {
            Message::Text(text) => {
                if let Some(disconnect) =
                    handle_text_message(text.as_str(), &mut socket, &mut handle_event).await?
                {
                    return Ok(disconnect);
                }
            }
            Message::Binary(bytes) => {
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    if let Some(disconnect) =
                        handle_text_message(text, &mut socket, &mut handle_event).await?
                    {
                        return Ok(disconnect);
                    }
                }
            }
            Message::Ping(payload) => {
                socket
                    .send(Message::Pong(payload))
                    .await
                    .context("failed to send Slack Socket Mode pong")?;
            }
            Message::Close(_) => return Ok(SocketModeDisconnect::ConnectionClosed),
            Message::Pong(_) | Message::Frame(_) => {}
        }
    }

    Ok(SocketModeDisconnect::ConnectionClosed)
}

async fn handle_text_message<S>(
    text: &str,
    socket: &mut S,
    handle_event: &mut impl FnMut(SocketModeEvent),
) -> Result<Option<SocketModeDisconnect>>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let envelope: SocketModeEnvelope =
        serde_json::from_str(text).context("failed to parse Slack Socket Mode envelope")?;

    if let Some(envelope_id) = envelope.envelope_id.as_deref() {
        let ack = serde_json::json!({ "envelope_id": envelope_id }).to_string();
        socket
            .send(Message::Text(ack.into()))
            .await
            .context("failed to acknowledge Slack Socket Mode envelope")?;
    }

    match envelope.kind.as_str() {
        "hello" => {
            let approximate_connection_time = envelope
                .debug_info
                .as_ref()
                .and_then(|debug| debug.get("approximate_connection_time"))
                .and_then(Value::as_u64)
                .map(Duration::from_secs);
            crate::debug::log(
                "socket",
                &format!(
                    "SocketModeHello approximate_connection_time={}",
                    approximate_connection_time
                        .map(|duration| format!("{}s", duration.as_secs()))
                        .unwrap_or_else(|| "<unknown>".to_string())
                ),
            );
            Ok(None)
        }
        "disconnect" => Ok(Some(SocketModeDisconnect::from_reason(
            envelope.reason.as_deref(),
        ))),
        "events_api" => {
            if let Some(payload) = envelope.payload.as_ref() {
                if let Some(event) = socket_event_from_payload(payload) {
                    handle_event(event);
                }
            }
            Ok(None)
        }
        kind => {
            crate::debug::log("socket", &format!("SocketModeIgnoredEnvelope type={kind}"));
            Ok(None)
        }
    }
}

pub fn socket_event_from_payload(payload: &Value) -> Option<SocketModeEvent> {
    let event = payload.get("event")?;
    let event_type = event.get("type").and_then(Value::as_str)?;

    match event_type {
        "message" => message_event(event).map(|event| SocketModeEvent::Message(Box::new(event))),
        "reaction_added" => reaction_event(event, true).map(SocketModeEvent::Reaction),
        "reaction_removed" => reaction_event(event, false).map(SocketModeEvent::Reaction),
        event_type if conversation_refresh_event(event_type) => {
            Some(SocketModeEvent::RefreshConversations)
        }
        _ => None,
    }
}

fn message_event(event: &Value) -> Option<SocketModeMessageEvent> {
    let channel_id = event.get("channel").and_then(Value::as_str)?.to_string();
    let subtype = event.get("subtype").and_then(Value::as_str);

    match subtype {
        Some("message_changed" | "message_replied") => {
            let message = event.get("message")?;
            let message = serde_json::from_value::<SlackMessage>(message.clone()).ok()?;
            non_empty_message(channel_id, message, SocketModeMessageKind::Changed)
        }
        Some("message_deleted") => {
            let deleted_ts = event.get("deleted_ts").and_then(Value::as_str)?;
            let previous = event
                .get("previous_message")
                .cloned()
                .and_then(|value| serde_json::from_value::<SlackMessage>(value).ok());
            let message = SlackMessage {
                ts: deleted_ts.to_string(),
                subtype: Some("message_deleted".to_string()),
                user: previous.as_ref().and_then(|message| message.user.clone()),
                username: previous
                    .as_ref()
                    .and_then(|message| message.username.clone()),
                thread_ts: previous
                    .as_ref()
                    .and_then(|message| message.thread_ts.clone()),
                reactions: previous.and_then(|message| message.reactions),
                ..Default::default()
            };
            non_empty_message(channel_id, message, SocketModeMessageKind::Deleted)
        }
        _ => {
            let message = serde_json::from_value::<SlackMessage>(event.clone()).ok()?;
            non_empty_message(channel_id, message, SocketModeMessageKind::Posted)
        }
    }
}

fn non_empty_message(
    channel_id: String,
    message: SlackMessage,
    kind: SocketModeMessageKind,
) -> Option<SocketModeMessageEvent> {
    (!message.ts.trim().is_empty()).then_some(SocketModeMessageEvent {
        channel_id,
        message,
        kind,
    })
}

fn reaction_event(event: &Value, added: bool) -> Option<SocketModeReactionEvent> {
    let item = event.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }

    Some(SocketModeReactionEvent {
        channel_id: item.get("channel").and_then(Value::as_str)?.to_string(),
        ts: item.get("ts").and_then(Value::as_str)?.to_string(),
        name: event.get("reaction").and_then(Value::as_str)?.to_string(),
        user_id: event.get("user").and_then(Value::as_str)?.to_string(),
        added,
    })
}

fn conversation_refresh_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "channel_archive"
            | "channel_created"
            | "channel_deleted"
            | "channel_left"
            | "channel_rename"
            | "channel_unarchive"
            | "group_archive"
            | "group_joined"
            | "group_left"
            | "group_rename"
            | "group_unarchive"
            | "im_created"
            | "member_joined_channel"
            | "member_left_channel"
            | "mpim_open"
    )
}

#[derive(Debug, Clone)]
struct SocketModeApi {
    http: Client,
    app_token: String,
}

impl SocketModeApi {
    fn new(app_token: &str) -> Self {
        Self {
            http: Client::new(),
            app_token: app_token.to_string(),
        }
    }

    async fn open_connection(&self) -> Result<String> {
        let response = self
            .http
            .post(CONNECTIONS_OPEN_URL)
            .bearer_auth(&self.app_token)
            .send()
            .await
            .context("failed to call Slack apps.connections.open")?
            .error_for_status()
            .context("Slack apps.connections.open returned an HTTP error")?
            .json::<AppsConnectionsOpenResponse>()
            .await
            .context("failed to parse Slack apps.connections.open response")?;

        if response.ok {
            response
                .url
                .filter(|url| url.starts_with("wss://"))
                .ok_or_else(|| {
                    anyhow!("Slack apps.connections.open did not return a WebSocket URL")
                })
        } else {
            Err(anyhow!(
                "Slack apps.connections.open failed: {}",
                response.error.as_deref().unwrap_or("unknown_error")
            ))
        }
    }
}

#[derive(Debug, Deserialize)]
struct SocketModeEnvelope {
    #[serde(rename = "type")]
    kind: String,
    envelope_id: Option<String>,
    payload: Option<Value>,
    reason: Option<String>,
    debug_info: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AppsConnectionsOpenResponse {
    ok: bool,
    error: Option<String>,
    url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(event: Value) -> Value {
        serde_json::json!({ "event": event })
    }

    #[test]
    fn parses_message_events() {
        let event = socket_event_from_payload(&payload(serde_json::json!({
            "type": "message",
            "channel": "C123",
            "user": "U123",
            "text": "hello",
            "ts": "1710000000.000100"
        })));

        assert_eq!(
            event,
            Some(SocketModeEvent::Message(Box::new(SocketModeMessageEvent {
                channel_id: "C123".to_string(),
                kind: SocketModeMessageKind::Posted,
                message: SlackMessage {
                    kind: Some("message".to_string()),
                    user: Some("U123".to_string()),
                    text: Some("hello".to_string()),
                    ts: "1710000000.000100".to_string(),
                    ..Default::default()
                }
            })))
        );
    }

    #[test]
    fn parses_changed_and_deleted_message_events() {
        let changed = socket_event_from_payload(&payload(serde_json::json!({
            "type": "message",
            "subtype": "message_changed",
            "channel": "C123",
            "message": {
                "type": "message",
                "user": "U123",
                "text": "edited",
                "ts": "1710000000.000100"
            }
        })));
        let deleted = socket_event_from_payload(&payload(serde_json::json!({
            "type": "message",
            "subtype": "message_deleted",
            "channel": "C123",
            "deleted_ts": "1710000000.000100",
            "previous_message": {
                "type": "message",
                "user": "U123",
                "text": "old",
                "ts": "1710000000.000100"
            }
        })));

        assert!(matches!(
            changed,
            Some(SocketModeEvent::Message(event))
                if event.kind == SocketModeMessageKind::Changed
        ));
        assert!(matches!(
            deleted,
            Some(SocketModeEvent::Message(event))
                if event.kind == SocketModeMessageKind::Deleted
                    && event.message.subtype.as_deref() == Some("message_deleted")
        ));
    }

    #[test]
    fn parses_reaction_events() {
        let event = socket_event_from_payload(&payload(serde_json::json!({
            "type": "reaction_added",
            "user": "U123",
            "reaction": "thumbsup",
            "item": {
                "type": "message",
                "channel": "C123",
                "ts": "1710000000.000100"
            }
        })));

        assert_eq!(
            event,
            Some(SocketModeEvent::Reaction(SocketModeReactionEvent {
                channel_id: "C123".to_string(),
                ts: "1710000000.000100".to_string(),
                name: "thumbsup".to_string(),
                user_id: "U123".to_string(),
                added: true,
            }))
        );
    }

    #[test]
    fn maps_disconnect_reasons() {
        assert_eq!(
            SocketModeDisconnect::from_reason(Some("refresh_requested")),
            SocketModeDisconnect::RefreshRequested
        );
        assert_eq!(
            SocketModeDisconnect::from_reason(Some("link_disabled")),
            SocketModeDisconnect::LinkDisabled
        );
        assert_eq!(
            SocketModeDisconnect::from_reason(None),
            SocketModeDisconnect::ConnectionClosed
        );
    }
}
