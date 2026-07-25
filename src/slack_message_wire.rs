use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::models::{SlackAttachment, SlackMessage};
use crate::rich_message::MESSAGE_CONTENT_VERSION;

pub(crate) struct SlackMessageWire {
    value: Value,
}

impl SlackMessageWire {
    pub(crate) fn from_value(value: Value) -> Self {
        Self { value }
    }

    pub(crate) fn into_message(self) -> Result<SlackMessage, serde_json::Error> {
        let mut value = self.value;
        let attachments = value
            .as_object_mut()
            .and_then(|object| {
                object.remove("author");
                object.remove("document");
                object.remove("content_version");
                object.remove("attachments")
            })
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|attachment| serde_json::from_value::<SlackAttachment>(attachment).ok())
            .collect::<Vec<_>>();
        let mut message = serde_json::from_value::<SlackMessage>(value)?;
        if !attachments.is_empty() {
            message.attachments = Some(attachments);
        }
        message.refresh_canonical_content();
        Ok(message)
    }
}

impl<'de> Deserialize<'de> for SlackMessageWire {
    fn deserialize<DeserializerT>(deserializer: DeserializerT) -> Result<Self, DeserializerT::Error>
    where
        DeserializerT: Deserializer<'de>,
    {
        Value::deserialize(deserializer).map(Self::from_value)
    }
}

pub(crate) fn normalize_cached_message(mut message: SlackMessage) -> SlackMessage {
    if message.content_version != MESSAGE_CONTENT_VERSION {
        message.refresh_canonical_content();
    }
    message.discard_wire_content();
    message
}

pub(crate) fn normalize_cached_messages(messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    messages.into_iter().map(normalize_cached_message).collect()
}

pub(crate) fn deserialize_message<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<SlackMessage, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    SlackMessageWire::deserialize(deserializer)?
        .into_message()
        .map_err(serde::de::Error::custom)
}

pub(crate) fn deserialize_messages<'de, DeserializerT>(
    deserializer: DeserializerT,
) -> Result<Vec<SlackMessage>, DeserializerT::Error>
where
    DeserializerT: Deserializer<'de>,
{
    let values = Vec::<Value>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .filter_map(|value| SlackMessageWire::from_value(value).into_message().ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct MessageEnvelope {
        #[serde(deserialize_with = "deserialize_messages")]
        messages: Vec<SlackMessage>,
    }

    #[test]
    fn web_api_message_lists_enter_the_canonical_boundary() {
        let envelope: MessageEnvelope = serde_json::from_value(serde_json::json!({
            "messages": [{
                "ts": "1710000000.000050",
                "bot_id": "B123",
                "bot_profile": {"name": "People assistant"},
                "attachments": [{"title": "Review request"}]
            }]
        }))
        .unwrap();

        assert_eq!(envelope.messages[0].author_label(), "People assistant");
        assert_eq!(envelope.messages[0].visible_text(), "Review request");
        assert_eq!(
            envelope.messages[0].content_version,
            MESSAGE_CONTENT_VERSION
        );
    }

    #[test]
    fn malformed_attachment_does_not_discard_valid_sibling() {
        let wire: SlackMessageWire = serde_json::from_value(serde_json::json!({
            "ts": "1710000000.000100",
            "attachments": [
                {"id": {"invalid": true}, "fallback": "Malformed"},
                {"title": "Valid sibling", "fallback": "Fallback"}
            ]
        }))
        .expect("wire envelope remains decodable");

        let message = wire.into_message().expect("message should normalize");

        assert_eq!(message.attachments.as_ref().map(Vec::len), Some(1));
        assert!(message.document.has_visible_content());
    }
}
