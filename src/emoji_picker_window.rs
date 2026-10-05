/* emoji_picker_window.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use adw::prelude::*;
use gettextrs::gettext;
use gtk::gdk::Key;
use gtk::glib;
use sha2::{Digest, Sha256};

use crate::emoji::{EmojiCatalog, EmojiValue};

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

fn resolve_custom_emoji_cached_file(url: &str) -> Option<PathBuf> {
    if let Ok(path) = PathBuf::from(url).canonicalize() {
        if path.exists() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            return Some(path);
        }
    }
    let cache_dir = crate::config::image_asset_cache_dir();
    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(url.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    for ext in ["png", "gif", "jpg", "webp"] {
        let p = cache_dir.join(format!("{hash}.{ext}"));
        if p.exists() && p.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            return Some(p);
        }
    }
    None
}

type DownloadCallback = Box<dyn FnOnce(PathBuf) + Send + 'static>;

struct DownloadRequest {
    url: String,
    callback: Option<DownloadCallback>,
}

struct LoaderState {
    queue: VecDeque<DownloadRequest>,
    in_flight: HashMap<String, Vec<DownloadCallback>>,
    failed: HashSet<String>,
}

struct CustomEmojiLoader {
    state: Arc<Mutex<LoaderState>>,
    condvar: Arc<std::sync::Condvar>,
}

impl CustomEmojiLoader {
    fn new() -> Self {
        let state = Arc::new(Mutex::new(LoaderState {
            queue: VecDeque::new(),
            in_flight: HashMap::new(),
            failed: HashSet::new(),
        }));
        let condvar = Arc::new(std::sync::Condvar::new());

        let state_clone = Arc::clone(&state);
        let condvar_clone = Arc::clone(&condvar);

        std::thread::Builder::new()
            .name("conduit-emoji-loader".to_string())
            .spawn(move || {
                // Downloads are network-bound, so a single-thread executor is
                // enough concurrency (bounded further by the semaphore below)
                // without paying for a dedicated multi-thread runtime here.
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => return,
                };

                let client = match crate::http_client::builder().build() {
                    Ok(c) => c,
                    Err(_) => return,
                };

                rt.block_on(async move {
                    let sem = Arc::new(tokio::sync::Semaphore::new(6));
                    loop {
                        let next_req = {
                            let mut s = state_clone.lock().unwrap();
                            while s.queue.is_empty() {
                                s = condvar_clone.wait(s).unwrap();
                            }
                            s.queue.pop_front()
                        };

                        let Some(req) = next_req else { continue };

                        if let Some(path) = resolve_custom_emoji_cached_file(&req.url) {
                            if let Some(cb) = req.callback {
                                cb(path);
                            }
                            continue;
                        }

                        {
                            let mut s = state_clone.lock().unwrap();
                            // A prior failure doesn't permanently block this URL: drop it from
                            // `failed` and retry, so a caller's callback always eventually fires
                            // instead of being silently dropped forever.
                            s.failed.remove(&req.url);
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
                        }

                        let client = client.clone();
                        let sem = Arc::clone(&sem);
                        let state = Arc::clone(&state_clone);
                        let url = req.url;

                        tokio::spawn(async move {
                            let _permit = sem.acquire().await;
                            let res = download_custom_emoji_file(&client, &url).await;

                            let (callbacks, is_ok) = {
                                let mut s = state.lock().unwrap();
                                let cbs = s.in_flight.remove(&url).unwrap_or_default();
                                if res.is_none() {
                                    s.failed.insert(url.clone());
                                }
                                (cbs, res)
                            };

                            if let Some(path) = is_ok {
                                for cb in callbacks {
                                    cb(path.clone());
                                }
                            }
                        });
                    }
                });
            })
            .expect("spawn emoji loader thread");

        Self { state, condvar }
    }

    fn queue(&self, url: String, high_priority: bool, callback: Option<DownloadCallback>) {
        if let Some(path) = resolve_custom_emoji_cached_file(&url) {
            if let Some(cb) = callback {
                cb(path);
            }
            return;
        }

        let mut s = self.state.lock().unwrap();
        // A previously-failed URL is retried rather than dropped, so every
        // caller that queues a callback for it eventually gets a resolution.
        s.failed.remove(&url);

        if let Some(cbs) = s.in_flight.get_mut(&url) {
            if let Some(cb) = callback {
                cbs.push(cb);
            }
            return;
        }

        if let Some(existing) = s.queue.iter_mut().find(|r| r.url == url) {
            if let Some(cb) = callback {
                match existing.callback.take() {
                    Some(prev) => {
                        existing.callback = Some(Box::new(move |p| {
                            prev(p.clone());
                            cb(p);
                        }));
                    }
                    None => {
                        existing.callback = Some(cb);
                    }
                }
            }
            return;
        }

        let req = DownloadRequest { url, callback };
        if high_priority {
            s.queue.push_front(req);
        } else {
            s.queue.push_back(req);
        }
        self.condvar.notify_one();
    }
}

async fn download_custom_emoji_file(client: &reqwest::Client, url: &str) -> Option<PathBuf> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return None;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return None;
    };
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return None;
    }

    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let ext = match resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        Some(ct) if ct.contains("gif") => "gif",
        Some(ct) if ct.contains("webp") => "webp",
        Some(ct) if ct.contains("jpeg") || ct.contains("jpg") => "jpg",
        _ => "png",
    };

    let bytes = resp.bytes().await.ok()?;
    if bytes.is_empty() || bytes.len() > 8 * 1024 * 1024 {
        return None;
    }

    let cache_dir = crate::config::image_asset_cache_dir();
    let _ = std::fs::create_dir_all(&cache_dir);

    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(url.as_bytes());
        format!("{:x}", hasher.finalize())
    };

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

    Some(final_path)
}

static LOADER: OnceLock<CustomEmojiLoader> = OnceLock::new();

fn emoji_loader() -> &'static CustomEmojiLoader {
    LOADER.get_or_init(CustomEmojiLoader::new)
}

impl EmojiPickerWindow {
    pub(crate) fn new(
        parent: &impl IsA<gtk::Widget>,
        custom_emojis: &HashMap<String, String>,
        on_selected: impl Fn(&str) + 'static,
    ) -> Self {
        let catalog = EmojiCatalog::new(custom_emojis);
        let all_entries = Rc::new(catalog.entries());
        let on_selected = Rc::new(on_selected);

        let (download_tx, mut download_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, PathBuf)>();
        let pending_views: Rc<
            RefCell<HashMap<String, Vec<(glib::WeakRef<gtk::Picture>, glib::WeakRef<gtk::Label>)>>>,
        > = Rc::new(RefCell::new(HashMap::new()));

        {
            let pending_views = pending_views.clone();
            glib::spawn_future_local(async move {
                while let Some((url, path)) = download_rx.recv().await {
                    if let Some(views) = pending_views.borrow_mut().remove(&url) {
                        for (pic_weak, placeholder_weak) in views {
                            if let Some(pic) = pic_weak.upgrade() {
                                pic.set_filename(Some(&path));
                                if let Some(ph) = placeholder_weak.upgrade() {
                                    ph.set_visible(false);
                                }
                            }
                        }
                    }
                }
            });
        }

        let loader = emoji_loader();
        for url in custom_emojis.values() {
            if url.starts_with("http://") || url.starts_with("https://") {
                if resolve_custom_emoji_cached_file(url).is_none() {
                    loader.queue(url.clone(), false, None);
                }
            }
        }

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
        let active_category = Rc::new(RefCell::new("All".to_string()));

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

        // Row 3: Grid Scroller
        let grid = gtk::FlowBox::new();
        grid.set_activate_on_single_click(true);
        grid.set_column_spacing(6);
        grid.set_row_spacing(6);
        grid.set_homogeneous(true);
        grid.set_min_children_per_line(8);
        grid.set_max_children_per_line(9);
        grid.set_selection_mode(gtk::SelectionMode::None);
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

        let current_names = Rc::new(RefCell::new(Vec::<String>::new()));

        let populate = {
            let all_entries = all_entries.clone();
            let grid = grid.clone();
            let empty_label = empty_label.clone();
            let current_names = current_names.clone();
            let pending_views = pending_views.clone();
            let download_tx = download_tx.clone();

            move |query: &str, category: &str| {
                while let Some(child) = grid.first_child() {
                    grid.remove(&child);
                }
                current_names.borrow_mut().clear();
                pending_views.borrow_mut().clear();

                let q = query.trim().to_lowercase();
                let filter_category = if category == "All" { None } else { Some(category) };

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
                    matching.push(entry);
                }

                empty_label.set_visible(matching.is_empty());

                for entry in matching {
                    let child = gtk::FlowBoxChild::new();
                    child.set_tooltip_text(Some(&format!(":{}:", entry.name)));
                    child.update_property(&[gtk::accessible::Property::Label(&entry.label)]);

                    let widget: gtk::Widget = match &entry.value {
                        EmojiValue::Unicode(u) => {
                            let lbl = gtk::Label::new(Some(u));
                            lbl.add_css_class("title-3");
                            lbl.set_size_request(36, 36);
                            lbl.upcast()
                        }
                        EmojiValue::CustomImage(url) => {
                            if let Some(cached_path) = resolve_custom_emoji_cached_file(url) {
                                let pic = gtk::Picture::for_filename(&cached_path);
                                pic.set_size_request(32, 32);
                                pic.set_can_shrink(true);
                                pic.set_content_fit(gtk::ContentFit::Cover);
                                pic.upcast()
                            } else {
                                let overlay = gtk::Overlay::new();
                                overlay.set_size_request(36, 36);

                                let placeholder = gtk::Label::new(Some("💬"));
                                placeholder.add_css_class("dim-label");
                                placeholder.set_size_request(36, 36);
                                overlay.set_child(Some(&placeholder));

                                let pic = gtk::Picture::new();
                                pic.set_size_request(32, 32);
                                pic.set_can_shrink(true);
                                pic.set_content_fit(gtk::ContentFit::Cover);
                                overlay.add_overlay(&pic);

                                let pic_weak = pic.downgrade();
                                let placeholder_weak = placeholder.downgrade();
                                pending_views
                                    .borrow_mut()
                                    .entry(url.clone())
                                    .or_default()
                                    .push((pic_weak, placeholder_weak));

                                let tx = download_tx.clone();
                                let url_clone = url.clone();
                                emoji_loader().queue(
                                    url.clone(),
                                    true,
                                    Some(Box::new(move |path| {
                                        let _ = tx.send((url_clone, path));
                                    })),
                                );

                                overlay.upcast()
                            }
                        }
                    };
                    child.set_child(Some(&widget));
                    grid.insert(&child, -1);
                    current_names.borrow_mut().push(entry.name.clone());
                }
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
            let current_names = current_names.clone();
            let on_selected = on_selected.clone();
            let win_weak = window.downgrade();
            grid.connect_child_activated(move |_, child| {
                let idx = child.index() as usize;
                if let Some(name) = current_names.borrow().get(idx).cloned() {
                    on_selected(&name);
                    if let Some(win) = win_weak.upgrade() {
                        win.close();
                    }
                }
            });
        }

        // Search Enter key
        {
            let current_names = current_names.clone();
            let on_selected = on_selected.clone();
            let win_weak = window.downgrade();
            search_entry.connect_activate(move |_| {
                if let Some(name) = current_names.borrow().first().cloned() {
                    on_selected(&name);
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
