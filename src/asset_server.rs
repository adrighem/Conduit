use std::collections::{BTreeSet, HashMap};

use crate::message_html::CachedAssetSource;
use crate::runtime::CachedAssetDescriptor;

pub const CONDUIT_ASSET_REGISTRY_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const CONDUIT_ASSET_REGISTRY_MAX_ENTRIES: usize = 2_048;
pub const IMAGE_ASSET_SOURCE_MAX_BYTES: usize = 8 * 1024;
pub const IMAGE_ASSET_KEY_SET_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const IMAGE_ASSET_KEY_SET_MAX_ENTRIES: usize = 2_048;

#[derive(Debug, Default)]
pub struct BoundedImageAssetKeys {
    entries: HashMap<String, u64>,
    order: BTreeSet<(u64, String)>,
    total_bytes: usize,
    clock: u64,
}

impl BoundedImageAssetKeys {
    pub fn try_insert(&mut self, key: String) -> bool {
        if self.entries.contains_key(&key)
            || key.is_empty()
            || key.len() > IMAGE_ASSET_SOURCE_MAX_BYTES
            || self.entries.len() >= IMAGE_ASSET_KEY_SET_MAX_ENTRIES
            || self.total_bytes.saturating_add(key.len()) > IMAGE_ASSET_KEY_SET_MAX_BYTES
        {
            return false;
        }
        self.clock = self.clock.saturating_add(1);
        self.total_bytes = self.total_bytes.saturating_add(key.len());
        self.order.insert((self.clock, key.clone()));
        self.entries.insert(key, self.clock);
        true
    }

    pub fn insert_evicting(&mut self, key: String) -> bool {
        if self.entries.contains_key(&key)
            || key.is_empty()
            || key.len() > IMAGE_ASSET_SOURCE_MAX_BYTES
            || key.len() > IMAGE_ASSET_KEY_SET_MAX_BYTES
        {
            return false;
        }
        self.clock = self.clock.saturating_add(1);
        self.total_bytes = self.total_bytes.saturating_add(key.len());
        self.order.insert((self.clock, key.clone()));
        self.entries.insert(key.clone(), self.clock);
        while self.entries.len() > IMAGE_ASSET_KEY_SET_MAX_ENTRIES
            || self.total_bytes > IMAGE_ASSET_KEY_SET_MAX_BYTES
        {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some((removed_key, _)) = self.entries.remove_entry(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(removed_key.len());
            }
        }
        self.entries.contains_key(&key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub fn remove(&mut self, key: &str) -> bool {
        let Some((removed_key, clock)) = self.entries.remove_entry(key) else {
            return false;
        };
        self.total_bytes = self.total_bytes.saturating_sub(removed_key.len());
        self.order.remove(&(clock, removed_key));
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.total_bytes = 0;
        self.clock = 0;
    }

    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Debug, Clone)]
pub struct ConduitAssetEntry {
    pub descriptor: CachedAssetDescriptor,
    pub last_used: u64,
}

#[derive(Debug)]
pub struct BoundedConduitAssets {
    workspace_key: Option<String>,
    entries: HashMap<String, ConduitAssetEntry>,
    order: BTreeSet<(u64, String)>,
    total_bytes: u64,
    max_bytes: u64,
    max_entries: usize,
    clock: u64,
}

impl Default for BoundedConduitAssets {
    fn default() -> Self {
        Self::new(
            CONDUIT_ASSET_REGISTRY_MAX_BYTES,
            CONDUIT_ASSET_REGISTRY_MAX_ENTRIES,
        )
    }
}

impl BoundedConduitAssets {
    pub fn new(max_bytes: u64, max_entries: usize) -> Self {
        Self {
            workspace_key: None,
            entries: HashMap::new(),
            order: BTreeSet::new(),
            total_bytes: 0,
            max_bytes,
            max_entries,
            clock: 0,
        }
    }

    pub fn set_workspace(&mut self, workspace_key: Option<String>) {
        if self.workspace_key != workspace_key {
            self.clear();
            self.workspace_key = workspace_key;
        }
    }

    pub fn insert(&mut self, descriptor: CachedAssetDescriptor) -> Option<Vec<String>> {
        let workspace_key = self.workspace_key.as_deref()?;
        let cache_key = descriptor.cache_key().to_string();
        let uri = descriptor.uri();
        if descriptor.workspace_key() != workspace_key
            || descriptor.size() == 0
            || descriptor.size() > self.max_bytes
            || conduit_asset_request_key(&uri).as_deref() != Some(cache_key.as_str())
        {
            return None;
        }

        self.clock = self.clock.saturating_add(1);
        if let Some(replaced) = self.entries.remove(&cache_key) {
            self.total_bytes = self.total_bytes.saturating_sub(replaced.descriptor.size());
            self.order.remove(&(replaced.last_used, cache_key.clone()));
        }
        self.total_bytes = self.total_bytes.saturating_add(descriptor.size());
        self.order.insert((self.clock, cache_key.clone()));
        self.entries.insert(
            cache_key,
            ConduitAssetEntry {
                descriptor,
                last_used: self.clock,
            },
        );

        let mut evicted = Vec::new();
        while self.total_bytes > self.max_bytes || self.entries.len() > self.max_entries {
            let Some((_, oldest_key)) = self.order.pop_first() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest_key) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.descriptor.size());
                evicted.push(oldest_key);
            }
        }
        Some(evicted)
    }

    #[cfg(test)]
    pub fn get(&mut self, cache_key: &str) -> Option<CachedAssetDescriptor> {
        let workspace_key = self.workspace_key.as_deref()?;
        let entry = self.entries.get_mut(cache_key)?;
        if entry.descriptor.workspace_key() != workspace_key {
            return None;
        }
        let previous = entry.last_used;
        self.clock = self.clock.saturating_add(1);
        entry.last_used = self.clock;
        let descriptor = entry.descriptor.clone();
        self.order.remove(&(previous, cache_key.to_string()));
        self.order.insert((self.clock, cache_key.to_string()));
        Some(descriptor)
    }

    pub fn remove(&mut self, cache_key: &str) -> Option<CachedAssetDescriptor> {
        let entry = self.entries.remove(cache_key)?;
        self.total_bytes = self.total_bytes.saturating_sub(entry.descriptor.size());
        self.order.remove(&(entry.last_used, cache_key.to_string()));
        Some(entry.descriptor)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.total_bytes = 0;
        self.clock = 0;
    }

    pub fn contains_key(&self, cache_key: &str) -> bool {
        self.entries.contains_key(cache_key)
    }

    #[cfg(test)]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

pub fn conduit_asset_request_key(uri: &str) -> Option<String> {
    let parsed = url::Url::parse(uri).ok()?;
    if parsed.scheme() != "conduit-asset"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
        || !parsed.path().is_empty()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let key = parsed.host_str()?;
    let valid_key = key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    (valid_key && uri == format!("conduit-asset://{key}")).then(|| key.to_string())
}

pub fn cached_asset_source_is_registered(
    source: &CachedAssetSource,
    assets: &BoundedConduitAssets,
) -> bool {
    conduit_asset_request_key(source.uri()).is_some_and(|key| assets.contains_key(&key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_image_asset_keys_evicts_oldest_in_clock_order() {
        let mut keys = BoundedImageAssetKeys::default();
        for idx in 0..=IMAGE_ASSET_KEY_SET_MAX_ENTRIES {
            assert!(keys.insert_evicting(format!("https://files.slack.com/{idx}")));
        }
        assert_eq!(keys.len(), IMAGE_ASSET_KEY_SET_MAX_ENTRIES);
        assert!(!keys.contains("https://files.slack.com/0"));
        assert!(keys.contains("https://files.slack.com/1"));
        assert!(keys.contains(&format!(
            "https://files.slack.com/{IMAGE_ASSET_KEY_SET_MAX_ENTRIES}"
        )));
    }
}
