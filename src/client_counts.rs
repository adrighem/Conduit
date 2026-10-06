/* client_counts.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

//! Defensive parsing of Slack's undocumented `client.counts` payload into
//! the read-state baseline consumed by `crate::unread_ledger`.

use serde::Deserialize;
use serde_json::Value;

use crate::unread_ledger::ServerReadCounts;

/// The conversation arrays of a `client.counts` response. Items stay raw
/// JSON so one malformed entry cannot reject the whole snapshot.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ClientCountsPayload {
    #[serde(default)]
    channels: Vec<Value>,
    #[serde(default)]
    mpims: Vec<Value>,
    #[serde(default)]
    ims: Vec<Value>,
}

impl ClientCountsPayload {
    pub(crate) fn server_read_counts(&self) -> Vec<ServerReadCounts> {
        let channels = self.channels.iter().map(|entry| (entry, false));
        let direct = self
            .mpims
            .iter()
            .chain(&self.ims)
            .map(|entry| (entry, true));
        channels
            .chain(direct)
            .filter_map(|(entry, is_direct)| client_count_entry(entry, is_direct))
            .collect()
    }
}

fn text_field(entry: &Value, key: &str) -> Option<String> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn count_field(entry: &Value, key: &str) -> u64 {
    entry
        .get(key)
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        })
        .unwrap_or_default()
}

/// DMs may report `dm_count` instead of (or next to) `mention_count`; every
/// DM message badges, so the larger of the two is the badge.
fn client_count_entry(entry: &Value, is_direct: bool) -> Option<ServerReadCounts> {
    let channel_id = text_field(entry, "id")?;
    let mention_count = count_field(entry, "mention_count");
    let dm_count = count_field(entry, "dm_count");
    let badge_count = if is_direct {
        mention_count.max(dm_count)
    } else {
        mention_count
    };
    Some(ServerReadCounts {
        channel_id,
        last_read: text_field(entry, "last_read"),
        latest: text_field(entry, "latest"),
        mention_count: badge_count,
        unread_count: if is_direct { dm_count } else { 0 },
        has_unreads: entry
            .get("has_unreads")
            .and_then(Value::as_bool)
            .unwrap_or(badge_count > 0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channels_mpims_and_ims_with_dm_count() {
        let payload: ClientCountsPayload = serde_json::from_value(serde_json::json!({
            "ok": true,
            "channels": [
                {"id": "C1", "last_read": "1.0", "latest": "3.0", "mention_count": 2,
                 "has_unreads": true},
                {"id": "C2", "last_read": "4.0", "latest": "4.0", "mention_count": 0,
                 "has_unreads": false},
                {"last_read": "1.0"}
            ],
            "mpims": [
                {"id": "G1", "last_read": "1.0", "latest": "2.0", "mention_count": "3"}
            ],
            "ims": [
                {"id": "D1", "last_read": "1.0", "latest": "5.0", "dm_count": 4,
                 "mention_count": 1, "has_unreads": true}
            ]
        }))
        .expect("client.counts payload deserializes");

        let counts = payload.server_read_counts();
        assert_eq!(
            counts
                .iter()
                .map(|entry| entry.channel_id.as_str())
                .collect::<Vec<_>>(),
            vec!["C1", "C2", "G1", "D1"]
        );
        assert_eq!(counts[0].mention_count, 2);
        assert_eq!(counts[0].latest.as_deref(), Some("3.0"));
        assert!(!counts[1].has_unreads);
        assert_eq!(counts[2].mention_count, 3);
        assert!(counts[2].has_unreads);
        assert_eq!(counts[3].mention_count, 4);
        assert_eq!(counts[3].unread_count, 4);
        assert_eq!(counts[3].last_read.as_deref(), Some("1.0"));
    }

    #[test]
    fn missing_arrays_yield_an_empty_snapshot() {
        let payload: ClientCountsPayload =
            serde_json::from_value(serde_json::json!({"ok": true})).expect("deserializes");
        assert!(payload.server_read_counts().is_empty());
    }
}
