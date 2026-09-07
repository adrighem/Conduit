use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::rc::Rc;

use gtk::gio::{self, prelude::*};
use gtk::glib;

use crate::config;
use crate::message_html::CachedAssetSource;
use crate::runtime::CachedAssetDescriptor;

pub const CONDUIT_ASSET_REGISTRY_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const CONDUIT_ASSET_REGISTRY_MAX_ENTRIES: usize = 2_048;
pub const CONDUIT_ASSET_VALIDATION_PREFIX_BYTES: u64 = 64;
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

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageAssetRecoveryAction {
    Retry,
    AlreadyPending,
    Fail,
}

pub fn image_asset_recovery_action(
    recovering: &mut BoundedImageAssetKeys,
    pending: &mut BoundedImageAssetKeys,
    key: &str,
) -> ImageAssetRecoveryAction {
    if !recovering.try_insert(key.to_string()) {
        return ImageAssetRecoveryAction::Fail;
    }
    if pending.try_insert(key.to_string()) {
        ImageAssetRecoveryAction::Retry
    } else if pending.contains(key) {
        ImageAssetRecoveryAction::AlreadyPending
    } else {
        ImageAssetRecoveryAction::Fail
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

pub fn conduit_asset_for_request(
    uri: &str,
    assets: &mut BoundedConduitAssets,
) -> Option<CachedAssetDescriptor> {
    let key = conduit_asset_request_key(uri)?;
    assets.get(&key)
}

pub fn cached_asset_source_is_registered(
    source: &CachedAssetSource,
    assets: &BoundedConduitAssets,
) -> bool {
    conduit_asset_request_key(source.uri()).is_some_and(|key| assets.contains_key(&key))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConduitAssetResponsePlan {
    Full,
    Partial { start: u64, end: u64 },
    NotSatisfiable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConduitAssetServeOutcome {
    Rejected,
    Served(String),
    Invalidated(String),
}

pub fn conduit_asset_request_method(method: Option<&str>) -> Option<&str> {
    match method {
        Some(method @ ("GET" | "HEAD")) => Some(method),
        _ => None,
    }
}

pub fn conduit_asset_response_plan(range: Option<&str>, size: u64) -> ConduitAssetResponsePlan {
    let Some(range) = range else {
        return ConduitAssetResponsePlan::Full;
    };
    let Some(specification) = range.trim().strip_prefix("bytes=") else {
        return ConduitAssetResponsePlan::NotSatisfiable;
    };
    if specification.contains(',') || size == 0 {
        return ConduitAssetResponsePlan::NotSatisfiable;
    }
    let Some((start, end)) = specification.split_once('-') else {
        return ConduitAssetResponsePlan::NotSatisfiable;
    };
    let start = start.trim();
    let end = end.trim();
    if start.is_empty() {
        let Ok(suffix_length) = end.parse::<u64>() else {
            return ConduitAssetResponsePlan::NotSatisfiable;
        };
        if suffix_length == 0 {
            return ConduitAssetResponsePlan::NotSatisfiable;
        }
        let suffix_length = suffix_length.min(size);
        return ConduitAssetResponsePlan::Partial {
            start: size - suffix_length,
            end: size - 1,
        };
    }

    let Ok(start) = start.parse::<u64>() else {
        return ConduitAssetResponsePlan::NotSatisfiable;
    };
    if start >= size {
        return ConduitAssetResponsePlan::NotSatisfiable;
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        let Ok(end) = end.parse::<u64>() else {
            return ConduitAssetResponsePlan::NotSatisfiable;
        };
        if end < start {
            return ConduitAssetResponsePlan::NotSatisfiable;
        }
        end.min(size - 1)
    };
    ConduitAssetResponsePlan::Partial { start, end }
}

#[cfg(unix)]
fn same_opened_file(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_opened_file(left: &Metadata, right: &Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok().is_some()
        && left.modified().ok() == right.modified().ok()
}

pub fn open_conduit_asset(descriptor: &CachedAssetDescriptor) -> io::Result<File> {
    open_conduit_asset_at(descriptor, &config::image_asset_cache_dir())
}

pub fn open_conduit_asset_at(
    descriptor: &CachedAssetDescriptor,
    cache_root: &Path,
) -> io::Result<File> {
    let path = descriptor.path_in(cache_root);
    let before = std::fs::symlink_metadata(&path)?;
    if !before.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cached asset",
        ));
    }
    #[cfg(unix)]
    if before.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cached asset",
        ));
    }

    let mut file = File::open(&path)?;
    let opened = file.metadata()?;
    let after = std::fs::symlink_metadata(&path)?;
    if !opened.is_file()
        || !after.file_type().is_file()
        || !same_opened_file(&before, &opened)
        || !same_opened_file(&opened, &after)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cached asset",
        ));
    }

    let mut prefix =
        Vec::with_capacity(opened.len().min(CONDUIT_ASSET_VALIDATION_PREFIX_BYTES) as usize);
    file.by_ref()
        .take(CONDUIT_ASSET_VALIDATION_PREFIX_BYTES)
        .read_to_end(&mut prefix)?;
    let validated = file.metadata()?;
    if !same_opened_file(&opened, &validated)
        || !descriptor.validates_opened_content(validated.len(), &prefix)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cached asset",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

pub fn finish_conduit_asset_error(request: &webkit6::URISchemeRequest) {
    let mut error = glib::Error::new(
        gio::IOErrorEnum::NotFound,
        "unknown or invalid Conduit asset",
    );
    request.finish_error(&mut error);
}

pub fn finish_conduit_asset_response(
    request: &webkit6::URISchemeRequest,
    descriptor: &CachedAssetDescriptor,
    mut file: File,
    method: &str,
    plan: ConduitAssetResponsePlan,
) -> io::Result<()> {
    let total_length = descriptor.size();
    let (status, reason, start, end, content_length) = match plan {
        ConduitAssetResponsePlan::Full => {
            (200, "OK", 0, total_length.saturating_sub(1), total_length)
        }
        ConduitAssetResponsePlan::Partial { start, end } => {
            (206, "Partial Content", start, end, end - start + 1)
        }
        ConduitAssetResponsePlan::NotSatisfiable => {
            let stream = gio::MemoryInputStream::new();
            let response = webkit6::URISchemeResponse::new(&stream, 0);
            response.set_status(416, Some("Range Not Satisfiable"));
            response.set_content_type(descriptor.content_type());
            let headers =
                webkit6::soup::MessageHeaders::new(webkit6::soup::MessageHeadersType::Response);
            headers.replace("Accept-Ranges", "bytes");
            headers.replace("Cache-Control", "no-store");
            headers.replace("Content-Length", "0");
            headers.replace("Content-Range", &format!("bytes */{total_length}"));
            headers.replace("X-Content-Type-Options", "nosniff");
            response.set_http_headers(headers);
            request.finish_with_response(&response);
            return Ok(());
        }
    };

    let stream: gio::InputStream = if method == "HEAD" {
        gio::MemoryInputStream::new().upcast()
    } else {
        file.seek(SeekFrom::Start(start))?;
        gio::ReadInputStream::new(file.take(content_length)).upcast()
    };
    let stream_length = if method == "HEAD" {
        0
    } else {
        content_length as i64
    };
    let response = webkit6::URISchemeResponse::new(&stream, stream_length);
    response.set_status(status, Some(reason));
    response.set_content_type(descriptor.content_type());
    let headers = webkit6::soup::MessageHeaders::new(webkit6::soup::MessageHeadersType::Response);
    headers.replace("Accept-Ranges", "bytes");
    headers.replace("Cache-Control", "no-store");
    headers.replace("Content-Length", &content_length.to_string());
    headers.replace("X-Content-Type-Options", "nosniff");
    if status == 206 {
        headers.replace(
            "Content-Range",
            &format!("bytes {start}-{end}/{total_length}"),
        );
    }
    response.set_http_headers(headers);
    request.finish_with_response(&response);
    Ok(())
}

pub fn serve_conduit_asset_request(
    request: &webkit6::URISchemeRequest,
    assets: &Rc<RefCell<BoundedConduitAssets>>,
) -> ConduitAssetServeOutcome {
    let Some(uri) = request.uri() else {
        finish_conduit_asset_error(request);
        return ConduitAssetServeOutcome::Rejected;
    };
    let Some(cache_key) = conduit_asset_request_key(uri.as_str()) else {
        finish_conduit_asset_error(request);
        return ConduitAssetServeOutcome::Rejected;
    };
    let Some(descriptor) = conduit_asset_for_request(uri.as_str(), &mut assets.borrow_mut()) else {
        finish_conduit_asset_error(request);
        return ConduitAssetServeOutcome::Rejected;
    };
    let method = request.http_method();
    let Some(method) = conduit_asset_request_method(method.as_deref()) else {
        finish_conduit_asset_error(request);
        return ConduitAssetServeOutcome::Rejected;
    };
    let range = request
        .http_headers()
        .and_then(|headers| headers.one("Range"))
        .map(|range| range.to_string());
    let plan = conduit_asset_response_plan(range.as_deref(), descriptor.size());
    let Ok(file) = open_conduit_asset(&descriptor) else {
        assets.borrow_mut().remove(&cache_key);
        finish_conduit_asset_error(request);
        return ConduitAssetServeOutcome::Invalidated(cache_key);
    };
    if finish_conduit_asset_response(request, &descriptor, file, method, plan).is_err() {
        assets.borrow_mut().remove(&cache_key);
        finish_conduit_asset_error(request);
        return ConduitAssetServeOutcome::Invalidated(cache_key);
    }
    ConduitAssetServeOutcome::Served(cache_key)
}
