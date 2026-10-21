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
        if !crate::animated_image::within_decode_budget(path) {
            return None;
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
    ".png", ".gif", ".jpg", ".jpeg", ".webp", ".avif", ".mp4", ".webm", ".mov", ".ogv", ".ogg",
];

/// Cache keys are file stems (hex digests in practice). Anything carrying a
/// path separator, `.`/`..` segment or other punctuation is not a key we wrote.
fn is_safe_cache_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Returns the resolved path of `candidate` only when it is a regular file
/// that still lives inside `cache_root` after symlinks are followed.
/// `cache_root` must already be canonical.
fn contained_cache_file(cache_root: &Path, candidate: &Path) -> Option<PathBuf> {
    let resolved = candidate.canonicalize().ok()?;
    (resolved.starts_with(cache_root) && resolved.is_file()).then_some(resolved)
}

fn check_file_extensions(cache_root: &Path, base_dir: &Path, key: &str) -> Option<PathBuf> {
    if !is_safe_cache_key(key) {
        return None;
    }
    EXTENSIONS
        .iter()
        .find_map(|ext| contained_cache_file(cache_root, &base_dir.join(format!("{key}{ext}"))))
}

/// Maps a Slack-provided URL (or a `conduit-asset://` URI registered for it)
/// to a file inside the image asset cache. Never returns a path outside the
/// cache directory, so untrusted message data cannot point the UI at
/// arbitrary local files (`/dev/zero`, `~/.ssh/...`, FIFOs).
pub(crate) fn resolve_cached_asset_path(
    url: &str,
    context: &MessageHtmlContext,
) -> Option<PathBuf> {
    resolve_cached_asset_path_in(&crate::config::image_asset_cache_dir(), url, context)
}

fn resolve_cached_asset_path_in(
    cache_dir: &Path,
    url: &str,
    context: &MessageHtmlContext,
) -> Option<PathBuf> {
    let cache_root = cache_dir.canonicalize().ok()?;
    let source_uri = context
        .image_assets
        .get(url)
        .map(|s| s.uri())
        .unwrap_or(url);

    let cache_key = source_uri
        .strip_prefix("conduit-asset://")
        .or_else(|| source_uri.strip_prefix("conduit-cache://"));

    if let Some(cache_key) = cache_key {
        let clean_key = cache_key
            .split('?')
            .next()
            .unwrap_or(cache_key)
            .trim_matches('/');
        if is_safe_cache_key(clean_key) {
            if let Some(path) = check_file_extensions(&cache_root, &cache_root, clean_key) {
                return Some(path);
            }

            if let Ok(entries) = std::fs::read_dir(&cache_root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        if let Some(sub_path) = check_file_extensions(&cache_root, &path, clean_key)
                        {
                            return Some(sub_path);
                        }
                    }
                }
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(&cache_root) {
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

                    if let Some(sub_path) = check_file_extensions(&cache_root, &path, &hash) {
                        return Some(sub_path);
                    }
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use crate::timeline::tests::test_context;

    /// Lays out `<root>/cache` plus a sibling `<root>/secret.png` the
    /// resolver must never reach.
    struct Fixture {
        root: PathBuf,
        cache: PathBuf,
        secret: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("conduit-cache-path-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let cache = root.join("cache");
            std::fs::create_dir_all(cache.join("workspace")).unwrap();
            let secret = root.join("secret.png");
            std::fs::write(&secret, b"secret").unwrap();
            Self {
                root,
                cache,
                secret,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn resolves_key_inside_cache_dir() {
        let fx = Fixture::new("inside");
        let key = "a".repeat(64);
        let file = fx.cache.join("workspace").join(format!("{key}.png"));
        std::fs::write(&file, b"img").unwrap();
        let found = resolve_cached_asset_path_in(
            &fx.cache,
            &format!("conduit-asset://{key}"),
            &test_context(),
        );
        assert_eq!(found, Some(file.canonicalize().unwrap()));
    }

    #[test]
    fn rejects_dot_dot_segments_in_asset_uri() {
        let fx = Fixture::new("dotdot");
        let ctx = test_context();
        for uri in [
            "conduit-asset://../secret",
            "conduit-asset://workspace/../../secret",
            "conduit-cache://../secret",
            "conduit-asset://..",
        ] {
            assert_eq!(
                resolve_cached_asset_path_in(&fx.cache, uri, &ctx),
                None,
                "{uri}"
            );
        }
        assert!(fx.secret.is_file());
    }

    #[test]
    fn rejects_bare_local_paths_outside_cache_dir() {
        let fx = Fixture::new("outside");
        let ctx = test_context();
        let secret = fx.secret.to_str().unwrap();
        assert_eq!(resolve_cached_asset_path_in(&fx.cache, secret, &ctx), None);
        assert_eq!(
            resolve_cached_asset_path_in(&fx.cache, "/dev/zero", &ctx),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_cache_dir() {
        let fx = Fixture::new("symlink");
        std::os::unix::fs::symlink(&fx.secret, fx.cache.join("escape.png")).unwrap();
        let found =
            resolve_cached_asset_path_in(&fx.cache, "conduit-asset://escape", &test_context());
        assert_eq!(found, None);
    }

    #[test]
    fn rejects_extensionless_cache_entries() {
        let fx = Fixture::new("noext");
        std::fs::write(fx.cache.join("bare"), b"img").unwrap();
        let found =
            resolve_cached_asset_path_in(&fx.cache, "conduit-asset://bare", &test_context());
        assert_eq!(found, None);
    }

    #[test]
    fn contained_cache_file_requires_regular_file_under_root() {
        let fx = Fixture::new("contained");
        let root = fx.cache.canonicalize().unwrap();
        assert_eq!(contained_cache_file(&root, &fx.secret), None);
        assert_eq!(contained_cache_file(&root, Path::new("/dev/zero")), None);
        assert_eq!(
            contained_cache_file(&root, &fx.cache.join("workspace")),
            None
        );
    }
}
