use std::collections::hash_map::Entry;
use std::collections::HashMap;

use gtk::gio;
use gtk::gio::prelude::SettingsExt;
use gtk::glib::variant::ToVariant;

use crate::config;

const DRAFT_KEY_VERSION: &str = "v1";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DraftKey {
    workspace_id: String,
    channel_id: String,
    thread_ts: Option<String>,
}

impl DraftKey {
    pub fn new(workspace_id: &str, channel_id: &str, thread_ts: Option<&str>) -> Self {
        Self {
            workspace_id: workspace_id.to_string(),
            channel_id: channel_id.to_string(),
            thread_ts: thread_ts
                .filter(|thread_ts| !thread_ts.is_empty())
                .map(ToString::to_string),
        }
    }

    pub fn storage_key(&self) -> String {
        let workspace_id = urlencoding::encode(&self.workspace_id);
        let channel_id = urlencoding::encode(&self.channel_id);
        match self.thread_ts.as_deref() {
            Some(thread_ts) => format!(
                "{DRAFT_KEY_VERSION}/{workspace_id}/{channel_id}/thread/{}",
                urlencoding::encode(thread_ts)
            ),
            None => format!("{DRAFT_KEY_VERSION}/{workspace_id}/{channel_id}/channel"),
        }
    }

    pub fn from_storage_key(storage_key: &str) -> Option<Self> {
        let segments = storage_key.split('/').collect::<Vec<_>>();
        let (workspace_id, channel_id, thread_ts) = match segments.as_slice() {
            [version, workspace_id, channel_id, "channel"] if *version == DRAFT_KEY_VERSION => {
                (*workspace_id, *channel_id, None)
            }
            [version, workspace_id, channel_id, "thread", thread_ts]
                if *version == DRAFT_KEY_VERSION =>
            {
                (*workspace_id, *channel_id, Some(*thread_ts))
            }
            _ => return None,
        };

        let workspace_id = decode_key_segment(workspace_id)?;
        let channel_id = decode_key_segment(channel_id)?;
        let thread_ts = match thread_ts {
            Some(thread_ts) => Some(decode_key_segment(thread_ts)?),
            None => None,
        };
        if workspace_id.is_empty()
            || channel_id.is_empty()
            || thread_ts.as_ref().is_some_and(String::is_empty)
        {
            return None;
        }

        Some(Self {
            workspace_id,
            channel_id,
            thread_ts,
        })
    }
}

fn decode_key_segment(segment: &str) -> Option<String> {
    urlencoding::decode(segment)
        .ok()
        .map(|value| value.into_owned())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drafts {
    entries: HashMap<DraftKey, String>,
}

impl Drafts {
    pub fn from_persisted(persisted: HashMap<String, String>) -> Self {
        let entries = persisted
            .into_iter()
            .filter(|(_, text)| !text.trim().is_empty())
            .filter_map(|(storage_key, text)| {
                DraftKey::from_storage_key(&storage_key).map(|key| (key, text))
            })
            .collect();
        Self { entries }
    }

    pub fn to_persisted(&self) -> HashMap<String, String> {
        self.entries
            .iter()
            .map(|(key, text)| (key.storage_key(), text.clone()))
            .collect()
    }

    pub fn get(&self, key: &DraftKey) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    pub fn upsert(&mut self, key: DraftKey, text: &str) -> bool {
        if text.trim().is_empty() {
            return self.remove(&key);
        }

        match self.entries.entry(key) {
            Entry::Occupied(mut entry) if entry.get() != text => {
                entry.insert(text.to_string());
                true
            }
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(text.to_string());
                true
            }
        }
    }

    pub fn remove(&mut self, key: &DraftKey) -> bool {
        self.entries.remove(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone)]
pub struct DraftSettings {
    settings: gio::Settings,
}

impl DraftSettings {
    pub fn new(settings: gio::Settings) -> Self {
        Self { settings }
    }

    pub fn load(&self) -> Drafts {
        let persisted = self
            .settings
            .value(config::MESSAGE_DRAFTS_KEY)
            .get::<HashMap<String, String>>()
            .unwrap_or_default();
        Drafts::from_persisted(persisted)
    }

    pub fn save(&self, drafts: &Drafts) -> Result<(), gtk::glib::BoolError> {
        self.settings.set_value(
            config::MESSAGE_DRAFTS_KEY,
            &drafts.to_persisted().to_variant(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn keys_isolate_workspaces_channels_and_threads() {
        let channel = DraftKey::new("T1:U1", "C1", None);
        let other_workspace = DraftKey::new("T2:U1", "C1", None);
        let other_channel = DraftKey::new("T1:U1", "C2", None);
        let thread = DraftKey::new("T1:U1", "C1", Some("1710000000.000100"));

        assert_ne!(channel, other_workspace);
        assert_ne!(channel, other_channel);
        assert_ne!(channel, thread);
        assert_ne!(other_channel, thread);
        assert_eq!(
            DraftKey::new("T1:U1", "C1", Some("")),
            DraftKey::new("T1:U1", "C1", None)
        );
    }

    #[test]
    fn storage_keys_round_trip_reserved_characters_without_collisions() {
        assert_eq!(
            DraftKey::new("T1:U1", "C1", Some("1710000000.000100")).storage_key(),
            "v1/T1%3AU1/C1/thread/1710000000.000100"
        );

        for key in [
            DraftKey::new("T/1%U", "C/1", None),
            DraftKey::new("T/1%U", "C/1", Some("171/0%1")),
        ] {
            let encoded = key.storage_key();

            assert_eq!(DraftKey::from_storage_key(&encoded), Some(key));
        }
    }

    #[test]
    fn persisted_map_round_trips_updates_and_removes_drafts() {
        let channel = DraftKey::new("T1:U1", "C1", None);
        let thread = DraftKey::new("T1:U1", "C1", Some("1710000000.000100"));
        let mut drafts = Drafts::default();

        assert!(drafts.upsert(channel.clone(), "channel draft"));
        assert!(drafts.upsert(thread.clone(), "thread draft"));
        assert_eq!(drafts.get(&channel), Some("channel draft"));
        assert_eq!(drafts.get(&thread), Some("thread draft"));

        assert!(drafts.upsert(channel.clone(), "updated draft"));
        assert!(!drafts.upsert(channel.clone(), "updated draft"));
        assert_eq!(drafts.get(&channel), Some("updated draft"));

        let restored = Drafts::from_persisted(drafts.to_persisted());
        assert_eq!(restored, drafts);

        assert!(drafts.remove(&thread));
        assert!(!drafts.remove(&thread));
        assert_eq!(drafts.get(&thread), None);
    }

    #[test]
    fn empty_drafts_delete_existing_entries() {
        let key = DraftKey::new("T1:U1", "C1", None);
        let mut drafts = Drafts::default();
        drafts.upsert(key.clone(), "keep me");

        assert!(drafts.upsert(key.clone(), "  \n"));
        assert_eq!(drafts.get(&key), None);
        assert!(!drafts.upsert(key, ""));
        assert!(drafts.is_empty());
    }

    #[test]
    fn loading_ignores_malformed_keys_and_blank_values() {
        let key = DraftKey::new("T1:U1", "C1", None);
        let persisted = HashMap::from([
            (key.storage_key(), "valid".to_string()),
            ("not-a-draft-key".to_string(), "invalid".to_string()),
            ("v1/%FF/C1/channel".to_string(), "invalid UTF-8".to_string()),
            (
                DraftKey::new("T1:U1", "C2", None).storage_key(),
                "  ".to_string(),
            ),
        ]);

        let drafts = Drafts::from_persisted(persisted);

        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts.get(&key), Some("valid"));
    }
}
