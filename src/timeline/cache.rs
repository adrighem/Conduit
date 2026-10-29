use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gtk::prelude::TextureExt;

use crate::message_markup::MessageRenderContext;

pub(crate) const MAX_TEXTURE_CACHE_SIZE: usize = 256;
/// Approximate memory (width x height x 4) each texture cache may hold. The
/// entry cap alone lets 256 full-size photos reach gigabytes.
pub(crate) const MAX_TEXTURE_CACHE_BYTES: usize = 128 * 1024 * 1024;

pub(crate) type CacheEntry = gtk::gdk::Texture;

fn texture_bytes(texture: &CacheEntry) -> usize {
    let side = |side: i32| usize::try_from(side).unwrap_or(0);
    side(texture.width())
        .saturating_mul(side(texture.height()))
        .saturating_mul(4)
}

/// Least-recently-used texture cache bounded by entry count and by bytes.
/// A single texture over the byte budget is still kept, alone, so it is not
/// decoded again on every row build.
pub(crate) struct BoundedTextureCache {
    entries: HashMap<PathBuf, CacheEntry>,
    order: VecDeque<PathBuf>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl BoundedTextureCache {
    pub(crate) fn new() -> Self {
        Self::with_limits(MAX_TEXTURE_CACHE_SIZE, MAX_TEXTURE_CACHE_BYTES)
    }

    fn with_limits(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
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
        if let Some(previous) = self.entries.remove(&path) {
            self.bytes -= texture_bytes(&previous);
            if let Some(pos) = self.order.iter().position(|p| p == &path) {
                self.order.remove(pos);
            }
        }
        let added = texture_bytes(&value);
        while !self.order.is_empty()
            && (self.entries.len() >= self.max_entries || self.bytes + added > self.max_bytes)
        {
            if let Some(oldest) = self.order.pop_front() {
                if let Some(evicted) = self.entries.remove(&oldest) {
                    self.bytes -= texture_bytes(&evicted);
                }
            }
        }
        self.bytes += added;
        self.entries.insert(path.clone(), value);
        self.order.push_back(path);
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
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
    decode::reset_texture_decodes();
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
        if let Ok(tex) = crate::debug::trace_image_decode(
            "texture",
            path,
            crate::debug::ImageLoader::Texture,
            || gtk::gdk::Texture::from_filename(path),
        ) {
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
/// for URLs the runtime has not reported yet (once per visible row, on every
/// scroll/recycle); without this, each call
/// re-walked the cache directory and probed every known extension for every
/// candidate path. A short TTL turns that into one directory walk every few
/// seconds. Misses are never trusted from the snapshot alone (see
/// [`probe_cache_key`]), so a freshly downloaded asset resolves immediately.
const RESOLVED_ASSET_INDEX_TTL: Duration = Duration::from_secs(2);

/// Snapshot of the per-workspace asset cache directories: every file stem
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

/// Verifies that a cache path the runtime reported still points at a regular
/// file inside the image asset cache (symlinks followed).
pub(crate) fn verified_cached_asset_path(path: &Path) -> Option<PathBuf> {
    let cache_root = crate::config::image_asset_cache_dir().canonicalize().ok()?;
    contained_cache_file(&cache_root, path)
}

/// Maps a Slack-provided URL to a file inside the image asset cache: the
/// path the runtime reported for it (`context.image_assets`), or else the
/// URL-hash file a previous session left behind. Never returns a path
/// outside the cache directory, so untrusted message data cannot point the
/// UI at arbitrary local files (`/dev/zero`, `~/.ssh/...`, FIFOs).
pub(crate) fn resolve_cached_asset_path(
    url: &str,
    context: &MessageRenderContext,
) -> Option<PathBuf> {
    resolve_cached_asset_path_in(&crate::config::image_asset_cache_dir(), url, context)
}

fn resolve_cached_asset_path_in(
    cache_dir: &Path,
    url: &str,
    context: &MessageRenderContext,
) -> Option<PathBuf> {
    let cache_root = cache_dir.canonicalize().ok()?;
    if let Some(path) = context
        .image_assets
        .get(url)
        .and_then(|path| contained_cache_file(&cache_root, path))
    {
        return Some(path);
    }

    with_resolved_asset_index(&cache_root, |index| {
        // Fast path: answer from the snapshot (one containment check).
        let hit = index
            .workspace_dirs
            .iter()
            .find_map(|dir_name| index.verified(&hashed_cache_key(dir_name, url)));
        if hit.is_some() {
            return hit;
        }

        // Miss: re-check the disk for just this URL. The root listing is
        // re-read (it holds only workspace dirs) so a workspace directory
        // created after the snapshot is found too.
        cache_workspace_dirs(&cache_root)
            .iter()
            .find_map(|(dir_name, dir)| {
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

    /// A render context in which the runtime reported `path` for `url`.
    fn context_with_asset(url: &str, path: PathBuf) -> MessageRenderContext {
        let mut ctx = test_context();
        ctx.image_assets.insert(url.to_string(), path);
        ctx
    }

    /// Where the runtime stores `url` for the fixture's `workspace` dir.
    fn hashed_file(fx: &Fixture, url: &str, ext: &str) -> PathBuf {
        fx.cache
            .join("workspace")
            .join(format!("{}{ext}", hashed_cache_key("workspace", url)))
    }

    #[test]
    fn resolves_registered_path_inside_cache_dir() {
        let fx = Fixture::new("inside");
        let file = fx
            .cache
            .join("workspace")
            .join(format!("{}.png", "a".repeat(64)));
        std::fs::write(&file, b"img").unwrap();
        let url = "https://example.com/inside.png";
        let found =
            resolve_cached_asset_path_in(&fx.cache, url, &context_with_asset(url, file.clone()));
        assert_eq!(found, Some(file.canonicalize().unwrap()));
    }

    #[test]
    fn rejects_registered_paths_escaping_cache_dir() {
        let fx = Fixture::new("dotdot");
        let url = "https://example.com/escape.png";
        for path in [
            fx.cache.join("workspace").join("../../secret.png"),
            fx.cache.join("../secret.png"),
            fx.secret.clone(),
            PathBuf::from("/dev/zero"),
            fx.cache.join("workspace"),
        ] {
            let ctx = context_with_asset(url, path.clone());
            assert_eq!(
                resolve_cached_asset_path_in(&fx.cache, url, &ctx),
                None,
                "{}",
                path.display()
            );
        }
        assert!(fx.secret.is_file());
    }

    #[test]
    fn legacy_asset_uris_do_not_resolve() {
        let fx = Fixture::new("legacy-uri");
        let key = "a".repeat(64);
        std::fs::write(
            fx.cache.join("workspace").join(format!("{key}.png")),
            b"img",
        )
        .unwrap();
        let ctx = test_context();
        for uri in [
            format!("conduit-asset://{key}"),
            format!("conduit-cache://{key}"),
            "conduit-asset://../secret".to_string(),
        ] {
            assert_eq!(
                resolve_cached_asset_path_in(&fx.cache, &uri, &ctx),
                None,
                "{uri}"
            );
        }
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
    fn rejects_registered_symlink_escaping_cache_dir() {
        let fx = Fixture::new("symlink");
        let link = fx.cache.join("workspace").join("escape.png");
        std::os::unix::fs::symlink(&fx.secret, &link).unwrap();
        let url = "https://example.com/escape.png";
        let found = resolve_cached_asset_path_in(&fx.cache, url, &context_with_asset(url, link));
        assert_eq!(found, None);
    }

    #[test]
    fn rejects_extensionless_cache_entries() {
        reset_resolved_asset_index_for_tests();
        let fx = Fixture::new("noext");
        let url = "https://example.com/bare";
        std::fs::write(hashed_file(&fx, url, ""), b"img").unwrap();
        let found = resolve_cached_asset_path_in(&fx.cache, url, &test_context());
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

    /// A repeated URL-hash lookup must come from the in-memory index, not a
    /// fresh directory scan: only one index build happens across both
    /// lookups until the index is reset (standing in for TTL expiry).
    #[test]
    fn resolve_uses_cached_index_instead_of_rescanning_on_repeat_lookup() {
        reset_resolved_asset_index_for_tests();
        let fx = Fixture::new("index-reuse");
        let url = "https://example.com/reuse.png";
        let file = hashed_file(&fx, url, ".png");
        std::fs::write(&file, b"img").unwrap();
        let ctx = test_context();

        let builds = resolved_asset_index_builds_for_tests();
        let first = resolve_cached_asset_path_in(&fx.cache, url, &ctx);
        assert_eq!(first, Some(file.canonicalize().unwrap()));
        assert_eq!(resolved_asset_index_builds_for_tests(), builds + 1);

        let second = resolve_cached_asset_path_in(&fx.cache, url, &ctx);
        assert_eq!(second, first);
        assert_eq!(resolved_asset_index_builds_for_tests(), builds + 1);

        // A hit is still verified at lookup time: a file deleted after the
        // snapshot is not returned.
        std::fs::remove_file(&file).unwrap();
        let third = resolve_cached_asset_path_in(&fx.cache, url, &ctx);
        assert_eq!(third, None);
    }

    /// Regression: a lookup before the download lands must not leave a
    /// stale miss that hides the file for the rest of the TTL window.
    #[test]
    fn resolve_finds_asset_written_after_a_miss_without_waiting_for_ttl() {
        reset_resolved_asset_index_for_tests();
        let fx = Fixture::new("stale-miss");
        let ctx = test_context();

        let url = "https://example.com/early.webp";
        assert_eq!(resolve_cached_asset_path_in(&fx.cache, url, &ctx), None);
        let builds = resolved_asset_index_builds_for_tests();

        let file = hashed_file(&fx, url, ".webp");
        std::fs::write(&file, b"img").unwrap();
        assert_eq!(
            resolve_cached_asset_path_in(&fx.cache, url, &ctx),
            Some(file.canonicalize().unwrap())
        );

        // Same in a workspace dir created after the snapshot was taken.
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
        let url = "https://example.com/symlink.png";
        std::os::unix::fs::symlink(&fx.secret, hashed_file(&fx, url, ".png")).unwrap();
        let found = resolve_cached_asset_path_in(&fx.cache, url, &test_context());
        assert_eq!(found, None);
    }
}

#[cfg(test)]
mod texture_cache_tests {
    use super::*;

    /// A texture of `side` x `side` pixels: `side * side * 4` budgeted bytes.
    fn texture(side: i32) -> CacheEntry {
        let pixbuf = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, side, side)
            .expect("allocate pixbuf");
        gtk::gdk::Texture::for_pixbuf(&pixbuf)
    }

    fn path(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    fn has(cache: &mut BoundedTextureCache, name: &str) -> bool {
        cache.get(&path(name)).is_some()
    }

    #[test]
    fn evicts_least_recently_used_entries_to_stay_within_the_byte_budget() {
        // Room for two 16x16 textures (1 KiB each), not three.
        let mut cache = BoundedTextureCache::with_limits(100, 2 * 1024);
        cache.insert(path("a"), texture(16));
        cache.insert(path("b"), texture(16));
        assert!(has(&mut cache, "a"), "touch a so b is the oldest");
        cache.insert(path("c"), texture(16));
        assert!(!has(&mut cache, "b"));
        assert!(has(&mut cache, "a") && has(&mut cache, "c"));
        assert_eq!(cache.bytes, 2 * 1024);
    }

    #[test]
    fn one_large_texture_evicts_as_many_entries_as_it_needs() {
        let mut cache = BoundedTextureCache::with_limits(100, 4 * 1024);
        for name in ["a", "b", "c"] {
            cache.insert(path(name), texture(16));
        }
        // 32x32 is 4 KiB: the whole budget.
        cache.insert(path("big"), texture(32));
        assert_eq!(cache.entries.len(), 1);
        assert!(has(&mut cache, "big"));
        assert_eq!(cache.bytes, 4 * 1024);
    }

    #[test]
    fn a_texture_over_the_budget_is_kept_alone() {
        let mut cache = BoundedTextureCache::with_limits(100, 1024);
        cache.insert(path("a"), texture(16));
        cache.insert(path("huge"), texture(64));
        assert!(!has(&mut cache, "a"));
        assert!(has(&mut cache, "huge"));
        cache.insert(path("b"), texture(16));
        assert!(!has(&mut cache, "huge"));
        assert_eq!(cache.bytes, 1024);
    }

    #[test]
    fn replacing_an_entry_swaps_its_bytes_and_clear_resets_them() {
        let mut cache = BoundedTextureCache::with_limits(100, 8 * 1024);
        cache.insert(path("a"), texture(16));
        cache.insert(path("a"), texture(32));
        assert_eq!(cache.bytes, 4 * 1024);
        assert_eq!(cache.entries.len(), 1);
        cache.clear();
        assert_eq!(cache.bytes, 0);
        assert!(!has(&mut cache, "a"));
    }

    #[test]
    fn the_entry_cap_still_applies() {
        let mut cache = BoundedTextureCache::with_limits(2, usize::MAX);
        for name in ["a", "b", "c"] {
            cache.insert(path(name), texture(4));
        }
        assert!(!has(&mut cache, "a"));
        assert!(has(&mut cache, "b") && has(&mut cache, "c"));
    }
}

/// Static textures decoded off the GTK thread.
///
/// A worker reads each file once, checks the decode budget on those bytes
/// and decodes the same bytes, so a cache prune or atomic rename between
/// check and decode cannot hand the loader another file. Results come back
/// to the GTK thread, which alone fills [`TEXTURE_CACHE`] and paints.
pub(crate) mod decode {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::hash::Hash;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::rc::{Rc, Weak};
    use std::time::{Duration, Instant};

    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use gtk::{gdk, gio, glib};

    use super::TEXTURE_CACHE;

    /// Decodes running at once: each may be a sandboxed glycin subprocess,
    /// and GIO's worker pool is shared with file I/O.
    pub(crate) const MAX_IN_FLIGHT: usize = 3;
    /// Retries of a transient failure (missing file, read error).
    pub(crate) const MAX_TRANSIENT_RETRIES: u32 = 1;
    pub(crate) const RETRY_DELAY: Duration = Duration::from_secs(2);
    /// How long a key whose retries ran out is refused before trying again.
    pub(crate) const TRANSIENT_COOLDOWN: Duration = Duration::from_secs(30);
    const MAX_FAILED: usize = 4096;
    /// Largest file a worker reads. Downloads are capped at this size.
    pub(crate) const MAX_ASSET_BYTES: usize = crate::slack::MAX_PREVIEW_IMAGE_BYTES;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum DecodeError {
        /// Decoding again cannot help: over budget, too large, corrupt or an
        /// unsupported format.
        Permanent(String),
        /// The file may be back shortly: missing (pruned and re-downloaded)
        /// or unreadable for now.
        Transient(String),
    }

    /// How a texture is decoded. Part of the dedupe key, so scaled decodes
    /// can join later without sharing a full-size result.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub(crate) enum TextureSpec {
        Full,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub(crate) struct TextureKey {
        pub(crate) path: PathBuf,
        pub(crate) spec: TextureSpec,
    }

    pub(crate) type Done<T> = Box<dyn FnOnce(Result<T, DecodeError>)>;
    type Lookup<K, T> = Box<dyn Fn(&K) -> Option<T>>;
    type Store<K, T> = Box<dyn Fn(&K, &T)>;
    pub(crate) type Timer = Box<dyn FnOnce()>;

    /// Where decodes run. `spawn` and `after` must call back later on the
    /// GTK thread, never from inside the call itself.
    pub(crate) trait DecodeExecutor<K, T> {
        fn spawn(&self, key: K, done: Done<T>);
        fn after(&self, delay: Duration, fire: Timer);
        fn now(&self) -> Instant;
    }

    /// A waiter's liveness and generation. The requester keeps the ticket;
    /// dropping it kills its waiters and [`DecodeTicket::invalidate`] makes
    /// them stale. Results for either are still cached, never applied.
    #[derive(Clone, Default)]
    pub(crate) struct DecodeTicket(Rc<Cell<u64>>);

    impl DecodeTicket {
        pub(crate) fn invalidate(&self) {
            self.0.set(self.0.get().wrapping_add(1));
        }
    }

    struct Waiter<T> {
        ticket: Weak<Cell<u64>>,
        generation: u64,
        deliver: Box<dyn FnOnce(&T)>,
    }

    impl<T> Waiter<T> {
        fn is_current(&self) -> bool {
            self.ticket
                .upgrade()
                .is_some_and(|generation| generation.get() == self.generation)
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum JobState {
        Queued,
        Running,
        AwaitingRetry,
    }

    struct Job<T> {
        waiters: Vec<Waiter<T>>,
        state: JobState,
        retries: u32,
    }

    #[derive(Debug, Clone, Copy)]
    enum Failure {
        Permanent,
        TransientUntil(Instant),
    }

    /// What a request found.
    pub(crate) enum Requested<T> {
        /// Already cached: apply it now.
        Ready(T),
        /// Queued or joined a running decode; `deliver` runs on completion
        /// if the ticket is still current.
        Pending,
        /// Known to fail; show the empty placeholder.
        Failed,
    }

    struct State<K, T> {
        executor: Rc<dyn DecodeExecutor<K, T>>,
        lookup: Lookup<K, T>,
        store: Store<K, T>,
        /// Bumped by [`Scheduler::reset`]; older completions are dropped.
        epoch: u64,
        /// Decodes executing, whatever their epoch.
        running: usize,
        jobs: HashMap<K, Job<T>>,
        /// Queued keys; the last requested is dispatched first, since it is
        /// what was bound most recently.
        queue: Vec<K>,
        failed: HashMap<K, Failure>,
    }

    /// Deduplicating decode queue with at most [`MAX_IN_FLIGHT`] decodes
    /// running. GTK-free, so tests drive it with a [`ManualExecutor`].
    pub(crate) struct Scheduler<K, T> {
        state: Rc<RefCell<State<K, T>>>,
    }

    impl<K, T> Clone for Scheduler<K, T> {
        fn clone(&self) -> Self {
            Self {
                state: self.state.clone(),
            }
        }
    }

    impl<K: Clone + Eq + Hash + 'static, T: Clone + 'static> Scheduler<K, T> {
        pub(crate) fn new(
            executor: Rc<dyn DecodeExecutor<K, T>>,
            lookup: impl Fn(&K) -> Option<T> + 'static,
            store: impl Fn(&K, &T) + 'static,
        ) -> Self {
            Self {
                state: Rc::new(RefCell::new(State {
                    executor,
                    lookup: Box::new(lookup),
                    store: Box::new(store),
                    epoch: 0,
                    running: 0,
                    jobs: HashMap::new(),
                    queue: Vec::new(),
                    failed: HashMap::new(),
                })),
            }
        }

        /// Asks for `key` on behalf of `ticket`'s current generation.
        pub(crate) fn request(
            &self,
            key: K,
            ticket: &DecodeTicket,
            deliver: impl FnOnce(&T) + 'static,
        ) -> Requested<T> {
            let mut state = self.state.borrow_mut();
            if let Some(value) = (state.lookup)(&key) {
                return Requested::Ready(value);
            }
            match state.failed.get(&key) {
                Some(Failure::Permanent) => return Requested::Failed,
                Some(Failure::TransientUntil(until)) => {
                    if state.executor.now() < *until {
                        return Requested::Failed;
                    }
                    state.failed.remove(&key);
                }
                None => {}
            }
            let waiter = Waiter {
                ticket: Rc::downgrade(&ticket.0),
                generation: ticket.0.get(),
                deliver: Box::new(deliver),
            };
            let job = state.jobs.entry(key.clone()).or_insert_with(|| Job {
                waiters: Vec::new(),
                state: JobState::Queued,
                retries: 0,
            });
            job.waiters.retain(Waiter::is_current);
            job.waiters.push(waiter);
            if job.state == JobState::Queued {
                // Re-requesting moves the key to the front of the line.
                state.queue.retain(|queued| queued != &key);
                state.queue.push(key);
            }
            self.pump(&mut state);
            Requested::Pending
        }

        /// Whether `key` is known to fail right now.
        pub(crate) fn is_failed(&self, key: &K) -> bool {
            let state = self.state.borrow();
            match state.failed.get(key) {
                Some(Failure::Permanent) => true,
                Some(Failure::TransientUntil(until)) => state.executor.now() < *until,
                None => false,
            }
        }

        /// Forgets known failures and results in progress: decodes already
        /// running finish, but are neither cached nor delivered. Waiters
        /// still current are queued afresh, so no visible image is left
        /// waiting on a dropped result.
        pub(crate) fn reset(&self) {
            let mut state = self.state.borrow_mut();
            state.epoch = state.epoch.wrapping_add(1);
            state.failed.clear();
            state.queue.clear();
            let mut requeued = Vec::new();
            state.jobs.retain(|key, job| {
                job.waiters.retain(Waiter::is_current);
                job.state = JobState::Queued;
                job.retries = 0;
                let keep = !job.waiters.is_empty();
                if keep {
                    requeued.push(key.clone());
                }
                keep
            });
            state.queue = requeued;
            self.pump(&mut state);
        }

        fn pump(&self, state: &mut State<K, T>) {
            while state.running < MAX_IN_FLIGHT {
                let Some(key) = state.queue.pop() else {
                    return;
                };
                let Some(job) = state.jobs.get_mut(&key) else {
                    continue;
                };
                job.waiters.retain(Waiter::is_current);
                if job.waiters.is_empty() {
                    // Scrolled past before its turn: skip the decode.
                    state.jobs.remove(&key);
                    continue;
                }
                job.state = JobState::Running;
                state.running += 1;
                let weak = Rc::downgrade(&self.state);
                let epoch = state.epoch;
                let done_key = key.clone();
                state.executor.spawn(
                    key,
                    Box::new(move |result| {
                        if let Some(state) = weak.upgrade() {
                            Scheduler { state }.complete(epoch, done_key, result);
                        }
                    }),
                );
            }
        }

        fn complete(&self, epoch: u64, key: K, result: Result<T, DecodeError>) {
            let delivery = {
                let mut state = self.state.borrow_mut();
                state.running = state.running.saturating_sub(1);
                let delivery = if epoch == state.epoch {
                    self.settle(&mut state, key, result)
                } else {
                    None
                };
                self.pump(&mut state);
                delivery
            };
            // Outside the borrow: a delivery may request more textures.
            if let Some((value, waiters)) = delivery {
                for waiter in waiters {
                    if waiter.is_current() {
                        (waiter.deliver)(&value);
                    }
                }
            }
        }

        fn settle(
            &self,
            state: &mut State<K, T>,
            key: K,
            result: Result<T, DecodeError>,
        ) -> Option<(T, Vec<Waiter<T>>)> {
            match result {
                Ok(value) => {
                    (state.store)(&key, &value);
                    let job = state.jobs.remove(&key)?;
                    Some((value, job.waiters))
                }
                Err(DecodeError::Transient(_)) => {
                    let job = state.jobs.get_mut(&key)?;
                    if job.retries < MAX_TRANSIENT_RETRIES {
                        job.retries += 1;
                        job.state = JobState::AwaitingRetry;
                        let weak = Rc::downgrade(&self.state);
                        let epoch = state.epoch;
                        state.executor.after(
                            RETRY_DELAY,
                            Box::new(move || {
                                if let Some(state) = weak.upgrade() {
                                    Scheduler { state }.retry(epoch, key);
                                }
                            }),
                        );
                    } else {
                        state.jobs.remove(&key);
                        let until = state.executor.now() + TRANSIENT_COOLDOWN;
                        remember_failure(state, key, Failure::TransientUntil(until));
                    }
                    None
                }
                Err(DecodeError::Permanent(_)) => {
                    state.jobs.remove(&key);
                    remember_failure(state, key, Failure::Permanent);
                    None
                }
            }
        }

        fn retry(&self, epoch: u64, key: K) {
            let mut state = self.state.borrow_mut();
            if epoch != state.epoch {
                return;
            }
            let Some(job) = state.jobs.get_mut(&key) else {
                return;
            };
            if job.state != JobState::AwaitingRetry {
                return;
            }
            job.state = JobState::Queued;
            state.queue.push(key);
            self.pump(&mut state);
        }
    }

    fn remember_failure<K: Eq + Hash, T>(state: &mut State<K, T>, key: K, failure: Failure) {
        if state.failed.len() >= MAX_FAILED {
            state.failed.clear();
        }
        state.failed.insert(key, failure);
    }

    /// Reads `path` once, checks the decode budget on those bytes and
    /// decodes them. Runs on a worker thread.
    pub(crate) fn decode_texture_file(path: &Path) -> Result<gdk::Texture, DecodeError> {
        let bytes = read_capped(path, MAX_ASSET_BYTES)?;
        if !crate::animated_image::bytes_within_decode_budget(&bytes, path) {
            return Err(DecodeError::Permanent("over the decode budget".into()));
        }
        crate::debug::trace_image_decode(
            "texture",
            path,
            crate::debug::ImageLoader::Texture,
            || gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)),
        )
        .map_err(|error| classify_glib_error(&error))
    }

    fn read_capped(path: &Path, cap: usize) -> Result<Vec<u8>, DecodeError> {
        let transient = |error: std::io::Error| DecodeError::Transient(error.to_string());
        let file = std::fs::File::open(path).map_err(transient)?;
        let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
        let mut bytes = Vec::new();
        file.take(limit)
            .read_to_end(&mut bytes)
            .map_err(transient)?;
        if bytes.len() > cap {
            return Err(DecodeError::Permanent(format!("larger than {cap} bytes")));
        }
        Ok(bytes)
    }

    /// File errors may clear once the asset is downloaded again; loader
    /// errors (`gdk-pixbuf-error`, `gdk-texture-error`) mean the bytes are bad.
    pub(crate) fn classify_glib_error(error: &glib::Error) -> DecodeError {
        if error.kind::<glib::FileError>().is_some() {
            DecodeError::Transient(error.to_string())
        } else {
            DecodeError::Permanent(error.to_string())
        }
    }

    /// Decodes on GIO's blocking pool and completes on the GTK thread.
    struct GioExecutor;

    impl DecodeExecutor<TextureKey, gdk::Texture> for GioExecutor {
        fn spawn(&self, key: TextureKey, done: Done<gdk::Texture>) {
            glib::spawn_future_local(async move {
                let result = gio::spawn_blocking(move || decode_texture_file(&key.path))
                    .await
                    .unwrap_or_else(|_| Err(DecodeError::Permanent("decode panicked".into())));
                done(result);
            });
        }

        fn after(&self, delay: Duration, fire: Timer) {
            glib::timeout_add_local_once(delay, fire);
        }

        fn now(&self) -> Instant {
            Instant::now()
        }
    }

    type TextureScheduler = Scheduler<TextureKey, gdk::Texture>;

    fn texture_scheduler(
        executor: Rc<dyn DecodeExecutor<TextureKey, gdk::Texture>>,
    ) -> TextureScheduler {
        Scheduler::new(
            executor,
            |key: &TextureKey| TEXTURE_CACHE.with(|cache| cache.borrow_mut().get(&key.path)),
            |key: &TextureKey, texture: &gdk::Texture| {
                TEXTURE_CACHE.with(|cache| {
                    cache.borrow_mut().insert(key.path.clone(), texture.clone());
                });
            },
        )
    }

    thread_local! {
        static SCHEDULER: RefCell<Option<TextureScheduler>> = const { RefCell::new(None) };
    }

    fn scheduler() -> TextureScheduler {
        SCHEDULER.with(|slot| {
            slot.borrow_mut()
                .get_or_insert_with(|| texture_scheduler(Rc::new(GioExecutor)))
                .clone()
        })
    }

    fn full_size(path: &Path) -> TextureKey {
        TextureKey {
            path: path.to_path_buf(),
            spec: TextureSpec::Full,
        }
    }

    /// The full-size texture of `path`: [`Requested::Ready`] from the cache,
    /// or decoded off the GTK thread and passed to `deliver` while `ticket`
    /// stays current.
    pub(crate) fn request_texture(
        path: &Path,
        ticket: &DecodeTicket,
        deliver: impl FnOnce(&gdk::Texture) + 'static,
    ) -> Requested<gdk::Texture> {
        scheduler().request(full_size(path), ticket, deliver)
    }

    /// Whether decoding `path` is known to fail, so a row can skip the
    /// placeholder.
    pub(crate) fn texture_failed(path: &Path) -> bool {
        scheduler().is_failed(&full_size(path))
    }

    pub(crate) fn reset_texture_decodes() {
        SCHEDULER.with(|slot| {
            if let Some(scheduler) = slot.borrow().as_ref() {
                scheduler.reset();
            }
        });
    }

    mod imp {
        use std::cell::{Cell, RefCell};

        use gtk::prelude::*;
        use gtk::subclass::prelude::*;
        use gtk::{gdk, glib};

        #[derive(Default)]
        pub struct DeferredTexture {
            pub(super) texture: RefCell<Option<gdk::Texture>>,
            /// Size the image's header declares, shown until it decodes.
            pub(super) declared: Cell<(i32, i32)>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for DeferredTexture {
            const NAME: &'static str = "ConduitDeferredTexture";
            type Type = super::DeferredTexture;
            type Interfaces = (gdk::Paintable,);
        }

        impl ObjectImpl for DeferredTexture {}

        impl PaintableImpl for DeferredTexture {
            fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
                if let Some(texture) = self.texture.borrow().as_ref() {
                    texture.snapshot(snapshot, width, height);
                }
            }

            fn intrinsic_width(&self) -> i32 {
                self.texture
                    .borrow()
                    .as_ref()
                    .map_or(self.declared.get().0, |texture| texture.width())
            }

            fn intrinsic_height(&self) -> i32 {
                self.texture
                    .borrow()
                    .as_ref()
                    .map_or(self.declared.get().1, |texture| texture.height())
            }
        }
    }

    glib::wrapper! {
        /// A static image that paints nothing at its declared size until its
        /// texture arrives, then paints in place: a picture showing it keeps
        /// its layout and its row is never rebound.
        pub struct DeferredTexture(ObjectSubclass<imp::DeferredTexture>)
            @implements gdk::Paintable;
    }

    impl DeferredTexture {
        fn new(declared: (i32, i32)) -> Self {
            let paintable: Self = glib::Object::new();
            paintable.imp().declared.set(declared);
            paintable
        }

        pub(crate) fn texture(&self) -> Option<gdk::Texture> {
            self.imp().texture.borrow().clone()
        }

        fn set_texture(&self, texture: &gdk::Texture) {
            let before = (self.intrinsic_width(), self.intrinsic_height());
            self.imp().texture.replace(Some(texture.clone()));
            if before != (texture.width(), texture.height()) {
                // Header unknown or wrong: the one case that relayouts.
                self.invalidate_size();
            }
            self.invalidate_contents();
        }
    }

    /// A picture of the static image at `path`. A cached texture shows at
    /// once; otherwise the picture holds a [`DeferredTexture`] sized from the
    /// image header and requests the decode only while it is mapped. Unmapping
    /// makes the request stale, and mapping again requests it anew.
    pub(crate) fn deferred_texture_picture(path: &Path) -> gtk::Picture {
        if let Some(texture) = TEXTURE_CACHE.with(|cache| cache.borrow_mut().get(path)) {
            return gtk::Picture::for_paintable(&texture);
        }
        if texture_failed(path) {
            // Decoding again would not help.
            return gtk::Picture::new();
        }
        let declared = crate::image_header::declared_dimensions(path)
            .and_then(|(width, height)| {
                Some((i32::try_from(width).ok()?, i32::try_from(height).ok()?))
            })
            .unwrap_or((0, 0));
        let paintable = DeferredTexture::new(declared);
        let picture = gtk::Picture::for_paintable(&paintable);
        let ticket = DecodeTicket::default();
        let weak = paintable.downgrade();
        let path = path.to_path_buf();
        let on_map = ticket.clone();
        picture.connect_map(move |_| request_into(&weak, &path, &on_map));
        picture.connect_unmap(move |_| ticket.invalidate());
        picture
    }

    fn request_into(weak: &glib::WeakRef<DeferredTexture>, path: &Path, ticket: &DecodeTicket) {
        let Some(paintable) = weak.upgrade() else {
            return;
        };
        if paintable.texture().is_some() {
            return;
        }
        let target = weak.clone();
        let requested = request_texture(path, ticket, move |texture| {
            if let Some(paintable) = target.upgrade() {
                paintable.set_texture(texture);
            }
        });
        if let Requested::Ready(texture) = requested {
            paintable.set_texture(&texture);
        }
    }

    /// Replaces this thread's scheduler with a fresh one on `executor`.
    /// `run_gtk_test` runs every GTK test on one thread, so tests that
    /// count decodes start from here.
    #[cfg(test)]
    pub(crate) fn install_texture_executor_for_tests(
        executor: Rc<dyn DecodeExecutor<TextureKey, gdk::Texture>>,
    ) {
        SCHEDULER.with(|slot| *slot.borrow_mut() = Some(texture_scheduler(executor)));
    }

    /// Back to a fresh scheduler on the real worker pool.
    #[cfg(test)]
    pub(crate) fn reset_texture_decoder_for_tests() {
        SCHEDULER.with(|slot| *slot.borrow_mut() = None);
    }

    /// Records jobs and timers; the test completes them in any order.
    #[cfg(test)]
    pub(crate) struct ManualExecutor<K, T> {
        jobs: RefCell<Vec<(K, Done<T>)>>,
        timers: RefCell<Vec<(Duration, Timer)>>,
        now: Cell<Instant>,
    }

    #[cfg(test)]
    impl<K: Clone, T> ManualExecutor<K, T> {
        pub(crate) fn new() -> Rc<Self> {
            Rc::new(Self {
                jobs: RefCell::new(Vec::new()),
                timers: RefCell::new(Vec::new()),
                now: Cell::new(Instant::now()),
            })
        }

        /// Keys of the decodes running now, in dispatch order.
        pub(crate) fn running(&self) -> Vec<K> {
            self.jobs
                .borrow()
                .iter()
                .map(|(key, _)| key.clone())
                .collect()
        }

        /// Finishes the `index`th running decode with `result`.
        pub(crate) fn complete(&self, index: usize, result: Result<T, DecodeError>) {
            let (_, done) = self.jobs.borrow_mut().remove(index);
            done(result);
        }

        pub(crate) fn advance(&self, by: Duration) {
            self.now.set(self.now.get() + by);
        }

        /// Fires every pending timer, as the main loop would once due.
        pub(crate) fn fire_timers(&self) {
            let timers = std::mem::take(&mut *self.timers.borrow_mut());
            for (_, fire) in timers {
                fire();
            }
        }

        pub(crate) fn timer_delays(&self) -> Vec<Duration> {
            self.timers
                .borrow()
                .iter()
                .map(|(delay, _)| *delay)
                .collect()
        }
    }

    #[cfg(test)]
    impl<K, T> DecodeExecutor<K, T> for ManualExecutor<K, T> {
        fn spawn(&self, key: K, done: Done<T>) {
            self.jobs.borrow_mut().push((key, done));
        }

        fn after(&self, delay: Duration, fire: Timer) {
            self.timers.borrow_mut().push((delay, fire));
        }

        fn now(&self) -> Instant {
            self.now.get()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::HashSet;

        type TestScheduler = Scheduler<&'static str, u32>;

        struct Harness {
            executor: Rc<ManualExecutor<&'static str, u32>>,
            scheduler: TestScheduler,
            cache: Rc<RefCell<HashMap<&'static str, u32>>>,
            delivered: Rc<RefCell<Vec<(&'static str, u32)>>>,
        }

        impl Harness {
            fn new() -> Self {
                let executor = ManualExecutor::new();
                let cache = Rc::new(RefCell::new(HashMap::new()));
                let lookup = cache.clone();
                let store = cache.clone();
                let scheduler = Scheduler::new(
                    executor.clone(),
                    move |key: &&'static str| lookup.borrow().get(key).copied(),
                    move |key: &&'static str, value: &u32| {
                        store.borrow_mut().insert(*key, *value);
                    },
                );
                Self {
                    executor,
                    scheduler,
                    cache,
                    delivered: Rc::default(),
                }
            }

            fn request(&self, key: &'static str, ticket: &DecodeTicket) -> Requested<u32> {
                let delivered = self.delivered.clone();
                self.scheduler.request(key, ticket, move |value| {
                    delivered.borrow_mut().push((key, *value));
                })
            }

            fn complete(&self, key: &'static str, result: Result<u32, DecodeError>) {
                let index = self
                    .executor
                    .running()
                    .iter()
                    .position(|running| *running == key)
                    .expect("decode is running");
                self.executor.complete(index, result);
            }
        }

        fn pending(requested: Requested<u32>) -> bool {
            matches!(requested, Requested::Pending)
        }

        fn transient() -> Result<u32, DecodeError> {
            Err(DecodeError::Transient("missing".into()))
        }

        #[test]
        fn repeat_requests_share_one_decode() {
            let harness = Harness::new();
            let tickets: Vec<DecodeTicket> = (0..40).map(|_| DecodeTicket::default()).collect();
            for ticket in &tickets {
                assert!(pending(harness.request("a", ticket)));
            }
            assert_eq!(harness.executor.running(), vec!["a"]);
            harness.complete("a", Ok(7));
            assert_eq!(harness.delivered.borrow().len(), 40);
            assert!(matches!(
                harness.request("a", &tickets[0]),
                Requested::Ready(7)
            ));
            assert!(harness.executor.running().is_empty());
        }

        #[test]
        fn at_most_three_decodes_run_and_the_newest_request_goes_first() {
            let harness = Harness::new();
            let ticket = DecodeTicket::default();
            for key in ["a", "b", "c", "d", "e"] {
                harness.request(key, &ticket);
            }
            assert_eq!(harness.executor.running(), vec!["a", "b", "c"]);
            // Completions arrive out of order; each frees one slot.
            harness.complete("b", Ok(2));
            assert_eq!(harness.executor.running(), vec!["a", "c", "e"]);
            harness.complete("e", Ok(5));
            harness.complete("a", Ok(1));
            assert_eq!(harness.executor.running(), vec!["c", "d"]);
            harness.complete("d", Ok(4));
            harness.complete("c", Ok(3));
            let delivered: HashSet<_> = harness.delivered.borrow().iter().copied().collect();
            assert_eq!(
                delivered,
                HashSet::from([("a", 1), ("b", 2), ("c", 3), ("d", 4), ("e", 5)])
            );
        }

        #[test]
        fn stale_and_dead_waiters_are_cached_but_not_applied() {
            let harness = Harness::new();
            let stale = DecodeTicket::default();
            let dead = DecodeTicket::default();
            let live = DecodeTicket::default();
            harness.request("a", &stale);
            harness.request("a", &dead);
            harness.request("a", &live);
            stale.invalidate();
            drop(dead);
            harness.complete("a", Ok(1));
            assert_eq!(*harness.delivered.borrow(), vec![("a", 1)]);
            assert_eq!(harness.cache.borrow().get("a"), Some(&1));

            // A running decode nobody waits for any more is still cached.
            let gone = DecodeTicket::default();
            harness.request("b", &gone);
            gone.invalidate();
            harness.complete("b", Ok(2));
            assert_eq!(harness.delivered.borrow().len(), 1);
            assert_eq!(harness.cache.borrow().get("b"), Some(&2));
        }

        #[test]
        fn queued_work_without_current_waiters_is_never_decoded() {
            let harness = Harness::new();
            let visible = DecodeTicket::default();
            let scrolled_past = DecodeTicket::default();
            for key in ["a", "b", "c"] {
                harness.request(key, &visible);
            }
            harness.request("d", &scrolled_past);
            scrolled_past.invalidate();
            harness.complete("a", Ok(1));
            assert_eq!(harness.executor.running(), vec!["b", "c"]);
            // Requested again on map: queued afresh and decoded.
            harness.request("d", &scrolled_past);
            assert_eq!(harness.executor.running(), vec!["b", "c", "d"]);
        }

        #[test]
        fn reset_drops_results_from_before_it() {
            let harness = Harness::new();
            let ticket = DecodeTicket::default();
            harness.request("a", &ticket);
            let gone = DecodeTicket::default();
            harness.request("b", &gone);
            drop(gone);
            harness.scheduler.reset();
            // The live waiter's decode starts over; the dead one's does not.
            assert_eq!(harness.executor.running(), vec!["a", "b", "a"]);
            harness.executor.complete(0, Ok(1));
            harness.executor.complete(0, Ok(2));
            assert!(harness.delivered.borrow().is_empty());
            assert!(harness.cache.borrow().is_empty());
            harness.complete("a", Ok(3));
            assert_eq!(*harness.delivered.borrow(), vec![("a", 3)]);
        }

        #[test]
        fn permanent_failures_are_never_decoded_again() {
            let harness = Harness::new();
            let ticket = DecodeTicket::default();
            harness.request("a", &ticket);
            harness.complete("a", Err(DecodeError::Permanent("corrupt".into())));
            assert!(harness.executor.timer_delays().is_empty());
            assert!(harness.scheduler.is_failed(&"a"));
            assert!(matches!(harness.request("a", &ticket), Requested::Failed));
            harness.executor.advance(TRANSIENT_COOLDOWN * 10);
            assert!(matches!(harness.request("a", &ticket), Requested::Failed));
            assert!(harness.executor.running().is_empty());
        }

        #[test]
        fn transient_failures_retry_once_after_a_delay_then_cool_down() {
            let harness = Harness::new();
            let ticket = DecodeTicket::default();
            harness.request("a", &ticket);
            harness.complete("a", transient());
            assert!(harness.executor.running().is_empty());
            assert_eq!(harness.executor.timer_delays(), vec![RETRY_DELAY]);
            // Requests meanwhile join the pending retry.
            assert!(pending(harness.request("a", &ticket)));
            assert!(harness.executor.running().is_empty());

            harness.executor.fire_timers();
            assert_eq!(harness.executor.running(), vec!["a"]);
            harness.complete("a", transient());
            assert!(harness.executor.timer_delays().is_empty(), "no retry storm");
            assert!(matches!(harness.request("a", &ticket), Requested::Failed));

            harness.executor.advance(TRANSIENT_COOLDOWN);
            assert!(pending(harness.request("a", &ticket)));
            harness.complete("a", Ok(1));
            assert_eq!(*harness.delivered.borrow(), vec![("a", 1)]);
        }

        #[test]
        fn glib_errors_split_into_transient_and_permanent() {
            let missing = glib::Error::new(glib::FileError::Noent, "gone");
            assert!(matches!(
                classify_glib_error(&missing),
                DecodeError::Transient(_)
            ));
            let corrupt = glib::Error::new(gdk_pixbuf::PixbufError::CorruptImage, "bad");
            assert!(matches!(
                classify_glib_error(&corrupt),
                DecodeError::Permanent(_)
            ));
            let unsupported = glib::Error::new(gdk::TextureError::UnsupportedFormat, "?");
            assert!(matches!(
                classify_glib_error(&unsupported),
                DecodeError::Permanent(_)
            ));
        }

        fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
            let path = std::env::temp_dir().join(format!("conduit-{}-{name}", std::process::id()));
            std::fs::write(&path, bytes).expect("write temp image");
            path
        }

        #[test]
        fn oversized_header_is_rejected_from_the_bytes_without_decoding() {
            // A 1x1 GIF whose header declares a 30000x30000 canvas.
            let mut bomb = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;".to_vec();
            bomb[6..10].copy_from_slice(&[0x30, 0x75, 0x30, 0x75]);
            let path = temp_file("decode-bomb.gif", &bomb);
            let before = crate::debug::IMAGE_DECODES.get();
            let result = decode_texture_file(&path);
            std::fs::remove_file(&path).ok();
            assert!(matches!(result, Err(DecodeError::Permanent(_))));
            assert_eq!(crate::debug::IMAGE_DECODES.get(), before, "never decoded");
        }

        #[test]
        fn missing_files_are_transient_and_oversized_files_permanent() {
            let missing = std::env::temp_dir().join("conduit-decode-missing.png");
            assert!(matches!(
                decode_texture_file(&missing),
                Err(DecodeError::Transient(_))
            ));
            let path = temp_file("decode-large.bin", &[0u8; 64]);
            let result = read_capped(&path, 63);
            std::fs::remove_file(&path).ok();
            assert!(matches!(result, Err(DecodeError::Permanent(_))));
        }

        #[test]
        fn corrupt_images_fail_permanently() {
            crate::timeline::tests::run_gtk_test(|| {
                // A valid PNG header over garbage.
                let mut corrupt = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x10\0\0\0\x10".to_vec();
                corrupt.extend_from_slice(&[0x5a; 64]);
                let path = temp_file("decode-corrupt.png", &corrupt);
                let result = decode_texture_file(&path);
                std::fs::remove_file(&path).ok();
                assert!(
                    matches!(result, Err(DecodeError::Permanent(_))),
                    "{result:?}"
                );
            });
        }
    }
}
