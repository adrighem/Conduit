use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// How long the in-memory snapshot of the asset cache directory is trusted
/// before a lookup rebuilds it. Row binds hammer `resolve_cached_asset_path`
/// (once per visible row, on every scroll/recycle); without this, each call
/// re-walked the cache directory and probed every known extension for every
/// candidate path. A short TTL turns that into one directory walk every few
/// seconds. Misses are never trusted from the snapshot alone (see
/// [`probe_cache_key`]), so a freshly downloaded asset resolves immediately.
const RESOLVED_ASSET_INDEX_TTL: Duration = Duration::from_secs(2);

/// One-level-deep snapshot of the asset cache directory: every file stem
/// (the content-addressed cache key, sans extension) mapped to its
/// *unverified* on-disk path, plus the top-level directory names (candidate
/// workspace keys) used to probe URL-hash based lookups. Building it is just
/// `read_dir` calls; the containment check runs per lookup hit instead, so a
/// rebuild costs no per-file syscalls.
struct ResolvedAssetIndex {
    cache_root: PathBuf,
    built_at: Instant,
    entries: HashMap<String, PathBuf>,
    workspace_dirs: Vec<String>,
}

impl ResolvedAssetIndex {
    /// Returns the indexed path for `key` only if it still passes the
    /// containment check (regular file inside the cache root after symlinks).
    fn verified(&self, key: &str) -> Option<PathBuf> {
        let path = self.entries.get(key)?;
        contained_cache_file(&self.cache_root, path)
    }
}

fn index_cache_file(path: PathBuf, entries: &mut HashMap<String, PathBuf>) {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let Some(stem) = EXTENSIONS.iter().find_map(|ext| name.strip_suffix(ext)) else {
        return;
    };
    if is_safe_cache_key(stem) {
        entries.insert(stem.to_string(), path);
    }
}

/// Top-level subdirectories of `cache_root`. The root only holds one
/// directory per workspace, so this listing is tiny.
fn cache_workspace_dirs(cache_root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(root_entries) = std::fs::read_dir(cache_root) else {
        return Vec::new();
    };
    root_entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_str()?.to_string();
            path.is_dir().then_some((name, path))
        })
        .collect()
}

fn build_resolved_asset_index(cache_root: &Path) -> ResolvedAssetIndex {
    #[cfg(test)]
    INDEX_BUILDS.with(|builds| builds.set(builds.get() + 1));
    let mut entries = HashMap::new();
    if let Ok(root_entries) = std::fs::read_dir(cache_root) {
        for entry in root_entries.flatten() {
            if entry.file_type().is_ok_and(|kind| !kind.is_dir()) {
                index_cache_file(entry.path(), &mut entries);
            }
        }
    }
    let workspace_dirs = cache_workspace_dirs(cache_root);
    for (_, dir) in &workspace_dirs {
        if let Ok(sub_entries) = std::fs::read_dir(dir) {
            for sub_entry in sub_entries.flatten() {
                index_cache_file(sub_entry.path(), &mut entries);
            }
        }
    }
    ResolvedAssetIndex {
        cache_root: cache_root.to_path_buf(),
        built_at: Instant::now(),
        entries,
        workspace_dirs: workspace_dirs.into_iter().map(|(name, _)| name).collect(),
    }
}

thread_local! {
    static RESOLVED_ASSET_INDEX: RefCell<Option<ResolvedAssetIndex>> = const { RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    static INDEX_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Looks up `cache_root` in the thread-local index, rebuilding it (one
/// `read_dir` of the root plus one per workspace subdirectory) only when the
/// root changed or the cached snapshot has aged past
/// [`RESOLVED_ASSET_INDEX_TTL`].
fn with_resolved_asset_index<T>(
    cache_root: &Path,
    f: impl FnOnce(&mut ResolvedAssetIndex) -> T,
) -> T {
    RESOLVED_ASSET_INDEX.with(|cell| {
        let mut slot = cell.borrow_mut();
        let stale = match slot.as_ref() {
            Some(index) => {
                index.cache_root != cache_root
                    || index.built_at.elapsed() >= RESOLVED_ASSET_INDEX_TTL
            }
            None => true,
        };
        if stale {
            *slot = Some(build_resolved_asset_index(cache_root));
        }
        f(slot.as_mut().expect("index was just built"))
    })
}

/// Test-only escape hatch: forces the next lookup to rebuild the index
/// instead of waiting out the TTL, so tests can assert on freshly-scanned
/// state without sleeping.
#[cfg(test)]
fn reset_resolved_asset_index_for_tests() {
    RESOLVED_ASSET_INDEX.with(|cell| *cell.borrow_mut() = None);
}

#[cfg(test)]
fn resolved_asset_index_builds_for_tests() -> usize {
    INDEX_BUILDS.with(std::cell::Cell::get)
}

fn hashed_cache_key(workspace_dir_name: &str, url: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(workspace_dir_name.as_bytes());
    hasher.update([0]);
    hasher.update(url.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Direct on-disk probe for one key in one directory, used when the index
/// misses: the snapshot may predate a download that just landed, so a miss
/// is re-checked against the filesystem instead of being trusted for the
/// rest of the TTL window. A plain `exists()` filters candidates before the
/// (costlier) containment check. A hit is recorded in the index.
fn probe_cache_key(index: &mut ResolvedAssetIndex, dir: &Path, key: &str) -> Option<PathBuf> {
    let (path, resolved) = EXTENSIONS.iter().find_map(|ext| {
        let path = dir.join(format!("{key}{ext}"));
        if !path.exists() {
            return None;
        }
        let resolved = contained_cache_file(&index.cache_root, &path)?;
        Some((path, resolved))
    })?;
    index.entries.insert(key.to_string(), path);
    Some(resolved)
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

    let asset_key = source_uri
        .strip_prefix("conduit-asset://")
        .or_else(|| source_uri.strip_prefix("conduit-cache://"))
        .map(|key| key.split('?').next().unwrap_or(key).trim_matches('/'))
        .filter(|key| is_safe_cache_key(key));

    with_resolved_asset_index(&cache_root, |index| {
        // Fast path: answer from the snapshot (one containment check).
        if let Some(path) = asset_key.and_then(|key| index.verified(key)) {
            return Some(path);
        }
        let hit = index
            .workspace_dirs
            .iter()
            .find_map(|dir_name| index.verified(&hashed_cache_key(dir_name, url)));
        if hit.is_some() {
            return hit;
        }

        // Miss: re-check the disk for just these keys. The root listing is
        // re-read (it holds only workspace dirs) so a workspace directory
        // created after the snapshot is found too.
        let workspace_dirs = cache_workspace_dirs(&cache_root);
        if let Some(key) = asset_key {
            let found = std::iter::once(cache_root.as_path())
                .chain(workspace_dirs.iter().map(|(_, dir)| dir.as_path()))
                .find_map(|dir| probe_cache_key(index, dir, key));
            if found.is_some() {
                return found;
            }
        }
        workspace_dirs.iter().find_map(|(dir_name, dir)| {
            probe_cache_key(index, dir, &hashed_cache_key(dir_name, url))
        })
    })
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

    /// A repeated lookup for the same key must come from the in-memory
    /// index, not a fresh directory scan: only one index build happens
    /// across both lookups until the index is reset (standing in for TTL
    /// expiry).
    #[test]
    fn resolve_uses_cached_index_instead_of_rescanning_on_repeat_lookup() {
        reset_resolved_asset_index_for_tests();
        let fx = Fixture::new("index-reuse");
        let key = "b".repeat(64);
        let file = fx.cache.join("workspace").join(format!("{key}.png"));
        std::fs::write(&file, b"img").unwrap();
        let uri = format!("conduit-asset://{key}");
        let ctx = test_context();

        let builds = resolved_asset_index_builds_for_tests();
        let first = resolve_cached_asset_path_in(&fx.cache, &uri, &ctx);
        assert_eq!(first, Some(file.canonicalize().unwrap()));
        assert_eq!(resolved_asset_index_builds_for_tests(), builds + 1);

        let second = resolve_cached_asset_path_in(&fx.cache, &uri, &ctx);
        assert_eq!(second, first);
        assert_eq!(resolved_asset_index_builds_for_tests(), builds + 1);

        // A hit is still verified at lookup time: a file deleted after the
        // snapshot is not returned.
        std::fs::remove_file(&file).unwrap();
        let third = resolve_cached_asset_path_in(&fx.cache, &uri, &ctx);
        assert_eq!(third, None);
    }

    /// Regression: a lookup before the download lands must not leave a
    /// stale miss that hides the file for the rest of the TTL window.
    #[test]
    fn resolve_finds_asset_written_after_a_miss_without_waiting_for_ttl() {
        reset_resolved_asset_index_for_tests();
        let fx = Fixture::new("stale-miss");
        let key = "c".repeat(64);
        let uri = format!("conduit-asset://{key}");
        let ctx = test_context();

        assert_eq!(resolve_cached_asset_path_in(&fx.cache, &uri, &ctx), None);
        let builds = resolved_asset_index_builds_for_tests();

        let file = fx.cache.join("workspace").join(format!("{key}.webp"));
        std::fs::write(&file, b"img").unwrap();
        assert_eq!(
            resolve_cached_asset_path_in(&fx.cache, &uri, &ctx),
            Some(file.canonicalize().unwrap())
        );

        // Same for URL-hash lookups in a workspace dir created after the
        // snapshot was taken.
        let url = "https://example.com/late.png";
        assert_eq!(resolve_cached_asset_path_in(&fx.cache, url, &ctx), None);
        let late_ws = fx.cache.join("late_ws");
        std::fs::create_dir_all(&late_ws).unwrap();
        let hashed = late_ws.join(format!("{}.png", hashed_cache_key("late_ws", url)));
        std::fs::write(&hashed, b"img").unwrap();
        assert_eq!(
            resolve_cached_asset_path_in(&fx.cache, url, &ctx),
            Some(hashed.canonicalize().unwrap())
        );
        assert_eq!(resolved_asset_index_builds_for_tests(), builds);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_cache_dir_inside_workspace_dir() {
        reset_resolved_asset_index_for_tests();
        let fx = Fixture::new("symlink-ws");
        let key = "d".repeat(64);
        std::os::unix::fs::symlink(
            &fx.secret,
            fx.cache.join("workspace").join(format!("{key}.png")),
        )
        .unwrap();
        let found = resolve_cached_asset_path_in(
            &fx.cache,
            &format!("conduit-asset://{key}"),
            &test_context(),
        );
        assert_eq!(found, None);
    }
}
