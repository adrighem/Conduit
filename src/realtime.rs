use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::runtime::RuntimeEvent;

#[derive(Clone)]
pub struct SocketModeClient {
    http: Client,
    app_token: String,
}

impl SocketModeClient {
    pub fn new(app_token: String) -> Self {
        Self {
            http: Client::new(),
            app_token,
        }
    }

    pub async fn run(self, events: Sender<RuntimeEvent>) {
        loop {
            if let Err(error) = self.connect_once(&events).await {
                let _ = events.send(RuntimeEvent::RealtimeStatus(format!(
                    "Realtime disconnected: {error}"
                )));
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    async fn connect_once(&self, events: &Sender<RuntimeEvent>) -> Result<()> {
        let url = self.open_socket_url().await?;
        let _ = events.send(RuntimeEvent::RealtimeStatus(
            "Connecting realtime".to_string(),
        ));
        let (mut websocket, _) = connect_async(url)
            .await
            .context("failed to connect Slack Socket Mode websocket")?;
        let _ = events.send(RuntimeEvent::RealtimeStarted);

        while let Some(message) = websocket.next().await {
            let message = message.context("failed to receive realtime websocket message")?;
            let Message::Text(text) = message else {
                continue;
            };
            let envelope: Value =
                serde_json::from_str(&text).context("failed to parse Socket Mode envelope")?;

            if let Some(envelope_id) = envelope.get("envelope_id").and_then(|id| id.as_str()) {
                websocket
                    .send(Message::Text(
                        json!({ "envelope_id": envelope_id }).to_string().into(),
                    ))
                    .await
                    .context("failed to acknowledge Socket Mode envelope")?;
            }

            if let Some(channel_id) = event_channel(&envelope) {
                let _ = events.send(RuntimeEvent::RealtimeMessage { channel_id });
            }
        }

        Err(anyhow!("Slack closed the realtime websocket"))
    }

    async fn open_socket_url(&self) -> Result<String> {
        let response = self
            .http
            .post("https://slack.com/api/apps.connections.open")
            .bearer_auth(&self.app_token)
            .send()
            .await
            .context("failed to open Slack Socket Mode connection")?
            .error_for_status()
            .context("Slack Socket Mode endpoint returned an HTTP error")?
            .json::<SocketOpenResponse>()
            .await
            .context("failed to parse Socket Mode connection response")?;

        if !response.ok {
            return Err(anyhow!(
                "Slack Socket Mode connection failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown_error".to_string())
            ));
        }

        response
            .url
            .ok_or_else(|| anyhow!("Slack did not return a Socket Mode websocket URL"))
    }
}

fn event_channel(envelope: &Value) -> Option<String> {
    envelope
        .pointer("/payload/event/channel")
        .and_then(|channel| channel.as_str())
        .map(ToString::to_string)
        .or_else(|| {
            envelope
                .pointer("/payload/event/item/channel")
                .and_then(|channel| channel.as_str())
                .map(ToString::to_string)
        })
}

#[derive(Debug, Deserialize)]
struct SocketOpenResponse {
    ok: bool,
    url: Option<String>,
    error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_message_channel() {
        let envelope = json!({
            "payload": {
                "event": {
                    "type": "message",
                    "channel": "C123"
                }
            }
        });

        assert_eq!(event_channel(&envelope).as_deref(), Some("C123"));
    }

    #[test]
    fn extracts_item_channel() {
        let envelope = json!({
            "payload": {
                "event": {
                    "type": "reaction_added",
                    "item": {
                        "channel": "C456"
                    }
                }
            }
        });

        assert_eq!(event_channel(&envelope).as_deref(), Some("C456"));
    }
}
