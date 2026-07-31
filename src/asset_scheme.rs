/* asset_scheme.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

use crate::runtime::CachedAssetDescriptor;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::rc::Rc;

const ASSET_SCHEME: &str = "conduit-asset";
const ASSET_URI_PREFIX: &str = "conduit-asset://";
const CACHE_KEY_LENGTH: usize = 64;
const VALIDATION_PREFIX_LENGTH: usize = 64;
const PUBLIC_ERROR_MESSAGE: &str = "Asset unavailable";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AssetSchemeError {
    InvalidRequest,
    UnknownAsset,
    Unavailable,
}

impl AssetSchemeError {
    const fn public_message(self) -> &'static str {
        PUBLIC_ERROR_MESSAGE
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AssetSchemeInstallError;

impl std::fmt::Display for AssetSchemeInstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WebKit security manager unavailable")
    }
}

impl std::error::Error for AssetSchemeInstallError {}

/// Main-thread registry of cache descriptors that the WebKit asset scheme may serve.
///
/// `Rc<RefCell<_>>` intentionally keeps this registry on the GTK main thread while
/// allowing the URI-scheme callback and window state to share the same known keys.
#[derive(Clone, Default)]
pub(crate) struct AssetSchemeRegistry {
    known_assets: Rc<RefCell<HashMap<String, CachedAssetDescriptor>>>,
    installed_contexts: Rc<RefCell<Vec<glib::WeakRef<webkit6::WebContext>>>>,
}

impl AssetSchemeRegistry {
    pub(crate) fn install_on(
        &self,
        context: &webkit6::WebContext,
    ) -> Result<(), AssetSchemeInstallError> {
        let security_manager = context.security_manager().ok_or(AssetSchemeInstallError)?;
        if !claim_install(&self.installed_contexts, context) {
            return Ok(());
        }
        security_manager.register_uri_scheme_as_secure(ASSET_SCHEME);

        let registry = self.clone();
        context.register_uri_scheme(ASSET_SCHEME, move |request| {
            registry.finish_request(request);
        });
        Ok(())
    }

    pub(crate) fn insert(&self, asset: CachedAssetDescriptor) {
        self.known_assets
            .borrow_mut()
            .insert(asset.cache_key().to_owned(), asset);
    }

    pub(crate) fn clear(&self) {
        self.known_assets.borrow_mut().clear();
    }

    fn resolve_request(
        &self,
        method: Option<&str>,
        uri: Option<&str>,
    ) -> Result<CachedAssetDescriptor, AssetSchemeError> {
        let cache_key = parse_asset_request(method, uri)?;
        self.known_assets
            .borrow()
            .get(cache_key)
            .cloned()
            .ok_or(AssetSchemeError::UnknownAsset)
    }

    fn finish_request(&self, request: &webkit6::URISchemeRequest) {
        let method = request.http_method();
        let uri = request.uri();
        let opened = self
            .resolve_request(method.as_deref(), uri.as_deref())
            .and_then(|asset| open_asset(&asset));

        match opened {
            Ok(OpenedAsset {
                file,
                length,
                content_type,
            }) => {
                // SAFETY: `file` is the sole owner of this descriptor. Ownership is
                // transferred to the GInputStream, which closes it when released.
                let stream = unsafe { gio::UnixInputStream::take_fd(file) };
                request.finish(&stream, length, Some(content_type));
            }
            Err(error) => {
                let mut public_error =
                    glib::Error::new(gio::IOErrorEnum::Failed, error.public_message());
                request.finish_error(&mut public_error);
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.known_assets.borrow().len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.known_assets.borrow().is_empty()
    }
}

fn claim_install<T: glib::object::ObjectType>(
    installed_objects: &RefCell<Vec<glib::WeakRef<T>>>,
    object: &T,
) -> bool {
    let mut installed_objects = installed_objects.borrow_mut();
    installed_objects.retain(|weak| weak.upgrade().is_some());
    if installed_objects.iter().any(|weak| weak == object) {
        return false;
    }
    let weak = glib::WeakRef::new();
    weak.set(Some(object));
    installed_objects.push(weak);
    true
}

impl std::fmt::Debug for AssetSchemeRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AssetSchemeRegistry")
            .field("known_assets", &self.known_assets.borrow().len())
            .finish()
    }
}

fn parse_asset_request<'a>(
    method: Option<&str>,
    uri: Option<&'a str>,
) -> Result<&'a str, AssetSchemeError> {
    if method != Some("GET") {
        return Err(AssetSchemeError::InvalidRequest);
    }
    let remainder = uri
        .and_then(|uri| uri.strip_prefix(ASSET_URI_PREFIX))
        .ok_or(AssetSchemeError::InvalidRequest)?;
    let cache_key = match remainder.len() {
        CACHE_KEY_LENGTH => remainder,
        length if length == CACHE_KEY_LENGTH + 1 && remainder.ends_with('/') => {
            &remainder[..CACHE_KEY_LENGTH]
        }
        _ => return Err(AssetSchemeError::InvalidRequest),
    };
    if !cache_key
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AssetSchemeError::InvalidRequest);
    }
    Ok(cache_key)
}

#[derive(Debug)]
struct OpenedAsset {
    file: File,
    length: i64,
    content_type: &'static str,
}

fn open_asset(asset: &CachedAssetDescriptor) -> Result<OpenedAsset, AssetSchemeError> {
    open_asset_with_pre_open(asset, || {})
}

fn open_asset_with_pre_open(
    asset: &CachedAssetDescriptor,
    before_open: impl FnOnce(),
) -> Result<OpenedAsset, AssetSchemeError> {
    let path_metadata = asset
        .path()
        .symlink_metadata()
        .map_err(|_| AssetSchemeError::Unavailable)?;
    if !path_metadata.is_file() {
        return Err(AssetSchemeError::Unavailable);
    }

    before_open();
    let mut file = File::open(asset.path()).map_err(|_| AssetSchemeError::Unavailable)?;
    let metadata = file.metadata().map_err(|_| AssetSchemeError::Unavailable)?;
    if !metadata.is_file() || !same_file_identity(&path_metadata, &metadata) {
        return Err(AssetSchemeError::Unavailable);
    }

    let prefix_length = metadata.len().min(VALIDATION_PREFIX_LENGTH as u64) as usize;
    let mut prefix = [0_u8; VALIDATION_PREFIX_LENGTH];
    file.read_exact(&mut prefix[..prefix_length])
        .map_err(|_| AssetSchemeError::Unavailable)?;

    // Re-read metadata from the already opened handle after the bounded read.
    // This avoids validating a pathname that could have been replaced in between.
    let validated_metadata = file.metadata().map_err(|_| AssetSchemeError::Unavailable)?;
    let current_path_metadata = asset
        .path()
        .symlink_metadata()
        .map_err(|_| AssetSchemeError::Unavailable)?;
    if !validated_metadata.is_file()
        || !current_path_metadata.is_file()
        || !same_file_identity(&metadata, &validated_metadata)
        || !same_file_identity(&validated_metadata, &current_path_metadata)
        || !asset.validates_opened_content(validated_metadata.len(), &prefix[..prefix_length])
    {
        return Err(AssetSchemeError::Unavailable);
    }
    let length =
        i64::try_from(validated_metadata.len()).map_err(|_| AssetSchemeError::Unavailable)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|_| AssetSchemeError::Unavailable)?;

    Ok(OpenedAsset {
        file,
        length,
        content_type: asset.content_type(),
    })
}

fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::CachedAssetDescriptor;
    use crate::slack::PreviewAssetMime;
    use std::fs;
    use std::io::{Read, Seek, SeekFrom};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\npreview";
    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestAsset {
        directory: PathBuf,
        descriptor: CachedAssetDescriptor,
    }

    impl TestAsset {
        fn png(cache_key: &str, bytes: &[u8]) -> Self {
            let directory = unique_temp_directory();
            fs::create_dir_all(&directory).expect("create test asset directory");
            let path = directory.join(format!("{cache_key}.png"));
            fs::write(&path, bytes).expect("write test asset");
            let descriptor = CachedAssetDescriptor::for_test(
                cache_key.to_owned(),
                path,
                PreviewAssetMime::Png,
                bytes.len() as u64,
            )
            .expect("construct test descriptor");
            Self {
                directory,
                descriptor,
            }
        }

        fn path(&self) -> &Path {
            self.descriptor.path()
        }
    }

    impl Drop for TestAsset {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn unique_temp_directory() -> PathBuf {
        let sequence = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "conduit-asset-scheme-test-{}-{sequence}",
            std::process::id()
        ))
    }

    fn key(character: char) -> String {
        std::iter::repeat_n(character, 64).collect()
    }

    #[test]
    fn request_parser_accepts_only_exact_get_cache_key_uris() {
        let cache_key = key('a');
        let uri = format!("conduit-asset://{cache_key}");
        assert_eq!(
            parse_asset_request(Some("GET"), Some(&uri)),
            Ok(cache_key.as_str())
        );
        let canonical_uri = format!("{uri}/");
        assert_eq!(
            parse_asset_request(Some("GET"), Some(&canonical_uri)),
            Ok(cache_key.as_str())
        );

        let invalid_requests = [
            (Some("POST"), Some(uri.as_str())),
            (Some("get"), Some(uri.as_str())),
            (None, Some(uri.as_str())),
            (Some("GET"), None),
            (
                Some("GET"),
                Some(
                    "Conduit-asset://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
            ),
            (
                Some("GET"),
                Some(
                    "conduit-asset://AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                ),
            ),
            (
                Some("GET"),
                Some(
                    "conduit-asset://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
            ),
            (
                Some("GET"),
                Some(
                    "conduit-asset://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
            ),
            (
                Some("GET"),
                Some(
                    "conduit-asset://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa//",
                ),
            ),
            (
                Some("GET"),
                Some(
                    "conduit-asset://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/child",
                ),
            ),
            (
                Some("GET"),
                Some(
                    "conduit-asset://user@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
            ),
            (
                Some("GET"),
                Some(
                    "conduit-asset://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:443",
                ),
            ),
            (
                Some("GET"),
                Some(
                    "conduit-asset://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?download=1",
                ),
            ),
            (
                Some("GET"),
                Some(
                    "conduit-asset://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#preview",
                ),
            ),
            (
                Some("GET"),
                Some(
                    "conduit-asset://%61aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ),
            ),
        ];
        for (method, candidate) in invalid_requests {
            assert_eq!(
                parse_asset_request(method, candidate),
                Err(AssetSchemeError::InvalidRequest),
                "accepted invalid request {method:?} {candidate:?}"
            );
        }
    }

    #[test]
    fn registry_resolves_only_descriptors_it_was_given() {
        let asset = TestAsset::png(&key('a'), PNG_BYTES);
        let registry = AssetSchemeRegistry::default();
        let known_uri = asset.descriptor.uri();
        let unknown_uri = format!("conduit-asset://{}", key('b'));

        assert_eq!(
            registry.resolve_request(Some("GET"), Some(&known_uri)),
            Err(AssetSchemeError::UnknownAsset)
        );
        registry.insert(asset.descriptor.clone());
        assert_eq!(
            registry
                .resolve_request(Some("GET"), Some(&known_uri))
                .expect("resolve registered asset"),
            asset.descriptor
        );
        assert_eq!(
            registry.resolve_request(Some("GET"), Some(&unknown_uri)),
            Err(AssetSchemeError::UnknownAsset)
        );
    }

    #[test]
    fn registry_clones_share_known_keys_and_clear_them() {
        let asset = TestAsset::png(&key('a'), PNG_BYTES);
        let registry = AssetSchemeRegistry::default();
        let clone = registry.clone();
        registry.insert(asset.descriptor.clone());

        assert_eq!(clone.len(), 1);
        clone.clear();
        assert!(registry.is_empty());
    }

    #[test]
    fn opening_a_registered_asset_revalidates_and_rewinds_the_file() {
        let asset = TestAsset::png(&key('a'), PNG_BYTES);
        let mut opened = open_asset(&asset.descriptor).expect("open valid asset");

        assert_eq!(opened.length, PNG_BYTES.len() as i64);
        assert_eq!(opened.content_type, "image/png");
        assert_eq!(opened.file.stream_position().expect("stream position"), 0);
        let mut bytes = Vec::new();
        opened
            .file
            .read_to_end(&mut bytes)
            .expect("read opened asset");
        assert_eq!(bytes, PNG_BYTES);
    }

    #[test]
    fn opening_rejects_missing_non_regular_changed_and_malformed_assets() {
        let missing = TestAsset::png(&key('a'), PNG_BYTES);
        fs::remove_file(missing.path()).expect("remove cached asset");
        assert!(matches!(
            open_asset(&missing.descriptor),
            Err(AssetSchemeError::Unavailable)
        ));

        let malformed = TestAsset::png(&key('b'), b"<script>alert(1)</script>");
        assert!(matches!(
            open_asset(&malformed.descriptor),
            Err(AssetSchemeError::Unavailable)
        ));

        let changed = TestAsset::png(&key('c'), PNG_BYTES);
        fs::write(changed.path(), b"\x89PNG\r\n\x1a\nchanged-size").expect("replace cached asset");
        assert!(matches!(
            open_asset(&changed.descriptor),
            Err(AssetSchemeError::Unavailable)
        ));

        let directory = unique_temp_directory();
        fs::create_dir_all(&directory).expect("create non-regular test root");
        let cache_key = key('d');
        let path = directory.join(format!("{cache_key}.png"));
        fs::create_dir(&path).expect("create directory at cache path");
        let descriptor = CachedAssetDescriptor::for_test(
            cache_key,
            path,
            PreviewAssetMime::Png,
            PNG_BYTES.len() as u64,
        )
        .expect("construct non-regular descriptor");
        assert!(matches!(
            open_asset(&descriptor),
            Err(AssetSchemeError::Unavailable)
        ));
        fs::remove_dir_all(directory).expect("remove non-regular test root");
    }

    #[cfg(unix)]
    #[test]
    fn opening_rejects_a_symlink_at_the_registered_path() {
        use std::os::unix::fs::symlink;

        let asset = TestAsset::png(&key('e'), PNG_BYTES);
        let target = asset.directory.join("target.png");
        fs::rename(asset.path(), &target).expect("move cached asset");
        symlink(&target, asset.path()).expect("replace cache path with symlink");

        assert!(matches!(
            open_asset(&asset.descriptor),
            Err(AssetSchemeError::Unavailable)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn opening_rejects_path_replacement_between_lstat_and_open() {
        let asset = TestAsset::png(&key('f'), PNG_BYTES);
        let original = asset.directory.join("original.png");

        let result = open_asset_with_pre_open(&asset.descriptor, || {
            fs::rename(asset.path(), &original).expect("move original asset");
            fs::write(asset.path(), PNG_BYTES).expect("replace asset with a new inode");
        });

        assert!(matches!(result, Err(AssetSchemeError::Unavailable)));
    }

    #[test]
    fn install_claim_is_idempotent_and_drops_dead_contexts() {
        let installs = RefCell::new(Vec::new());
        let first = gio::SimpleAction::new("first", None);

        assert!(claim_install(&installs, &first));
        assert!(!claim_install(&installs, &first));
        assert_eq!(installs.borrow().len(), 1);

        drop(first);
        let second = gio::SimpleAction::new("second", None);
        assert!(claim_install(&installs, &second));
        assert_eq!(installs.borrow().len(), 1);
    }

    #[test]
    fn all_request_failures_use_one_safe_public_message() {
        assert_eq!(
            AssetSchemeError::InvalidRequest.public_message(),
            "Asset unavailable"
        );
        assert_eq!(
            AssetSchemeError::UnknownAsset.public_message(),
            "Asset unavailable"
        );
        assert_eq!(
            AssetSchemeError::Unavailable.public_message(),
            "Asset unavailable"
        );
    }

    #[test]
    fn registry_debug_output_does_not_disclose_cache_keys_or_paths() {
        let asset = TestAsset::png(&key('a'), PNG_BYTES);
        let path = asset.path().display().to_string();
        let cache_key = asset.descriptor.cache_key().to_owned();
        let registry = AssetSchemeRegistry::default();
        registry.insert(asset.descriptor.clone());

        let debug = format!("{registry:?}");
        assert!(debug.contains("known_assets: 1"));
        assert!(!debug.contains(&path));
        assert!(!debug.contains(&cache_key));
    }

    #[test]
    fn seek_import_is_exercised_for_the_opened_handle() {
        let asset = TestAsset::png(&key('b'), PNG_BYTES);
        let mut opened = open_asset(&asset.descriptor).expect("open valid asset");
        opened.file.seek(SeekFrom::End(0)).expect("seek asset");
        assert_eq!(
            opened.file.stream_position().expect("stream position"),
            PNG_BYTES.len() as u64
        );
    }
}
