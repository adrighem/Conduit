use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::message_html::MessageHtmlContext;

pub(crate) const MAX_TEXTURE_CACHE_SIZE: usize = 256;

pub(crate) type CacheEntry = gtk::gdk::Texture;

pub(crate) struct BoundedTextureCache {
    entries: HashMap<PathBuf, CacheEntry>,
    order: VecDeque<PathBuf>,
}

impl BoundedTextureCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub(crate) fn get(&mut self, path: &Path) -> Option<CacheEntry> {
        if let Some(entry) = self.entries.get(path) {
            if let Some(pos) = self.order.iter().position(|p| p == path) {
                self.order.remove(pos);
            }
            self.order.push_back(path.to_path_buf());
            return Some(entry.clone());
        }
        None
    }

    pub(crate) fn insert(&mut self, path: PathBuf, value: CacheEntry) {
        if self.entries.contains_key(&path) {
            self.entries.insert(path.clone(), value);
            if let Some(pos) = self.order.iter().position(|p| p == &path) {
                self.order.remove(pos);
            }
            self.order.push_back(path);
        } else {
            if self.entries.len() >= MAX_TEXTURE_CACHE_SIZE {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
            self.entries.insert(path.clone(), value);
            self.order.push_back(path);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

thread_local! {
    pub(crate) static TEXTURE_CACHE: RefCell<BoundedTextureCache> =
        RefCell::new(BoundedTextureCache::new());
    // Reaction emoji pre-scaled to 64px (2x HiDPI density for 32x32 request). Kept apart from TEXTURE_CACHE, which
    // stores full-size textures for the same paths (inline emoji, media): a
    // shared cache hands whichever size loaded first to the other caller.
    pub(crate) static REACTION_TEXTURE_CACHE: RefCell<BoundedTextureCache> =
        RefCell::new(BoundedTextureCache::new());
    // Collapsed image/video attachments, in-memory only (reset on app restart).
    // Message rows are fully rebuilt on every scroll-recycle (no connect_unbind),
    // so this state cannot live on the transient widget - it must survive here,
    // keyed by a stable ts+index pair.
    pub(crate) static COLLAPSED_MEDIA: RefCell<HashSet<String>> =
        RefCell::new(HashSet::new());
}

pub fn clear_timeline_view_caches() {
    COLLAPSED_MEDIA.with(|set| set.borrow_mut().clear());
    TEXTURE_CACHE.with(|cache| cache.borrow_mut().clear());
    REACTION_TEXTURE_CACHE.with(|cache| cache.borrow_mut().clear());
    crate::animated_image::clear_caches();
    crate::timeline_document_widget::clear_document_accent_cache();
}

pub(crate) fn get_or_load_texture(path: &Path) -> Option<gtk::gdk::Texture> {
    TEXTURE_CACHE.with(|cache| {
        let mut map = cache.borrow_mut();
        if let Some(tex) = map.get(path) {
            return Some(tex);
        }
        if let Ok(tex) = gtk::gdk::Texture::from_filename(path) {
            map.insert(path.to_path_buf(), tex.clone());
            return Some(tex);
        }
        None
    })
}

pub(crate) fn media_collapse_key(ts: &str, slot: &str) -> String {
    format!("{ts}:{slot}")
}

pub(crate) fn is_media_collapsed(ts: &str, slot: &str) -> bool {
    let key = media_collapse_key(ts, slot);
    COLLAPSED_MEDIA.with(|set| set.borrow().contains(&key))
}

pub(crate) fn set_media_collapsed(ts: &str, slot: &str, collapsed: bool) {
    let key = media_collapse_key(ts, slot);
    COLLAPSED_MEDIA.with(|set| {
        if collapsed {
            set.borrow_mut().insert(key);
        } else {
            set.borrow_mut().remove(&key);
        }
    });
}

const EXTENSIONS: &[&str] = &[
    "", ".png", ".gif", ".jpg", ".jpeg", ".webp", ".mp4", ".webm", ".mov", ".ogg", ".avif",
];

fn check_file_extensions(base_dir: &Path, key: &str) -> Option<PathBuf> {
    for ext in EXTENSIONS {
        let p = base_dir.join(format!("{key}{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub(crate) fn resolve_cached_asset_path(
    url: &str,
    context: &MessageHtmlContext,
) -> Option<PathBuf> {
    let source_uri = context
        .image_assets
        .get(url)
        .map(|s| s.uri())
        .unwrap_or(url);

    let cache_key = source_uri
        .strip_prefix("conduit-asset://")
        .or_else(|| source_uri.strip_prefix("conduit-cache://"));

    let cache_dir = crate::config::image_asset_cache_dir();

    if let Some(cache_key) = cache_key {
        let clean_key = cache_key
            .split('?')
            .next()
            .unwrap_or(cache_key)
            .trim_matches('/');
        if !clean_key.is_empty() {
            if let Some(path) = check_file_extensions(&cache_dir, clean_key) {
                return Some(path);
            }

            if let Ok(entries) = std::fs::read_dir(&cache_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        if let Some(sub_path) = check_file_extensions(&path, clean_key) {
                            return Some(sub_path);
                        }
                    }
                }
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(workspace_dir_name) = entry.file_name().to_str() {
                    let hash = {
                        use sha2::{Digest, Sha256};
                        let mut hasher = Sha256::new();
                        hasher.update(workspace_dir_name.as_bytes());
                        hasher.update([0]);
                        hasher.update(url.as_bytes());
                        format!("{:x}", hasher.finalize())
                    };

                    if let Some(sub_path) = check_file_extensions(&path, &hash) {
                        return Some(sub_path);
                    }
                }
            }
        }
    }

    None
}
