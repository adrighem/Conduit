use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

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

/// One image asset the runtime wrote to the on-disk cache.
#[derive(Debug, Clone)]
struct CachedImageAsset {
    path: PathBuf,
    size: u64,
    inserted: u64,
}

/// Bounded map from an image source key (the Slack URL a row asks for) to
/// the cache file the runtime downloaded for it, scoped to the connected
/// workspace. Rows read the path directly; nothing re-derives it from keys.
/// When either bound is exceeded the oldest insertions are evicted.
#[derive(Debug)]
pub struct CachedImageAssets {
    cache_root: PathBuf,
    workspace_key: Option<String>,
    entries: HashMap<String, CachedImageAsset>,
    order: BTreeSet<(u64, String)>,
    total_bytes: u64,
    max_bytes: u64,
    max_entries: usize,
    clock: u64,
}

impl Default for CachedImageAssets {
    fn default() -> Self {
        Self::new(
            crate::config::image_asset_cache_dir(),
            CONDUIT_ASSET_REGISTRY_MAX_BYTES,
            CONDUIT_ASSET_REGISTRY_MAX_ENTRIES,
        )
    }
}

impl CachedImageAssets {
    pub fn new(cache_root: PathBuf, max_bytes: u64, max_entries: usize) -> Self {
        Self {
            cache_root,
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

    /// Records `descriptor` as the cached file for `key`. Returns the source
    /// keys evicted to stay within bounds, or `None` when the descriptor is
    /// rejected (no workspace, other workspace, or an out-of-range size).
    pub fn insert(
        &mut self,
        key: String,
        descriptor: CachedAssetDescriptor,
    ) -> Option<Vec<String>> {
        let workspace_key = self.workspace_key.as_deref()?;
        if descriptor.workspace_key() != workspace_key
            || descriptor.size() == 0
            || descriptor.size() > self.max_bytes
        {
            return None;
        }

        self.remove(&key);
        self.clock = self.clock.saturating_add(1);
        self.total_bytes = self.total_bytes.saturating_add(descriptor.size());
        self.order.insert((self.clock, key.clone()));
        self.entries.insert(
            key,
            CachedImageAsset {
                path: descriptor.path_in(&self.cache_root),
                size: descriptor.size(),
                inserted: self.clock,
            },
        );

        let mut evicted = Vec::new();
        while self.total_bytes > self.max_bytes || self.entries.len() > self.max_entries {
            let Some((_, oldest_key)) = self.order.pop_first() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest_key) {
                self.total_bytes = self.total_bytes.saturating_sub(entry.size);
                evicted.push(oldest_key);
            }
        }
        Some(evicted)
    }

    pub fn path(&self, key: &str) -> Option<&Path> {
        self.entries.get(key).map(|entry| entry.path.as_path())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Path)> {
        self.entries
            .iter()
            .map(|(key, entry)| (key, entry.path.as_path()))
    }

    pub fn remove(&mut self, key: &str) -> bool {
        let Some(entry) = self.entries.remove(key) else {
            return false;
        };
        self.total_bytes = self.total_bytes.saturating_sub(entry.size);
        self.order.remove(&(entry.inserted, key.to_string()));
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.total_bytes = 0;
        self.clock = 0;
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slack::PreviewAssetMime;

    const WORKSPACE: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    fn descriptor(workspace_key: &str, marker: char, size: u64) -> CachedAssetDescriptor {
        CachedAssetDescriptor::new(
            workspace_key.to_string(),
            marker.to_string().repeat(64),
            PreviewAssetMime::Png,
            size,
        )
        .unwrap()
    }

    fn assets(max_bytes: u64, max_entries: usize) -> CachedImageAssets {
        let mut assets = CachedImageAssets::new(PathBuf::from("/cache"), max_bytes, max_entries);
        assets.set_workspace(Some(WORKSPACE.to_string()));
        assets
    }

    #[test]
    fn cached_image_assets_map_source_keys_to_cache_paths() {
        let mut assets = assets(64, 4);
        assert_eq!(
            assets.insert("https://a".into(), descriptor(WORKSPACE, 'a', 4)),
            Some(Vec::new())
        );
        let expected = PathBuf::from(format!("/cache/{WORKSPACE}/{}.png", "a".repeat(64)));
        assert_eq!(assets.path("https://a"), Some(expected.as_path()));
        assert_eq!(assets.path("https://b"), None);
    }

    #[test]
    fn cached_image_assets_evict_oldest_by_bytes() {
        let mut assets = assets(8, 4);
        assets.insert("first".into(), descriptor(WORKSPACE, 'a', 4));
        assets.insert("second".into(), descriptor(WORKSPACE, 'b', 4));
        assert_eq!(
            assets.insert("third".into(), descriptor(WORKSPACE, 'c', 4)),
            Some(vec!["first".to_string()])
        );
        assert!(!assets.contains_key("first"));
        assert!(assets.contains_key("second"));
        assert!(assets.contains_key("third"));
        assert_eq!(assets.total_bytes(), 8);
    }

    #[test]
    fn cached_image_assets_cap_entry_count() {
        let mut assets = assets(64, 2);
        for marker in ['a', 'b', 'c'] {
            assets
                .insert(marker.to_string(), descriptor(WORKSPACE, marker, 3))
                .unwrap();
        }
        assert_eq!(assets.len(), 2);
        assert!(!assets.contains_key("a"));
        assert!(assets.contains_key("b"));
        assert!(assets.contains_key("c"));
    }

    #[test]
    fn cached_image_assets_reinsert_replaces_without_double_counting() {
        let mut assets = assets(64, 4);
        assets.insert("key".into(), descriptor(WORKSPACE, 'a', 4));
        assets.insert("key".into(), descriptor(WORKSPACE, 'a', 6));
        assert_eq!(assets.len(), 1);
        assert_eq!(assets.total_bytes(), 6);
    }

    #[test]
    fn cached_image_assets_reject_cross_workspace_and_unscoped_descriptors() {
        let mut assets = assets(16, 4);
        assert_eq!(
            assets.insert("key".into(), descriptor(&"b".repeat(64), 'c', 8)),
            None
        );
        assert_eq!(assets.total_bytes(), 0);

        assets.set_workspace(None);
        assert_eq!(
            assets.insert("key".into(), descriptor(WORKSPACE, 'c', 8)),
            None
        );
    }

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
