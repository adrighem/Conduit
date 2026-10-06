use std::collections::HashMap;

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
        self.entries.insert(key.clone(), self.clock);
        while self.entries.len() > IMAGE_ASSET_KEY_SET_MAX_ENTRIES
            || self.total_bytes > IMAGE_ASSET_KEY_SET_MAX_BYTES
        {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by(|(left_key, left_clock), (right_key, right_clock)| {
                    left_clock
                        .cmp(right_clock)
                        .then_with(|| left_key.cmp(right_key))
                })
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.remove(&oldest);
        }
        self.entries.contains_key(&key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub fn remove(&mut self, key: &str) -> bool {
        let Some((key, _)) = self.entries.remove_entry(key) else {
            return false;
        };
        self.total_bytes = self.total_bytes.saturating_sub(key.len());
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
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
        }
        self.total_bytes = self.total_bytes.saturating_add(descriptor.size());
        self.entries.insert(
            cache_key,
            ConduitAssetEntry {
                descriptor,
                last_used: self.clock,
            },
        );

        let mut evicted = Vec::new();
        while self.total_bytes > self.max_bytes || self.entries.len() > self.max_entries {
            let Some(oldest_key) = self
                .entries
                .iter()
                .min_by(|(left_key, left), (right_key, right)| {
                    left.last_used
                        .cmp(&right.last_used)
                        .then_with(|| left_key.cmp(right_key))
                })
                .map(|(key, _)| key.clone())
            else {
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
        self.clock = self.clock.saturating_add(1);
        entry.last_used = self.clock;
        Some(entry.descriptor.clone())
    }

    pub fn remove(&mut self, cache_key: &str) -> Option<CachedAssetDescriptor> {
        let entry = self.entries.remove(cache_key)?;
        self.total_bytes = self.total_bytes.saturating_sub(entry.descriptor.size());
        Some(entry.descriptor)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
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
