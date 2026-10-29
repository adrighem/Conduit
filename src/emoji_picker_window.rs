/* emoji_picker_window.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use adw::prelude::*;
use gettextrs::gettext;
use gtk::gdk::Key;
use gtk::{gio, glib};
use sha2::{Digest, Sha256};

use crate::emoji::{EmojiCatalog, EmojiEntry, EmojiValue};
use crate::timeline::cache::decode::{self, DecodeTicket, Decoded, Requested, TextureSpec};

const CATEGORY_ICONS: &[(&str, &str)] = &[
    ("All", "🌟"),
    ("Smileys", "😀"),
    ("People", "👋"),
    ("Nature", "🌿"),
    ("Food", "🍔"),
    ("Travel", "🚗"),
    ("Activities", "⚽"),
    ("Objects", "💡"),
    ("Symbols", "🔣"),
    ("Flags", "🚩"),
    ("Workspace", "💬"),
];

pub(crate) struct EmojiPickerWindow {
    window: adw::Window,
}

const MAX_CUSTOM_EMOJI_BYTES: usize = 8 * 1024 * 1024;
const CUSTOM_EMOJI_EXTENSIONS: &[&str] = &["png", "gif", "jpg", "jpeg", "webp"];

static RESOLVED_CACHE_MAP: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();

fn resolved_cache_map() -> &'static Mutex<HashMap<String, PathBuf>> {
    RESOLVED_CACHE_MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn custom_emoji_cache_dir() -> PathBuf {
    let image_cache_dir = crate::config::image_asset_cache_dir();
    image_cache_dir.parent().map_or_else(
        || image_cache_dir.join("custom-emoji"),
        |parent| parent.join("custom-emoji"),
    )
}

fn is_non_empty_file(path: &Path) -> bool {
    path.is_file() && path.metadata().map(|m| m.len() > 0).unwrap_or(false)
}

pub(crate) fn invalidate_or_set_custom_emoji_cached_file(url: &str, path: PathBuf) {
    if let Ok(mut map) = resolved_cache_map().lock() {
        if is_non_empty_file(&path) {
            map.insert(url.to_string(), path);
        } else {
            map.remove(url);
        }
    }
}

pub(crate) fn resolve_custom_emoji_cached_file(url: &str) -> Option<PathBuf> {
    if let Ok(mut map) = resolved_cache_map().lock() {
        if let Some(cached) = map.get(url) {
            if is_non_empty_file(cached) {
                return Some(cached.clone());
            }
            map.remove(url);
        }
    }

    let resolved = resolve_custom_emoji_cached_file_disk(url);
    if let Some(ref path) = resolved {
        if let Ok(mut map) = resolved_cache_map().lock() {
            map.insert(url.to_string(), path.clone());
        }
    }
    resolved
}

fn custom_emoji_url_hash(url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn find_hashed_emoji_in_dir(dir: &Path, hash: &str) -> Option<PathBuf> {
    for ext in CUSTOM_EMOJI_EXTENSIONS {
        let p = dir.join(format!("{hash}.{ext}"));
        if is_non_empty_file(&p) {
            return Some(p);
        }
    }
    None
}

fn resolve_custom_emoji_cached_file_disk(url: &str) -> Option<PathBuf> {
    let hash = custom_emoji_url_hash(url);
    let emoji_dir = custom_emoji_cache_dir();
    if let Some(found) = find_hashed_emoji_in_dir(&emoji_dir, &hash) {
        return Some(found);
    }

    let cache_dir = crate::config::image_asset_cache_dir();
    if let Some(found) = find_hashed_emoji_in_dir(&cache_dir, &hash) {
        return Some(found);
    }
    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find_hashed_emoji_in_dir(&path, &hash) {
                    return Some(found);
                }
                if let Some(workspace_dir_name) = entry.file_name().to_str() {
                    let ws_hash = {
                        let mut hasher = Sha256::new();
                        hasher.update(workspace_dir_name.as_bytes());
                        hasher.update([0]);
                        hasher.update(url.as_bytes());
                        format!("{:x}", hasher.finalize())
                    };
                    if let Some(found) = find_hashed_emoji_in_dir(&path, &ws_hash) {
                        return Some(found);
                    }
                }
            }
        }
    }
    None
}

type DownloadCallback = Box<dyn FnOnce(PathBuf) + Send + 'static>;

struct DownloadRequest {
    url: String,
    callback: Option<DownloadCallback>,
}

/// Caps how many distinct URLs the "don't retry this session" failure set
/// can hold, so a workspace with many broken custom-emoji URLs over a long
/// session can't grow it unboundedly. Mirrors the eviction style of
/// `BoundedTextureCache` in `src/timeline/cache.rs`.
const MAX_FAILED_EMOJI_URLS: usize = 256;

struct LoaderState {
    queue: VecDeque<DownloadRequest>,
    in_flight: HashMap<String, Vec<DownloadCallback>>,
    failed: HashSet<String>,
    failed_order: VecDeque<String>,
}

impl LoaderState {
    /// Records a permanent-for-this-session download failure, evicting the
    /// oldest entry once `MAX_FAILED_EMOJI_URLS` is exceeded.
    fn mark_failed(&mut self, url: String) {
        if self.failed.insert(url.clone()) {
            self.failed_order.push_back(url);
            if self.failed_order.len() > MAX_FAILED_EMOJI_URLS {
                if let Some(oldest) = self.failed_order.pop_front() {
                    self.failed.remove(&oldest);
                }
            }
        }
    }
}

struct CustomEmojiLoader {
    state: Arc<Mutex<LoaderState>>,
    notify: Arc<tokio::sync::Notify>,
}

impl CustomEmojiLoader {
    fn new() -> Self {
        let state = Arc::new(Mutex::new(LoaderState {
            queue: VecDeque::new(),
            in_flight: HashMap::new(),
            failed: HashSet::new(),
            failed_order: VecDeque::new(),
        }));
        let notify = Arc::new(tokio::sync::Notify::new());

        let state_clone = Arc::clone(&state);
        let notify_clone = Arc::clone(&notify);

        std::thread::Builder::new()
            .name("conduit-emoji-loader".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => return,
                };

                let client = crate::http_client::client().clone();

                rt.block_on(async move {
                    const NUM_WORKERS: usize = 6;
                    let mut handles = Vec::new();

                    for _ in 0..NUM_WORKERS {
                        let state = Arc::clone(&state_clone);
                        let notify = Arc::clone(&notify_clone);
                        let client = client.clone();

                        handles.push(tokio::spawn(async move {
                            loop {
                                let notified = notify.notified();
                                // Dequeue under the lock, but do the (possibly
                                // slow, disk-scanning) cache resolution below
                                // with the lock released: `LoaderState`'s
                                // mutex is shared with every other worker and
                                // with `queue()` on the GTK main thread, so
                                // holding it across filesystem I/O would
                                // serialize all of them behind one scan.
                                let next_url = loop {
                                    let req = {
                                        let mut s = state.lock().unwrap();
                                        let Some(req) = s.queue.pop_front() else {
                                            break None;
                                        };
                                        if s.failed.contains(&req.url) {
                                            // Marked failed after being
                                            // queued (or re-queued before
                                            // `queue()` learned of the
                                            // failure): drop it rather than
                                            // retry within this session.
                                            continue;
                                        }
                                        req
                                    };

                                    if let Some(path) = resolve_custom_emoji_cached_file(&req.url) {
                                        if let Some(cb) = req.callback {
                                            cb(path);
                                        }
                                        continue;
                                    }

                                    let mut s = state.lock().unwrap();
                                    if let Some(cb) = req.callback {
                                        if let Some(cbs) = s.in_flight.get_mut(&req.url) {
                                            cbs.push(cb);
                                            continue;
                                        } else {
                                            s.in_flight.insert(req.url.clone(), vec![cb]);
                                        }
                                    } else if s.in_flight.contains_key(&req.url) {
                                        continue;
                                    } else {
                                        s.in_flight.insert(req.url.clone(), Vec::new());
                                    }

                                    break Some(req.url);
                                };

                                let Some(url) = next_url else {
                                    notified.await;
                                    continue;
                                };

                                let res = download_custom_emoji_file(&client, &url).await;

                                let (callbacks, is_ok) = {
                                    let mut s = state.lock().unwrap();
                                    let cbs = s.in_flight.remove(&url).unwrap_or_default();
                                    if res.is_none() {
                                        s.mark_failed(url.clone());
                                    }
                                    (cbs, res)
                                };

                                if let Some(path) = is_ok {
                                    for cb in callbacks {
                                        cb(path.clone());
                                    }
                                }
                            }
                        }));
                    }

                    futures_util::future::join_all(handles).await;
                });
            })
            .expect("spawn emoji loader thread");

        Self { state, notify }
    }

    fn queue(&self, url: String, high_priority: bool, callback: Option<DownloadCallback>) {
        if let Some(path) = resolve_custom_emoji_cached_file(&url) {
            if let Some(cb) = callback {
                cb(path);
            }
            return;
        }

        let mut s = self.state.lock().unwrap();
        if s.failed.contains(&url) {
            // Already failed once this session (404, invalid URL, etc.):
            // skip re-fetching rather than hammering the server on every
            // row bind/recycle that references this emoji. Cleared only by
            // an app restart (the set is in-memory); the codebase doesn't
            // otherwise distinguish permanent vs. transient failures, so
            // this is the simplest safe policy.
            return;
        }

        if let Some(cbs) = s.in_flight.get_mut(&url) {
            if let Some(cb) = callback {
                cbs.push(cb);
            }
            return;
        }

        if let Some(pos) = s.queue.iter().position(|r| r.url == url) {
            let mut req = s.queue.remove(pos).unwrap();
            if let Some(cb) = callback {
                match req.callback.take() {
                    Some(prev) => {
                        req.callback = Some(Box::new(move |p| {
                            prev(p.clone());
                            cb(p);
                        }));
                    }
                    None => {
                        req.callback = Some(cb);
                    }
                }
            }
            if high_priority {
                s.queue.push_front(req);
            } else {
                s.queue.push_back(req);
            }
            drop(s);
            self.notify.notify_waiters();
            return;
        }

        let req = DownloadRequest { url, callback };
        if high_priority {
            s.queue.push_front(req);
        } else {
            s.queue.push_back(req);
        }
        drop(s);
        self.notify.notify_waiters();
    }
}

fn is_valid_custom_emoji_url(url: &str) -> bool {
    if crate::slack::supports_native_preview_asset_url(url) {
        return true;
    }
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && crate::http_client::is_public_domain(&parsed)
}

fn extension_for_content_type(content_type: Option<&str>) -> &'static str {
    let Some(ct) = content_type else {
        return "png";
    };
    let lower = ct.to_ascii_lowercase();
    if lower.contains("gif") {
        "gif"
    } else if lower.contains("webp") {
        "webp"
    } else if lower.contains("jpeg") || lower.contains("jpg") {
        "jpg"
    } else {
        "png"
    }
}

async fn download_custom_emoji_file(client: &reqwest::Client, url: &str) -> Option<PathBuf> {
    if !is_valid_custom_emoji_url(url) {
        return None;
    }

    let mut req = client.get(url);
    if let Some(ua) = crate::config::slack_user_agent() {
        req = req.header(reqwest::header::USER_AGENT, ua);
    } else {
        req = req.header(
            reqwest::header::USER_AGENT,
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0.0.0 Safari/537.36",
        );
    }

    let mut resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    if resp
        .content_length()
        .is_some_and(|len| len == 0 || len > MAX_CUSTOM_EMOJI_BYTES as u64)
    {
        return None;
    }

    let ext = extension_for_content_type(
        resp.headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
    );

    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.ok()? {
        if bytes.len().saturating_add(chunk.len()) > MAX_CUSTOM_EMOJI_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return None;
    }

    let cache_dir = custom_emoji_cache_dir();
    let _ = std::fs::create_dir_all(&cache_dir);

    let hash = custom_emoji_url_hash(url);

    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let tmp_name = format!(
        "{hash}.tmp.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let tmp_path = cache_dir.join(tmp_name);
    let final_path = cache_dir.join(format!("{hash}.{ext}"));

    if std::fs::write(&tmp_path, &bytes).is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        return None;
    }

    if std::fs::rename(&tmp_path, &final_path).is_err() {
        let _ = std::fs::remove_file(&tmp_path);
        return None;
    }

    invalidate_or_set_custom_emoji_cached_file(url, final_path.clone());
    Some(final_path)
}

static LOADER: OnceLock<CustomEmojiLoader> = OnceLock::new();

fn emoji_loader() -> &'static CustomEmojiLoader {
    LOADER.get_or_init(CustomEmojiLoader::new)
}

/// Queues a high-priority download of a custom emoji image through the shared
/// loader. `on_ready` runs on the loader thread once the file is cached.
pub(crate) fn request_custom_emoji_file(
    url: &str,
    on_ready: impl FnOnce(PathBuf) + Send + 'static,
) {
    emoji_loader().queue(url.to_string(), true, Some(Box::new(on_ready)));
}

/// First frames of the animated custom emoji seen so far, by URL. A cell
/// shows one at once, and its presence marks the emoji as animated.
type Stills = Rc<RefCell<HashMap<String, gtk::gdk::Texture>>>;

/// One recycled grid cell. Its image is decoded off the GTK thread, and only
/// while the cell is mapped: unmapping, unbinding or rebinding it makes any
/// decode it still waits on stale, so a late result never lands on a cell
/// that has moved on to another emoji.
struct EmojiSlot {
    container: gtk::Box,
    stack: gtk::Stack,
    pic: gtk::Picture,
    stills: Stills,
    /// The cached file of the custom emoji the cell is bound to.
    file: RefCell<Option<PathBuf>>,
    /// Waits on the image the cell shows.
    load: DecodeTicket,
    /// Waits on the animation a hover plays.
    hover: DecodeTicket,
}

impl EmojiSlot {
    /// A cell of the grid: a stack of the unicode text, the image and a
    /// placeholder, which plays an animated emoji while hovered.
    fn build(stills: &Stills) -> Rc<Self> {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.set_size_request(38, 38);
        container.set_halign(gtk::Align::Center);
        container.set_valign(gtk::Align::Center);

        let stack = gtk::Stack::new();
        stack.set_size_request(36, 36);
        stack.set_transition_type(gtk::StackTransitionType::Crossfade);

        let lbl = gtk::Label::new(None);
        lbl.add_css_class("title-3");
        lbl.set_size_request(36, 36);
        stack.add_named(&lbl, Some("text"));

        let pic = gtk::Picture::new();
        pic.set_size_request(32, 32);
        pic.set_can_shrink(true);
        pic.set_content_fit(gtk::ContentFit::Cover);
        stack.add_named(&pic, Some("pic"));

        let placeholder = gtk::Label::new(Some("💬"));
        placeholder.add_css_class("dim-label");
        placeholder.set_size_request(36, 36);
        stack.add_named(&placeholder, Some("placeholder"));

        container.append(&stack);

        let slot = Rc::new(Self {
            container: container.clone(),
            stack,
            pic,
            stills: stills.clone(),
            file: RefCell::new(None),
            load: DecodeTicket::default(),
            hover: DecodeTicket::default(),
        });
        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&slot);
        motion.connect_enter(move |_, _, _| {
            if let Some(slot) = weak.upgrade() {
                slot.hover(true);
            }
        });
        let weak = Rc::downgrade(&slot);
        motion.connect_leave(move |_| {
            if let Some(slot) = weak.upgrade() {
                slot.hover(false);
            }
        });
        container.add_controller(motion);
        let weak = Rc::downgrade(&slot);
        container.connect_map(move |_| {
            if let Some(slot) = weak.upgrade() {
                slot.request();
            }
        });
        let weak = Rc::downgrade(&slot);
        container.connect_unmap(move |_| {
            if let Some(slot) = weak.upgrade() {
                slot.suspend();
            }
        });
        slot
    }

    /// The URL of the custom emoji the cell is bound to, empty for none.
    fn url(&self) -> String {
        self.stack.widget_name().to_string()
    }

    /// Stops waiting on decodes, e.g. once the cell is off screen.
    fn suspend(&self) {
        self.load.invalidate();
        self.hover.invalidate();
    }

    /// Unbinds the cell from whatever emoji it showed.
    fn reset(&self) {
        self.suspend();
        self.file.take();
        self.container.remove_css_class("hovered");
        self.container.remove_css_class("animating");
        self.stack.set_widget_name("");
        self.pic.set_paintable(None::<&gtk::gdk::Paintable>);
    }

    /// The cell's emoji is downloaded: show it, once the cell is mapped.
    fn set_file(self: &Rc<Self>, path: PathBuf) {
        self.file.replace(Some(path));
        self.request();
    }

    /// Asks for the image of the bound emoji, unless the cell is not mapped
    /// or already shows it.
    fn request(self: &Rc<Self>) {
        let url = self.url();
        let Some(path) = self.file.borrow().clone() else {
            return;
        };
        if !self.container.is_mapped() || url.is_empty() || self.pic.paintable().is_some() {
            return;
        }
        let still = self.stills.borrow().get(&url).cloned();
        if let Some(still) = still {
            self.show(&still);
            return;
        }
        if !crate::animated_image::may_animate(&path) {
            self.request_still(&path);
            return;
        }
        let slot = Rc::downgrade(self);
        let (arrived_url, arrived_path) = (url.clone(), path.clone());
        let requested = decode::request_animation(&path, &self.load, move |decoded| {
            if let Some(slot) = slot.upgrade() {
                slot.animation_loaded(&arrived_url, &arrived_path, decoded);
            }
        });
        if let Requested::Ready(decoded) = requested {
            self.animation_loaded(&url, &path, &decoded);
        }
    }

    fn animation_loaded(self: &Rc<Self>, url: &str, path: &Path, decoded: &Decoded) {
        if self.url() != url {
            return;
        }
        match decoded {
            Decoded::Animation(animation) => {
                let still = gtk::gdk::Texture::for_pixbuf(&animation.iter(None).pixbuf());
                self.stills
                    .borrow_mut()
                    .insert(url.to_string(), still.clone());
                self.show(&still);
                if self.container.has_css_class("hovered") {
                    self.animate(animation);
                }
            }
            // One frame after all.
            Decoded::Still | Decoded::Texture(_) => self.request_still(path),
        }
    }

    fn request_still(self: &Rc<Self>, path: &Path) {
        let url = self.url();
        let slot = Rc::downgrade(self);
        let requested =
            decode::request_texture(path, TextureSpec::Full, &self.load, move |texture| {
                if let Some(slot) = slot.upgrade().filter(|slot| slot.url() == url) {
                    slot.show(texture);
                }
            });
        if let Requested::Ready(texture) = requested {
            self.show(&texture);
        }
    }

    fn show(&self, texture: &gtk::gdk::Texture) {
        self.pic.set_paintable(Some(texture));
        self.stack.set_visible_child_name("pic");
    }

    /// The pointer entered or left the cell. An animated emoji plays while
    /// hovered, loaded again then rather than kept for every cell shown.
    fn hover(self: &Rc<Self>, entered: bool) {
        let url = self.url();
        if !entered {
            self.container.remove_css_class("hovered");
            self.container.remove_css_class("animating");
            self.hover.invalidate();
            let still = self.stills.borrow().get(&url).cloned();
            if let Some(still) = still {
                self.pic.set_paintable(Some(&still));
            }
            return;
        }
        self.container.add_css_class("hovered");
        let path = self.file.borrow().clone();
        let animated = self.stills.borrow().contains_key(&url);
        let (true, Some(path), false) = (animated, path, self.container.has_css_class("animating"))
        else {
            return;
        };
        let slot = Rc::downgrade(self);
        // A known still has nothing to play: `Ready` needs no handling.
        let _ = decode::request_animation(&path, &self.hover, move |decoded| {
            if let (Some(slot), Decoded::Animation(animation)) = (slot.upgrade(), decoded) {
                if slot.url() == url {
                    slot.animate(animation);
                }
            }
        });
    }

    fn animate(&self, animation: &gdk_pixbuf::PixbufAnimation) {
        if self.container.has_css_class("animating") {
            return;
        }
        self.container.add_css_class("animating");
        let iter = animation.iter(None);
        let last_update = Rc::new(Cell::new(std::time::Instant::now()));
        let delay_ms = iter
            .delay_time()
            .map(|d| d.as_millis() as u64)
            .unwrap_or(60)
            .max(60);
        let stack = self.stack.downgrade();
        let container = self.container.downgrade();
        let url = self.url();
        self.pic.add_tick_callback(move |p, _frame_clock| {
            let (Some(stack), Some(container)) = (stack.upgrade(), container.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            if !container.has_css_class("hovered") || !p.is_mapped() || stack.widget_name() != url {
                container.remove_css_class("animating");
                return glib::ControlFlow::Break;
            }
            if last_update.get().elapsed().as_millis() as u64 >= delay_ms {
                last_update.set(std::time::Instant::now());
                iter.advance(std::time::SystemTime::now());
                p.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&iter.pixbuf())));
            }
            glib::ControlFlow::Continue
        });
    }
}

impl EmojiPickerWindow {
    pub(crate) fn new(
        parent: &impl IsA<gtk::Widget>,
        custom_emojis: &HashMap<String, String>,
        on_selected: impl Fn(&str) + 'static,
    ) -> Self {
        let catalog = EmojiCatalog::new(custom_emojis);
        let all_entries = Rc::new(
            catalog
                .entries()
                .into_iter()
                .map(Rc::new)
                .collect::<Vec<_>>(),
        );
        let on_selected = Rc::new(on_selected);

        let window = adw::Window::new();
        window.set_title(Some(&gettext("Emoji")));
        window.set_modal(true);
        window.set_default_size(440, 520);
        window.set_resizable(false);

        if let Some(root) = parent.root() {
            if let Some(toplevel) = root.downcast_ref::<gtk::Window>() {
                window.set_transient_for(Some(toplevel));
            }
        }

        let toolbar_view = adw::ToolbarView::new();

        let header = adw::HeaderBar::new();
        header.set_show_title(true);
        header.set_title_widget(Some(&adw::WindowTitle::new(&gettext("Emoji"), "")));
        toolbar_view.add_top_bar(&header);

        let content_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content_box.set_margin_top(6);
        content_box.set_margin_bottom(10);
        content_box.set_margin_start(12);
        content_box.set_margin_end(12);

        // Row 1: Search Entry
        let search_entry = gtk::SearchEntry::new();
        search_entry.set_placeholder_text(Some(&gettext("Search emoji...")));
        search_entry.update_property(&[gtk::accessible::Property::Label(&gettext("Search emoji"))]);
        content_box.append(&search_entry);

        // Row 2: Category Bar
        let category_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        category_box.set_halign(gtk::Align::Center);
        category_box.set_margin_top(2);
        category_box.set_margin_bottom(4);

        let mut category_buttons = Vec::new();
        let mut first_btn: Option<gtk::ToggleButton> = None;
        let active_category = std::rc::Rc::new(std::cell::RefCell::new("All".to_string()));

        for (cat_name, icon) in CATEGORY_ICONS {
            let btn = gtk::ToggleButton::with_label(icon);
            btn.set_tooltip_text(Some(cat_name));
            btn.add_css_class("flat");
            btn.add_css_class("circular");
            if let Some(ref first) = first_btn {
                btn.set_group(Some(first));
            } else {
                btn.set_active(true);
                first_btn = Some(btn.clone());
            }
            category_box.append(&btn);
            category_buttons.push(((*cat_name).to_string(), btn));
        }

        let category_scroller = gtk::ScrolledWindow::new();
        category_scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
        category_scroller.set_child(Some(&category_box));
        content_box.append(&category_scroller);

        // Row 3: Virtualized GridView
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);

        let (download_tx, mut download_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, PathBuf)>();
        let pending_widgets: Rc<RefCell<HashMap<String, Vec<std::rc::Weak<EmojiSlot>>>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let stills: Stills = Rc::new(RefCell::new(HashMap::new()));
        let slots: Rc<RefCell<HashMap<gtk::Box, Rc<EmojiSlot>>>> =
            Rc::new(RefCell::new(HashMap::new()));

        {
            let pending_widgets = pending_widgets.clone();
            glib::spawn_future_local(async move {
                while let Some((url, path)) = download_rx.recv().await {
                    let waiting = pending_widgets.borrow_mut().remove(&url);
                    for slot in waiting.into_iter().flatten() {
                        if let Some(slot) = slot.upgrade().filter(|slot| slot.url() == url) {
                            slot.set_file(path.clone());
                        }
                    }
                }
            });
        }

        let factory = gtk::SignalListItemFactory::new();
        {
            let stills = stills.clone();
            let slots = slots.clone();
            factory.connect_setup(move |_, list_item| {
                let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
                let slot = EmojiSlot::build(&stills);
                list_item.set_child(Some(&slot.container));
                slots.borrow_mut().insert(slot.container.clone(), slot);
            });
        }

        {
            let slots = slots.clone();
            factory.connect_teardown(move |_, list_item| {
                if let Some(container) = list_item
                    .downcast_ref::<gtk::ListItem>()
                    .and_then(|list_item| list_item.child())
                    .and_downcast::<gtk::Box>()
                {
                    slots.borrow_mut().remove(&container);
                }
            });
        }

        {
            let pending_widgets = pending_widgets.clone();
            let download_tx = download_tx.clone();
            let slots = slots.clone();
            factory.connect_bind(move |_, list_item| {
                let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
                let Some(boxed) = list_item.item().and_downcast::<glib::BoxedAnyObject>() else {
                    return;
                };
                let entry: std::cell::Ref<Rc<EmojiEntry>> = boxed.borrow();
                let entry = entry.clone();

                let Some(container) = list_item.child().and_downcast::<gtk::Box>() else {
                    return;
                };
                let Some(slot) = slots.borrow().get(&container).cloned() else {
                    return;
                };
                slot.reset();
                container.set_tooltip_text(Some(&format!(":{}:", entry.name)));
                container.update_property(&[gtk::accessible::Property::Label(&entry.label)]);

                let stack = &slot.stack;
                match &entry.value {
                    EmojiValue::Unicode(u) => {
                        if let Some(lbl) = stack.child_by_name("text").and_downcast::<gtk::Label>()
                        {
                            lbl.set_text(u);
                        }
                        stack.set_visible_child_name("text");
                    }
                    EmojiValue::CustomImage(url) => {
                        stack.set_widget_name(url);
                        stack.set_visible_child_name("placeholder");
                        if let Some(cached_path) = resolve_custom_emoji_cached_file(url) {
                            slot.set_file(cached_path);
                        } else {
                            pending_widgets
                                .borrow_mut()
                                .entry(url.clone())
                                .or_default()
                                .push(Rc::downgrade(&slot));

                            let tx = download_tx.clone();
                            let url_clone = url.clone();
                            request_custom_emoji_file(url, move |path| {
                                let _ = tx.send((url_clone, path));
                            });
                        }
                    }
                }
            });
        }

        factory.connect_unbind(move |_, list_item| {
            let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
            if let Some(slot) = list_item
                .child()
                .and_downcast::<gtk::Box>()
                .and_then(|container| slots.borrow().get(&container).cloned())
            {
                slot.reset();
            }
        });

        let grid = gtk::GridView::new(Some(selection), Some(factory));
        grid.set_min_columns(8);
        grid.set_max_columns(9);
        grid.set_enable_rubberband(false);
        grid.set_single_click_activate(true);
        grid.set_valign(gtk::Align::Start);
        grid.set_halign(gtk::Align::Center);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_vexpand(true);
        scroller.set_hexpand(true);
        scroller.set_child(Some(&grid));
        content_box.append(&scroller);

        let empty_label = gtk::Label::new(Some(&gettext("No emoji found")));
        empty_label.add_css_class("dim-label");
        empty_label.set_margin_top(32);
        empty_label.set_visible(false);
        content_box.append(&empty_label);

        toolbar_view.set_content(Some(&content_box));
        window.set_content(Some(&toolbar_view));

        let populate = {
            let all_entries = all_entries.clone();
            let store = store.clone();
            let empty_label = empty_label.clone();

            move |query: &str, category: &str| {
                let q = query.trim().to_lowercase();
                let filter_category = if category == "All" {
                    None
                } else {
                    Some(category)
                };

                let mut matching = Vec::new();
                for entry in all_entries.iter() {
                    if let Some(cat) = filter_category {
                        if entry.category != cat {
                            continue;
                        }
                    }
                    if !q.is_empty() {
                        let name_match = entry.name.to_lowercase().contains(&q);
                        let label_match = entry.label.to_lowercase().contains(&q);
                        if !name_match && !label_match {
                            continue;
                        }
                    }
                    matching.push(glib::BoxedAnyObject::new(entry.clone()));
                }

                empty_label.set_visible(matching.is_empty());
                store.splice(0, store.n_items(), &matching);
            }
        };

        // Populate initial
        populate("", "All");

        // Search change
        {
            let populate = populate.clone();
            let active_category = active_category.clone();
            search_entry.connect_search_changed(move |entry| {
                populate(entry.text().as_str(), &active_category.borrow());
            });
        }

        // Category tabs
        for (cat_name, btn) in category_buttons {
            let populate = populate.clone();
            let active_category = active_category.clone();
            let search_entry = search_entry.clone();
            btn.connect_toggled(move |b| {
                if b.is_active() {
                    active_category.replace(cat_name.clone());
                    populate(search_entry.text().as_str(), &cat_name);
                }
            });
        }

        // Item click
        {
            let store = store.clone();
            let on_selected = on_selected.clone();
            let win_weak = window.downgrade();
            grid.connect_activate(move |_, pos| {
                if let Some(boxed) = store.item(pos).and_downcast::<glib::BoxedAnyObject>() {
                    let entry: std::cell::Ref<Rc<EmojiEntry>> = boxed.borrow();
                    on_selected(&entry.name);
                    if let Some(win) = win_weak.upgrade() {
                        win.close();
                    }
                }
            });
        }

        // Search Enter key
        {
            let store = store.clone();
            let on_selected = on_selected.clone();
            let win_weak = window.downgrade();
            search_entry.connect_activate(move |_| {
                if let Some(boxed) = store.item(0).and_downcast::<glib::BoxedAnyObject>() {
                    let entry: std::cell::Ref<Rc<EmojiEntry>> = boxed.borrow();
                    on_selected(&entry.name);
                    if let Some(win) = win_weak.upgrade() {
                        win.close();
                    }
                }
            });
        }

        // Escape key to close window cleanly
        let key_controller = gtk::EventControllerKey::new();
        {
            let win_weak = window.downgrade();
            key_controller.connect_key_pressed(move |_, key, _, _| {
                if key == Key::Escape {
                    if let Some(win) = win_weak.upgrade() {
                        win.close();
                        return glib::Propagation::Stop;
                    }
                }
                glib::Propagation::Proceed
            });
        }
        window.add_controller(key_controller);

        Self { window }
    }

    pub(crate) fn present(&self) {
        self.window.present();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_maps_to_expected_extension() {
        assert_eq!(extension_for_content_type(Some("image/gif")), "gif");
        assert_eq!(
            extension_for_content_type(Some("image/webp; charset=binary")),
            "webp"
        );
        assert_eq!(extension_for_content_type(Some("image/jpeg")), "jpg");
        assert_eq!(extension_for_content_type(Some("image/jpg")), "jpg");
        assert_eq!(extension_for_content_type(Some("image/png")), "png");
        assert_eq!(extension_for_content_type(None), "png");
    }

    #[test]
    fn custom_emoji_url_validation_blocks_ssrf_and_local_paths() {
        assert!(is_valid_custom_emoji_url(
            "https://emoji.slack-edge.com/T1/parrot/abc.gif"
        ));
        assert!(is_valid_custom_emoji_url(
            "https://cdn.example.com/emoji/custom.png"
        ));
        assert!(!is_valid_custom_emoji_url(
            "http://emoji.slack-edge.com/T1/parrot/abc.gif"
        ));
        assert!(!is_valid_custom_emoji_url(
            "http://127.0.0.1:8080/emoji.png"
        ));
        assert!(!is_valid_custom_emoji_url(
            "https://169.254.169.254/latest/meta-data/"
        ));
        assert!(!is_valid_custom_emoji_url("https://localhost/emoji.png"));
        assert!(!is_valid_custom_emoji_url(
            "https://token@emoji.slack-edge.com/T1/parrot/abc.gif"
        ));
        assert!(!is_valid_custom_emoji_url("/etc/passwd"));
    }

    #[test]
    fn resolve_custom_emoji_does_not_negative_cache_missing_files() {
        let url = format!(
            "https://emoji.slack-edge.com/T_TEST/non_negative_{}/emoji.gif",
            std::process::id()
        );
        assert!(resolve_custom_emoji_cached_file(&url).is_none());
        assert!(!resolved_cache_map()
            .lock()
            .unwrap()
            .contains_key(url.as_str()));

        let dir = custom_emoji_cache_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let hash = custom_emoji_url_hash(&url);
        let file_path = dir.join(format!("{hash}.gif"));
        std::fs::write(&file_path, b"GIF89a").unwrap();

        let resolved = resolve_custom_emoji_cached_file(&url);
        assert_eq!(resolved.as_deref(), Some(file_path.as_path()));

        std::fs::remove_file(&file_path).unwrap();
        assert!(resolve_custom_emoji_cached_file(&url).is_none());
    }

    #[test]
    fn queue_skips_url_already_marked_failed() {
        let url = "https://emoji.slack-edge.com/T_TEST/bad/emoji.png".to_string();
        let loader = CustomEmojiLoader {
            state: Arc::new(Mutex::new(LoaderState {
                queue: VecDeque::new(),
                in_flight: HashMap::new(),
                failed: HashSet::from([url.clone()]),
                failed_order: VecDeque::from([url.clone()]),
            })),
            notify: Arc::new(tokio::sync::Notify::new()),
        };

        loader.queue(url.clone(), false, None);

        let state = loader.state.lock().unwrap();
        assert!(
            state.queue.is_empty(),
            "a URL already in the failed set must not be re-queued for download"
        );
        assert!(!state.in_flight.contains_key(&url));
    }

    #[test]
    fn mark_failed_evicts_oldest_entry_past_the_cap() {
        let mut state = LoaderState {
            queue: VecDeque::new(),
            in_flight: HashMap::new(),
            failed: HashSet::new(),
            failed_order: VecDeque::new(),
        };

        for i in 0..MAX_FAILED_EMOJI_URLS + 10 {
            state.mark_failed(format!("https://example.com/emoji-{i}.png"));
        }

        assert_eq!(state.failed.len(), MAX_FAILED_EMOJI_URLS);
        assert_eq!(state.failed_order.len(), MAX_FAILED_EMOJI_URLS);
        assert!(!state.failed.contains("https://example.com/emoji-0.png"));
        assert!(state.failed.contains(&format!(
            "https://example.com/emoji-{}.png",
            MAX_FAILED_EMOJI_URLS + 9
        )));
    }

    /// Tests that create GTK widgets. They run only when
    /// `CONDUIT_GRAPHICAL_TESTS` is set (see `run_gtk_test`).
    mod graphical {
        use super::*;
        use crate::animated_image::tests::ANIMATED_GIF;
        use crate::timeline::tests::{
            descendants, run_gtk_test, save_test_image, wait_for, ManualDecodes, TempTestDir,
        };

        const QUERY: &str = "qqx";
        const STILL_SIDES: [i32; 4] = [20, 24, 28, 32];

        struct Picker {
            _dir: TempTestDir,
            parent: gtk::Window,
            picker: EmojiPickerWindow,
        }

        impl Picker {
            fn cells(&self) -> Vec<(String, gtk::Box, gtk::Stack)> {
                descendants(self.picker.window.upcast_ref())
                    .into_iter()
                    .filter_map(|widget| widget.downcast::<gtk::Stack>().ok())
                    .filter(|stack| stack.widget_name().starts_with("https://"))
                    .filter_map(|stack| {
                        let container = stack.parent().and_downcast::<gtk::Box>()?;
                        Some((stack.widget_name().to_string(), container, stack))
                    })
                    .collect()
            }

            fn urls(&self) -> Vec<String> {
                self.cells().into_iter().map(|(url, ..)| url).collect()
            }

            fn search(&self, query: &str) {
                let entry = descendants(self.picker.window.upcast_ref())
                    .into_iter()
                    .find_map(|widget| widget.downcast::<gtk::SearchEntry>().ok())
                    .expect("search entry");
                entry.set_text(query);
            }

            /// Width of the still a cell shows, once it has one.
            fn shown_width(stack: &gtk::Stack) -> Option<i32> {
                let pic = stack.child_by_name("pic").and_downcast::<gtk::Picture>()?;
                let texture = pic.paintable()?;
                Some(texture.intrinsic_width())
            }
        }

        impl Drop for Picker {
            fn drop(&mut self) {
                self.picker.window.destroy();
                self.parent.destroy();
            }
        }

        /// Four static emoji of different sizes, two animated ones and an
        /// alias of the first of those, all cached on disk already.
        fn open_picker() -> Picker {
            let dir = TempTestDir::new("emoji-picker");
            let mut custom = HashMap::new();
            let mut register = |name: &str, extension: &str, write: &dyn Fn(&Path)| {
                let path = dir.path().join(format!("{name}.{extension}"));
                write(&path);
                let url = format!("https://emoji.example.com/{name}.{extension}");
                invalidate_or_set_custom_emoji_cached_file(&url, path);
                custom.insert(name.to_string(), url);
            };
            for (index, side) in STILL_SIDES.iter().enumerate() {
                register(&format!("{QUERY}-still-{index}"), "png", &|path| {
                    save_test_image(path, "png", *side, *side);
                });
            }
            for index in 0..2 {
                register(&format!("{QUERY}-anim-{index}"), "gif", &|path| {
                    std::fs::write(path, ANIMATED_GIF).expect("write gif");
                });
            }
            custom.insert(format!("{QUERY}-alias"), format!("alias:{QUERY}-anim-0"));
            let parent = gtk::Window::new();
            let picker = EmojiPickerWindow::new(&parent, &custom, |_| {});
            picker.present();
            let picker = Picker {
                _dir: dir,
                parent,
                picker,
            };
            picker.search(QUERY);
            wait_for(
                5000,
                "the picker to show its seven emoji",
                || format!("{:?}", picker.urls()),
                || picker.cells().len() == 7,
            );
            picker
        }

        #[test]
        fn perf_guard_emoji_picker_decodes_off_the_gtk_thread() {
            run_gtk_test(|| {
                let _ = adw::init();
                let decodes = ManualDecodes::install();
                let before = crate::debug::IMAGE_DECODES.get();
                let picker = open_picker();
                wait_for(
                    5000,
                    "every cell mapped and three decodes running",
                    || format!("{:?} running {:?}", picker.urls(), decodes.0.running()),
                    || {
                        picker
                            .cells()
                            .iter()
                            .all(|(_, container, _)| container.is_mapped())
                            && decodes.0.running().len() == decode::MAX_IN_FLIGHT
                    },
                );
                assert_eq!(
                    crate::debug::IMAGE_DECODES.get() - before,
                    0,
                    "the picker must leave decodes to the workers"
                );
                let cells = picker.cells();
                assert_eq!(cells.len(), 7, "four stills, two animations, one alias");
                assert_eq!(decodes.0.running().len(), decode::MAX_IN_FLIGHT);
                for (_, _, stack) in &cells {
                    assert_eq!(stack.visible_child_name().as_deref(), Some("placeholder"));
                }

                let finished = decodes.finish_all();
                wait_for(
                    5000,
                    "every cell to show its image",
                    || format!("{:?}", picker.urls()),
                    || {
                        cells.iter().all(|(_, _, stack)| {
                            stack.visible_child_name().as_deref() == Some("pic")
                        })
                    },
                );
                let unique: HashSet<_> = finished.iter().collect();
                assert_eq!(finished.len(), 6, "one decode per file: {finished:?}");
                assert_eq!(unique.len(), finished.len(), "no file decoded twice");
                for (url, container, stack) in &cells {
                    assert_eq!(stack.visible_child_name().as_deref(), Some("pic"), "{url}");
                    assert!(!container.has_css_class("animating"));
                    let width = Picker::shown_width(stack).expect("image shown");
                    if let Some(index) = url
                        .split("still-")
                        .nth(1)
                        .and_then(|rest| rest.split('.').next())
                        .and_then(|index| index.parse::<usize>().ok())
                    {
                        assert_eq!(width, STILL_SIDES[index], "{url} shows its own image");
                    }
                }

                // Hovering an animated emoji loads it again, off the thread,
                // and plays it; leaving restores the still frame.
                let animated: Vec<_> = cells
                    .iter()
                    .filter(|(url, ..)| url.ends_with("anim-0.gif"))
                    .collect();
                assert_eq!(animated.len(), 2, "the alias shows the same file");
                let hover = |container: &gtk::Box, signal: &str| {
                    let motion = container
                        .observe_controllers()
                        .into_iter()
                        .filter_map(|controller| {
                            controller
                                .ok()?
                                .downcast::<gtk::EventControllerMotion>()
                                .ok()
                        })
                        .next()
                        .expect("motion controller");
                    match signal {
                        "enter" => motion.emit_by_name::<()>("enter", &[&0.0_f64, &0.0_f64]),
                        _ => motion.emit_by_name::<()>("leave", &[]),
                    }
                };
                let before = crate::debug::IMAGE_DECODES.get();
                hover(&animated[0].1, "enter");
                hover(&animated[1].1, "enter");
                assert_eq!(
                    decodes.0.running().len(),
                    1,
                    "both cells share one animation load"
                );
                assert_eq!(crate::debug::IMAGE_DECODES.get() - before, 0);
                decodes.finish_all();
                assert!(animated[0].1.has_css_class("animating"));
                assert!(animated[1].1.has_css_class("animating"));
                hover(&animated[0].1, "leave");
                assert!(!animated[0].1.has_css_class("animating"));
            });
        }

        #[test]
        fn recycled_cell_ignores_the_decode_of_the_emoji_it_showed_before() {
            run_gtk_test(|| {
                let _ = adw::init();
                let decodes = ManualDecodes::install();
                let dir = TempTestDir::new("emoji-recycle");
                let slot = EmojiSlot::build(&Stills::default());
                let window = gtk::Window::new();
                window.set_child(Some(&slot.container));
                window.present();
                wait_for(
                    5000,
                    "the cell to be mapped",
                    || "unmapped".to_string(),
                    || slot.container.is_mapped(),
                );
                // Bound to one emoji, then to another before the first
                // one's decode has landed, as a scrolled grid does.
                for (index, side) in [20, 24].into_iter().enumerate() {
                    let path = dir.path().join(format!("{index}.png"));
                    save_test_image(&path, "png", side, side);
                    slot.reset();
                    slot.stack
                        .set_widget_name(&format!("https://emoji.example.com/{index}.png"));
                    slot.set_file(path);
                }
                let running = decodes.0.running();
                assert_eq!(running.len(), 2, "{running:?}");
                // The second one's own result lands first, the first one's
                // late.
                for key in [&running[1], &running[0]] {
                    let index = decodes
                        .0
                        .running()
                        .iter()
                        .position(|running| running == key)
                        .expect("still running");
                    decodes.0.complete(index, decode::decode_now_for_tests(key));
                }
                assert_eq!(
                    Picker::shown_width(&slot.stack),
                    Some(24),
                    "a late result of another emoji must not replace it"
                );
                window.destroy();
            });
        }

        #[test]
        fn unmapped_cells_request_no_decodes() {
            run_gtk_test(|| {
                let _ = adw::init();
                let decodes = ManualDecodes::install();
                let picker = open_picker();
                wait_for(
                    5000,
                    "every cell mapped and three decodes running",
                    || format!("{:?} running {:?}", picker.urls(), decodes.0.running()),
                    || {
                        picker
                            .cells()
                            .iter()
                            .all(|(_, container, _)| container.is_mapped())
                            && decodes.0.running().len() == decode::MAX_IN_FLIGHT
                    },
                );
                picker.picker.window.set_visible(false);
                wait_for(
                    5000,
                    "every cell to be unmapped",
                    || format!("{:?}", picker.urls()),
                    || {
                        picker
                            .cells()
                            .iter()
                            .all(|(_, container, _)| !container.is_mapped())
                    },
                );
                // Whatever was queued for cells that are no longer shown is
                // skipped: finishing the running decodes starts nothing new.
                let running = decodes.0.running();
                let finished = decodes.finish_all();
                assert_eq!(
                    finished.len(),
                    running.len(),
                    "a queued decode started: running {running:?}, finished {finished:?}, cells {:?}",
                    picker
                        .cells()
                        .iter()
                        .map(|(url, container, _)| (url.clone(), container.is_mapped()))
                        .collect::<Vec<_>>()
                );
            });
        }
    }
}
