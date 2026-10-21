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
        let pending_widgets: std::rc::Rc<
            std::cell::RefCell<HashMap<String, Vec<glib::WeakRef<gtk::Stack>>>>,
        > = std::rc::Rc::new(std::cell::RefCell::new(HashMap::new()));

        let animated_emojis: Rc<RefCell<HashMap<String, (PathBuf, gtk::gdk::Texture)>>> =
            Rc::new(RefCell::new(HashMap::new()));

        fn start_slot_animation(
            container: &gtk::Box,
            stack: &gtk::Stack,
            pic: &gtk::Picture,
            path: &Path,
            expected_url: &str,
        ) {
            if container.has_css_class("animating")
                || !crate::animated_image::within_decode_budget(path)
            {
                return;
            }
            let Ok(anim) = gdk_pixbuf::PixbufAnimation::from_file(path) else {
                return;
            };
            if anim.is_static_image() {
                return;
            }
            container.add_css_class("animating");

            let iter = anim.iter(None);
            let last_update = Rc::new(Cell::new(std::time::Instant::now()));
            let delay_ms = iter
                .delay_time()
                .map(|d| d.as_millis() as u64)
                .unwrap_or(60)
                .max(60);

            let stack_weak = stack.downgrade();
            let container_weak = container.downgrade();
            let url_owned = expected_url.to_string();

            pic.add_tick_callback(move |p, _frame_clock| {
                let Some(stack) = stack_weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let Some(container) = container_weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                if !container.has_css_class("hovered")
                    || !p.is_mapped()
                    || stack.widget_name() != url_owned
                {
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

        fn apply_custom_emoji_image(
            stack: &gtk::Stack,
            path: &Path,
            expected_url: &str,
            animated_emojis: &Rc<RefCell<HashMap<String, (PathBuf, gtk::gdk::Texture)>>>,
        ) {
            let Some(pic) = stack.child_by_name("pic").and_downcast::<gtk::Picture>() else {
                return;
            };
            if !crate::animated_image::within_decode_budget(path) {
                return;
            }
            if let Ok(anim) = gdk_pixbuf::PixbufAnimation::from_file(path) {
                if !anim.is_static_image() {
                    let iter = anim.iter(None);
                    let first_pixbuf = iter.pixbuf();
                    let first_texture = gtk::gdk::Texture::for_pixbuf(&first_pixbuf);
                    pic.set_paintable(Some(&first_texture));
                    stack.set_visible_child_name("pic");

                    animated_emojis.borrow_mut().insert(
                        expected_url.to_string(),
                        (path.to_path_buf(), first_texture),
                    );

                    if let Some(container) = stack.parent().and_downcast::<gtk::Box>() {
                        if container.has_css_class("hovered") {
                            start_slot_animation(&container, stack, &pic, path, expected_url);
                        }
                    }
                    return;
                }
            }

            pic.set_filename(Some(path));
            stack.set_visible_child_name("pic");
        }

        {
            let pending_widgets = pending_widgets.clone();
            let animated_emojis = animated_emojis.clone();
            glib::spawn_future_local(async move {
                while let Some((url, path)) = download_rx.recv().await {
                    if let Some(stacks) = pending_widgets.borrow_mut().remove(&url) {
                        for stack_weak in stacks {
                            if let Some(stack) = stack_weak.upgrade() {
                                if stack.widget_name() == url {
                                    apply_custom_emoji_image(&stack, &path, &url, &animated_emojis);
                                }
                            }
                        }
                    }
                }
            });
        }

        let factory = gtk::SignalListItemFactory::new();
        {
            let animated_emojis = animated_emojis.clone();
            factory.connect_setup(move |_, list_item| {
                let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();

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

                let motion = gtk::EventControllerMotion::new();
                {
                    let stack_weak = stack.downgrade();
                    let pic_weak = pic.downgrade();
                    let container_weak = container.downgrade();
                    let animated_emojis = animated_emojis.clone();
                    motion.connect_enter(move |_, _, _| {
                        let Some(container) = container_weak.upgrade() else {
                            return;
                        };
                        let Some(stack) = stack_weak.upgrade() else {
                            return;
                        };
                        let Some(pic) = pic_weak.upgrade() else {
                            return;
                        };
                        container.add_css_class("hovered");
                        let url = stack.widget_name().to_string();
                        if url.is_empty() {
                            return;
                        }
                        if let Some((path, _)) = animated_emojis.borrow().get(&url) {
                            start_slot_animation(&container, &stack, &pic, path, &url);
                        }
                    });
                }
                {
                    let stack_weak = stack.downgrade();
                    let pic_weak = pic.downgrade();
                    let container_weak = container.downgrade();
                    let animated_emojis = animated_emojis.clone();
                    motion.connect_leave(move |_| {
                        let Some(container) = container_weak.upgrade() else {
                            return;
                        };
                        let Some(stack) = stack_weak.upgrade() else {
                            return;
                        };
                        let Some(pic) = pic_weak.upgrade() else {
                            return;
                        };
                        container.remove_css_class("hovered");
                        container.remove_css_class("animating");
                        let url = stack.widget_name().to_string();
                        if let Some((_, first_frame)) = animated_emojis.borrow().get(&url) {
                            pic.set_paintable(Some(first_frame));
                        }
                    });
                }
                container.add_controller(motion);

                list_item.set_child(Some(&container));
            });
        }

        {
            let pending_widgets = pending_widgets.clone();
            let download_tx = download_tx.clone();
            let animated_emojis = animated_emojis.clone();
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
                container.remove_css_class("hovered");
                container.remove_css_class("animating");
                container.set_tooltip_text(Some(&format!(":{}:", entry.name)));
                container.update_property(&[gtk::accessible::Property::Label(&entry.label)]);

                let Some(stack) = container.first_child().and_downcast::<gtk::Stack>() else {
                    return;
                };

                match &entry.value {
                    EmojiValue::Unicode(u) => {
                        stack.set_widget_name("");
                        if let Some(lbl) = stack.child_by_name("text").and_downcast::<gtk::Label>()
                        {
                            lbl.set_text(u);
                        }
                        stack.set_visible_child_name("text");
                    }
                    EmojiValue::CustomImage(url) => {
                        stack.set_widget_name(url);
                        if let Some(cached_path) = resolve_custom_emoji_cached_file(url) {
                            apply_custom_emoji_image(&stack, &cached_path, url, &animated_emojis);
                        } else {
                            stack.set_visible_child_name("placeholder");
                            pending_widgets
                                .borrow_mut()
                                .entry(url.clone())
                                .or_default()
                                .push(stack.downgrade());

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
            if let Some(container) = list_item.child().and_downcast::<gtk::Box>() {
                container.remove_css_class("hovered");
                container.remove_css_class("animating");
                if let Some(stack) = container.first_child().and_downcast::<gtk::Stack>() {
                    stack.set_widget_name("");
                    if let Some(pic) = stack.child_by_name("pic").and_downcast::<gtk::Picture>() {
                        pic.set_paintable(Option::<&gtk::gdk::Paintable>::None);
                    }
                }
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
}
