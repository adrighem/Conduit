use super::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{glib, Box, Button, Image, Label, Orientation, Picture, TextView, ToggleButton, Widget};

use crate::message_markup::MessageRenderContext;
use crate::models::{
    SlackAttachment, SlackAttachmentField, SlackFile, SlackMessage, SlackReaction,
};
use crate::timeline::cache::decode;

#[test]
fn fit_within_keeps_aspect_ratio() {
    assert_eq!(fit_within(128, 128, 32), (32, 32));
    assert_eq!(fit_within(128, 64, 32), (32, 16));
    assert_eq!(fit_within(48, 96, 32), (16, 32));
    assert_eq!(fit_within(16, 16, 32), (32, 32));
    assert_eq!(fit_within(1000, 1, 32), (32, 1));
}

#[test]
fn dispatch_allows_handler_to_replace_itself() {
    let on_action: ActionSlot = Rc::new(RefCell::new(None));
    let replaced = Rc::new(Cell::new(false));
    let slot = on_action.clone();
    let flag = replaced.clone();
    *on_action.borrow_mut() = Some(Rc::new(move |_| {
        // Mirrors a re-render calling `set_on_action` from inside a handler.
        *slot.borrow_mut() = Some(Rc::new(|_| {}));
        flag.set(true);
    }));

    dispatch_timeline_action(&on_action, TimelineAction::OpenThread("1.0".to_string()));

    assert!(replaced.get());
}

#[test]
fn strip_anchor_tags_returns_link_ranges_of_visible_text() {
    let (clean, links) =
        strip_anchor_tags("a &amp; <b>b</b> <a href=\"https://x.io/?a=1&amp;b=2\">link</a> z");
    assert_eq!(clean, "a &amp; <b>b</b> link z");
    assert_eq!(
        links,
        vec![MarkupLink {
            start: 6,
            end: 10,
            href: "https://x.io/?a=1&b=2".into()
        }]
    );
}

pub(crate) fn test_context() -> MessageRenderContext {
    MessageRenderContext {
        user_names: Arc::new(HashMap::from([("U123".to_string(), "Alice".to_string())])),
        user_full_names: Arc::default(),
        user_avatar_urls: Arc::default(),
        bot_user_ids: Arc::default(),
        conversation_titles: Arc::default(),
        private_conversation_ids: Arc::default(),
        user_statuses: Arc::default(),
        user_group_names: Arc::default(),
        user_group_members: Arc::default(),
        current_user_id: None,
        thread_ts: None,
        timeline_scroll: crate::message_markup::TimelineScrollBehavior::default(),
        image_assets: HashMap::default(),
        failed_image_urls: std::collections::HashSet::default(),
        recent_reactions: Vec::new(),
        custom_emojis: Arc::default(),
        last_read: None,
        unread_separator_ts: None,
    }
}

type GtkTestJob = std::boxed::Box<dyn FnOnce() + Send>;
type GtkTestRunner = Result<std::sync::mpsc::Sender<GtkTestJob>, String>;
static GTK_TEST_RUNNER: std::sync::OnceLock<GtkTestRunner> = std::sync::OnceLock::new();

/// Whether graphical tests (anything that creates GTK widgets or needs a
/// display) should run. Plain `cargo test` leaves `CONDUIT_GRAPHICAL_TESTS`
/// unset and skips them without touching GTK; the pre-push hook and CI set it
/// to `1` under Xvfb.
pub(crate) fn graphical_tests_enabled() -> bool {
    std::env::var_os("CONDUIT_GRAPHICAL_TESTS").is_some_and(|v| !v.is_empty() && v != "0")
}

fn start_gtk_test_runner() -> GtkTestRunner {
    let (tx, rx) = std::sync::mpsc::channel::<GtkTestJob>();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("gtk-test-worker".into())
        .spawn(move || {
            // gtk_init would otherwise apply the user's locale to the
            // whole test process, so date tests on other threads would
            // format month names in, say, Dutch depending on test order.
            gtk::disable_setlocale();
            let init = match std::panic::catch_unwind(gtk::init) {
                Ok(Ok(())) => Ok(()),
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("gtk::init panicked".to_string()),
            };
            let ok = init.is_ok();
            let _ = init_tx.send(init);
            if !ok {
                return;
            }
            while let Ok(job) = rx.recv() {
                job();
            }
        })
        .map_err(|err| err.to_string())?;
    init_rx.recv().map_err(|err| err.to_string())??;
    Ok(tx)
}

/// Whether a test thread named `thread_name` may call `run_gtk_test`. libtest
/// names each test thread after its path; names that are not test paths (no
/// `::`) are not checked.
fn graphical_placement_ok(thread_name: &str) -> bool {
    !thread_name.contains("::") || thread_name.contains("::graphical::")
}

/// Runs `f` on a shared GTK thread. Returns at once, passing as skipped,
/// unless `CONDUIT_GRAPHICAL_TESTS` is set; then GTK must initialise or the
/// test panics, so a missing display never passes silently. The caller must
/// live in a `mod graphical`, which is how CI selects graphical tests.
pub(crate) fn run_gtk_test<F: FnOnce() + Send + 'static>(f: F) {
    let current = std::thread::current();
    if let Some(name) = current.name() {
        assert!(
            graphical_placement_ok(name),
            "{name} uses run_gtk_test outside a `mod graphical`; move it there, \
             or CI's `graphical::` filter will never run it"
        );
    }
    if !graphical_tests_enabled() {
        return;
    }
    let sender = GTK_TEST_RUNNER
        .get_or_init(start_gtk_test_runner)
        .as_ref()
        .unwrap_or_else(|err| {
            panic!("CONDUIT_GRAPHICAL_TESTS is set but GTK failed to initialise: {err}")
        });

    let (done_tx, done_rx) =
        std::sync::mpsc::channel::<Result<(), std::boxed::Box<dyn std::any::Any + Send>>>();
    let job = std::boxed::Box::new(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        let _ = done_tx.send(res);
    });
    sender.send(job).expect("GTK test worker is gone");
    if let Err(panic_payload) = done_rx.recv().expect("GTK test worker died mid-test") {
        std::panic::resume_unwind(panic_payload);
    }
}

#[test]
fn rich_text_channel_element_renders_as_pill_instead_of_vanishing() {
    let token = crate::timeline_document_widget::inlines_to_mrkdwn(&[
        crate::rich_message::RichInline::Channel("C123".to_string()),
    ]);
    assert_eq!(token, "<#C123>");

    let mut context = test_context();
    Arc::make_mut(&mut context.conversation_titles)
        .insert("C123".to_string(), "#general".to_string());
    let pango = crate::message_markup::mrkdwn_to_pango(&token, &context);
    assert!(pango.contains("background=\"#D6ECFF\""));
    assert!(pango.contains("general"));
}

#[test]
fn rich_text_usergroup_element_renders_its_handle() {
    let token = crate::timeline_document_widget::inlines_to_mrkdwn(&[
        crate::rich_message::RichInline::UserGroup("S0614TZR7".to_string()),
    ]);
    let mut context = test_context();
    Arc::make_mut(&mut context.user_group_names)
        .insert("S0614TZR7".to_string(), "cloudops-support".to_string());
    let pango = crate::message_markup::mrkdwn_to_pango(&token, &context);
    assert!(pango.contains("@cloudops-support"), "{pango}");
    assert!(
        pango.contains("<a href=\"conduit-usergroup://S0614TZR7\">"),
        "group pill should link to its members: {pango}"
    );
}

#[test]
fn graphical_placement_accepts_only_graphical_modules_or_non_test_threads() {
    assert!(graphical_placement_ok("a::graphical::b"));
    assert!(!graphical_placement_ok("a::tests::b"));
    assert!(graphical_placement_ok("gtk-test-worker"));
}

#[test]
fn media_collapse_key_is_stable_and_namespaced_per_slot() {
    let a = media_collapse_key("1710000000.000100", "file:0");
    let b = media_collapse_key("1710000000.000100", "file:0");
    assert_eq!(
        a, b,
        "querying the same ts/slot twice must yield the same key"
    );

    let c = media_collapse_key("1710000000.000100", "attachment:0");
    assert_ne!(
        a, c,
        "a file and an attachment at the same index must not collide"
    );
}

fn descendants(root: &Widget) -> Vec<Widget> {
    let mut found = Vec::new();
    let mut child = root.first_child();
    while let Some(widget) = child {
        found.push(widget.clone());
        found.extend(descendants(&widget));
        child = widget.next_sibling();
    }
    found
}

fn label_texts(root: &Widget) -> Vec<String> {
    descendants(root)
        .into_iter()
        .filter_map(|widget| widget.downcast::<Label>().ok())
        .map(|label| label.text().to_string())
        .collect()
}

fn cached_roundtrip(message: SlackMessage) -> SlackMessage {
    let stored = serde_json::to_value(crate::slack_message_wire::normalize_cached_message(message))
        .expect("message serializes for the cache");
    crate::slack_message_wire::normalize_cached_message(
        serde_json::from_value(stored).expect("cached message deserializes"),
    )
}

fn giphy_command_message() -> SlackMessage {
    crate::slack_message_wire::SlackMessageWire::from_value(serde_json::json!({
        "type": "message",
        "user": "U04R5M67EBV",
        "bot_id": "B8D9J9E80",
        "app_id": "A0F827J2C",
        "text": "plumber",
        "ts": "1789462599.765779",
        "bot_profile": {
            "id": "B8D9J9E80",
            "app_id": "A0F827J2C",
            "name": "giphy",
            "icons": {"image_72": "https://a.slack-edge.com/dc483/img/plugins/giphy/service_72.png"}
        },
        "blocks": [
            {
                "type": "image",
                "block_id": "giphy",
                "title": {"type": "plain_text", "text": "plumber", "emoji": true},
                "image_url": "https://media4.giphy.com/media/9TbgGqK1KhpnNORo9a/giphy.gif?cid=1&rid=giphy.gif&ct=g",
                "alt_text": "plumber",
                "image_width": 480,
                "image_height": 270,
                "image_bytes": 1234567,
                "is_animated": true
            },
            {
                "type": "context",
                "elements": [
                    {
                        "type": "image",
                        "image_url": "https://a.slack-edge.com/dc483/img/plugins/giphy/service_32.png",
                        "alt_text": "giphy logo"
                    },
                    {
                        "type": "mrkdwn",
                        "text": "Posted using /giphy | GIF by <https://giphy.com/channel/fuzzyghost/|fuzzyghost>"
                    }
                ]
            }
        ]
    }))
    .into_message()
    .expect("giphy message parses")
}

#[test]
fn giphy_command_message_keeps_title_image_context_and_user_through_cache() {
    let message = cached_roundtrip(giphy_command_message());

    assert_eq!(message.app_invoking_user_id(), Some("U04R5M67EBV"));
    let crate::rich_message::MessageNode::Image(image) = &message.document.nodes()[0] else {
        panic!(
            "first node should be the GIF image: {:?}",
            message.document.nodes()
        );
    };
    assert_eq!(image.title.as_deref(), Some("plumber"));
    assert_eq!((image.width, image.height), (Some(480), Some(270)));
    let crate::rich_message::MessageNode::Context(elements) = &message.document.nodes()[1] else {
        panic!("second node should be the context line");
    };
    assert!(matches!(
        &elements[0],
        crate::rich_message::MessageContextElement::Image(icon)
            if icon.url.as_deref()
                == Some("https://a.slack-edge.com/dc483/img/plugins/giphy/service_32.png")
    ));
    assert_eq!(
        message.document.image_urls().collect::<Vec<_>>(),
        vec![
            "https://media4.giphy.com/media/9TbgGqK1KhpnNORo9a/giphy.gif?cid=1&rid=giphy.gif&ct=g",
            "https://a.slack-edge.com/dc483/img/plugins/giphy/service_32.png",
        ]
    );
}

#[test]
fn app_message_author_prefers_known_invoking_person_only() {
    let message = cached_roundtrip(giphy_command_message());
    let mut ctx = test_context();
    assert_eq!(
        ctx.display_user_id(&message),
        None,
        "unknown user keeps the app"
    );

    Arc::make_mut(&mut ctx.user_full_names)
        .insert("U04R5M67EBV".to_string(), "Robey Groeneweg".to_string());
    assert_eq!(ctx.display_user_id(&message), Some("U04R5M67EBV"));

    Arc::make_mut(&mut ctx.bot_user_ids).insert("U04R5M67EBV".to_string());
    assert_eq!(
        ctx.display_user_id(&message),
        None,
        "bot users keep the app"
    );
}

pub(crate) struct TempTestDir(PathBuf);

impl TempTestDir {
    pub(crate) fn new(prefix: &str) -> Self {
        static NEXT_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NEXT_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "conduit-media-test-{prefix}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn test_parse_text_segments() {
    let text = "Hello\n```rust\nfn main() {}\n```\n> quoted\nMore text";
    let segments = parse_text_segments(text);
    assert_eq!(segments.len(), 4);
    assert_eq!(segments[0], TextSegment::Normal("Hello".to_string()));
    assert_eq!(
        segments[1],
        TextSegment::CodeBlock {
            lang: Some("rust".to_string()),
            code: "fn main() {}".to_string(),
        }
    );
    assert_eq!(segments[2], TextSegment::Quote("quoted".to_string()));
    assert_eq!(segments[3], TextSegment::Normal("More text".to_string()));
}

#[test]
fn test_file_size_formatting() {
    assert_eq!(format_file_size(500), "500 B");
    assert_eq!(format_file_size(2048), "2.0 KB");
    assert_eq!(format_file_size(1572864), "1.5 MB");
    assert_eq!(format_file_size(10737418240), "10.0 GB");
}

fn find_descendant(widget: &Widget, pred: &dyn Fn(&Widget) -> bool) -> Option<Widget> {
    if pred(widget) {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(found) = find_descendant(&current, pred) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

fn find_menu_button(widget: &Widget) -> Option<gtk::MenuButton> {
    if let Some(button) = widget.downcast_ref::<gtk::MenuButton>() {
        return Some(button.clone());
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(found) = find_menu_button(&current) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

fn read_test_messages() -> Vec<SlackMessage> {
    (0..3)
        .rev()
        .map(|i| SlackMessage {
            ts: format!("1700000{i:03}.000100"),
            user: Some("U123".to_string()),
            text: Some(format!("message {i}")),
            ..Default::default()
        })
        .collect()
}

/// Spins the default main context for `ms` milliseconds.
pub(crate) fn pump(ms: u64) {
    let context = glib::MainContext::default();
    let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    while std::time::Instant::now() < end {
        context.iteration(false);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Pumps until `done` holds or `ms` elapse, so a loaded test run still
/// gets the first layout and dwell in.
pub(crate) fn pump_until(ms: u64, done: impl Fn() -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    while !done() && std::time::Instant::now() < end {
        pump(10);
    }
}

/// Presents `window` and pumps until its first layout. On Wayland the
/// toplevel is only sized once the compositor configures it, and a
/// GtkListView `scroll_to` issued before that is lost, so a timeline filled
/// in the same turn as `present()` would open at its oldest row there.
fn present_and_wait_for_layout(window: &gtk::Window) {
    window.present();
    pump_until(5000, || window.height() > 0);
    assert!(window.height() > 0, "window was never laid out");
}

/// Pumps until the timeline sits at its newest message, failing loudly if
/// it never gets there.
fn wait_until_at_bottom(view: &NativeTimelineView) {
    let vadj = view.scrolled_window.vadjustment();
    let at_bottom = || vadj.value() > 0.0 && vadj.upper() - vadj.page_size() - vadj.value() < 1.0;
    pump_until(5000, at_bottom);
    assert!(
        at_bottom(),
        "opens at the newest message: value {} of upper {} page {}",
        vadj.value(),
        vadj.upper(),
        vadj.page_size()
    );
}

fn record_auto_marks(view: &NativeTimelineView) -> Rc<RefCell<Vec<String>>> {
    let marks: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = marks.clone();
    view.set_on_action(move |action| {
        if let TimelineAction::AutoMarkRead { ts, .. } = action {
            recorded.borrow_mut().push(ts);
        }
    });
    marks
}

/// Bound message rows and their widgets; a rebind replaces the widget.
fn bound_row_widgets(view: &NativeTimelineView) -> Vec<(String, Widget)> {
    view.bound_items
        .borrow()
        .iter()
        .filter_map(glib::WeakRef::upgrade)
        .filter_map(|item| {
            let obj = item.item().and_downcast::<TimelineMessageObject>()?;
            let child = item.child()?;
            (!obj.is_day_separator()).then(|| (obj.message().ts, child))
        })
        .collect()
}

/// Timestamps whose bound widget differs from `before`.
fn rebound_rows(view: &NativeTimelineView, before: &[(String, Widget)]) -> Vec<String> {
    let after = bound_row_widgets(view);
    before
        .iter()
        .filter(|(ts, widget)| {
            after
                .iter()
                .find(|(after_ts, _)| after_ts == ts)
                .is_some_and(|(_, after_widget)| after_widget != widget)
        })
        .map(|(ts, _)| ts.clone())
        .collect()
}

type TextureExecutor = decode::ManualExecutor<decode::TextureKey, gtk::gdk::Texture>;

/// Runs static texture decodes on a manual executor until dropped.
struct ManualDecodes(Rc<TextureExecutor>);

impl ManualDecodes {
    fn install() -> Self {
        clear_timeline_view_caches();
        let executor = TextureExecutor::new();
        decode::install_texture_executor_for_tests(executor.clone());
        Self(executor)
    }

    /// Decodes everything running, and whatever that lets start, as the
    /// workers would. Returns the keys in completion order.
    fn finish_all(&self) -> Vec<decode::TextureKey> {
        let mut finished = Vec::new();
        while let Some(key) = self.0.running().first().cloned() {
            assert!(finished.len() < 1000, "decodes keep coming");
            self.0.complete(0, decode::decode_texture_file(&key.path));
            finished.push(key);
        }
        finished
    }
}

impl Drop for ManualDecodes {
    fn drop(&mut self) {
        decode::reset_texture_decoder_for_tests();
    }
}

/// A directory inside the image asset cache, where row binds accept files.
struct AssetCacheTestDir(PathBuf);

impl AssetCacheTestDir {
    fn new(prefix: &str) -> Self {
        let dir = crate::config::image_asset_cache_dir()
            .join(format!("conduit-test-{prefix}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("asset cache test dir");
        Self(dir)
    }
}

impl Drop for AssetCacheTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Pictures under `root` that wait on, or show, an off-thread decode.
fn deferred_pictures(root: &Widget) -> Vec<(Picture, decode::DeferredTexture)> {
    descendants(root)
        .into_iter()
        .filter_map(|widget| widget.downcast::<Picture>().ok())
        .filter_map(|picture| {
            let deferred = picture
                .paintable()
                .and_downcast::<decode::DeferredTexture>()?;
            Some((picture, deferred))
        })
        .collect()
}

fn collect_link_labels(widget: &Widget, labels: &mut Vec<Label>) {
    if let Some(label) = widget.downcast_ref::<Label>() {
        if label.label().contains("<a href=") {
            labels.push(label.clone());
        }
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        collect_link_labels(&current, labels);
        child = current.next_sibling();
    }
}

#[test]
fn row_qualifies_when_fully_visible() {
    assert!(row_qualifies_for_read(0.0, 100.0, 500.0));
}

#[test]
fn row_qualifies_via_own_height_even_if_small_viewport_fraction() {
    // A short row (40px) fully on-screen inside a tall viewport (2000px)
    // satisfies the row-height condition even though it's a tiny sliver
    // of the viewport.
    assert!(row_qualifies_for_read(10.0, 50.0, 2000.0));
}

#[test]
fn row_qualifies_via_viewport_height_for_a_row_taller_than_the_viewport() {
    // A row taller than the viewport can never show 80% of itself, but
    // qualifies once it covers 80% of the viewport instead.
    assert!(row_qualifies_for_read(-500.0, 1000.0, 400.0));
}

#[test]
fn row_does_not_qualify_when_mostly_scrolled_past() {
    // Only the top 20px of a 100px row is visible in a tall viewport.
    assert!(!row_qualifies_for_read(-80.0, 20.0, 2000.0));
}

#[test]
fn row_does_not_qualify_when_fully_offscreen() {
    assert!(!row_qualifies_for_read(600.0, 700.0, 500.0));
    assert!(!row_qualifies_for_read(-200.0, -100.0, 500.0));
}

#[test]
fn row_does_not_qualify_for_degenerate_sizes() {
    assert!(!row_qualifies_for_read(0.0, 0.0, 500.0));
    assert!(!row_qualifies_for_read(0.0, 100.0, 0.0));
}

#[test]
fn test_extract_custom_emojis_and_prepare_markup() {
    let pango = "Hello <b><conduit-custom-emoji name=\"heart-sparkle\" url=\"https://example.com/heart.gif\"/></b> world";
    let (clean, emojis) = extract_custom_emojis_and_prepare_markup(pango);
    assert_eq!(clean, "Hello <b>\u{FFFC}</b> world");
    assert_eq!(emojis.len(), 1);
    assert_eq!(emojis[0].name, "heart-sparkle");
    assert_eq!(emojis[0].url, "https://example.com/heart.gif");
}

#[test]
fn test_can_group_messages() {
    let prev = SlackMessage {
        ts: "1700000000.000100".to_string(),
        user: Some("U123".to_string()),
        text: Some("first".to_string()),
        ..Default::default()
    };
    let curr_same = SlackMessage {
        ts: "1700000050.000100".to_string(),
        user: Some("U123".to_string()),
        text: Some("second".to_string()),
        ..Default::default()
    };
    assert!(can_group_messages(&prev, &curr_same));

    // Different user
    let curr_diff_user = SlackMessage {
        ts: "1700000050.000100".to_string(),
        user: Some("U456".to_string()),
        text: Some("second".to_string()),
        ..Default::default()
    };
    assert!(!can_group_messages(&prev, &curr_diff_user));

    // >= 180 seconds apart
    let curr_late = SlackMessage {
        ts: "1700000181.000100".to_string(),
        user: Some("U123".to_string()),
        text: Some("too late".to_string()),
        ..Default::default()
    };
    assert!(!can_group_messages(&prev, &curr_late));

    // Negative delta (out of order)
    let curr_before = SlackMessage {
        ts: "1699999990.000100".to_string(),
        user: Some("U123".to_string()),
        text: Some("earlier".to_string()),
        ..Default::default()
    };
    assert!(!can_group_messages(&prev, &curr_before));

    // System message subtype
    let curr_system = SlackMessage {
        ts: "1700000050.000100".to_string(),
        user: Some("U123".to_string()),
        subtype: Some("channel_join".to_string()),
        ..Default::default()
    };
    assert!(!can_group_messages(&prev, &curr_system));

    // Thread broadcast
    let curr_broadcast = SlackMessage {
        ts: "1700000050.000100".to_string(),
        user: Some("U123".to_string()),
        is_thread_broadcast: Some(true),
        ..Default::default()
    };
    assert!(!can_group_messages(&prev, &curr_broadcast));
}

fn save_test_image(path: &Path, format: &str, width: i32, height: i32) {
    gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, width, height)
        .expect("pixbuf")
        .savev(path, format, &[])
        .expect("save");
}

fn cached_reaction_size(path: &Path) -> (i32, i32) {
    let texture = REACTION_TEXTURE_CACHE
        .with(|cache| cache.borrow_mut().get(path))
        .expect("emoji cached");
    (texture.width(), texture.height())
}

#[test]
fn scroll_behavior_pins_only_fresh_bottom_or_following_renders() {
    use crate::message_markup::TimelineScrollBehavior as B;
    for behavior in [B::Preserve, B::PreservePrepend, B::Bottom, B::StickToBottom] {
        assert!(pins_to_bottom(behavior, true, false), "{behavior:?} fresh");
    }
    assert!(pins_to_bottom(B::Bottom, false, false));
    assert!(pins_to_bottom(B::StickToBottom, false, true));
    assert!(!pins_to_bottom(B::StickToBottom, false, false));
    assert!(!pins_to_bottom(B::Preserve, false, true));
    assert!(!pins_to_bottom(B::PreservePrepend, false, true));
}

#[test]
fn oldest_edge_is_within_half_a_page_of_the_top() {
    assert!(near_oldest_edge(0.0, 500.0));
    assert!(near_oldest_edge(249.0, 500.0));
    assert!(!near_oldest_edge(250.0, 500.0));
    assert!(!near_oldest_edge(0.0, 0.0), "unallocated view never pages");
}

#[test]
fn older_page_backoff_grows_caps_and_resets() {
    use std::time::{Duration, Instant};
    let backoff = OlderPageBackoff::default();
    let now = Instant::now();
    assert!(!backoff.cooling_down(now));
    let delays: Vec<u64> = (0..7).map(|_| backoff.fail(now).as_secs()).collect();
    assert_eq!(delays, vec![2, 4, 8, 16, 30, 30, 30]);
    assert!(backoff.cooling_down(now + Duration::from_secs(29)));
    assert!(!backoff.cooling_down(now + Duration::from_secs(30)));
    backoff.reset();
    assert!(!backoff.cooling_down(now));
    assert_eq!(backoff.fail(now), Duration::from_secs(2));
}

/// Tests that create GTK widgets. They run only when
/// `CONDUIT_GRAPHICAL_TESTS` is set (see `run_gtk_test`).
mod graphical {
    use super::*;

    #[test]
    fn giphy_command_message_renders_user_title_image_and_context() {
        run_gtk_test(|| {
            let message = cached_roundtrip(giphy_command_message());
            let mut ctx = test_context();
            Arc::make_mut(&mut ctx.user_full_names)
                .insert("U04R5M67EBV".to_string(), "Robey Groeneweg".to_string());

            let widget =
                build_timeline_message_widget(&message, &ctx, None, None, None, None, None, true)
                    .upcast::<Widget>();
            let texts = label_texts(&widget);

            assert!(
                texts.iter().any(|text| text == "Robey Groeneweg"),
                "{texts:?}"
            );
            assert!(!texts.iter().any(|text| text == "giphy"), "{texts:?}");
            assert!(texts.iter().any(|text| text == "plumber"), "{texts:?}");
            assert!(
                texts
                    .iter()
                    .any(|text| text.starts_with("Posted using /giphy | GIF by")),
                "{texts:?}"
            );
            assert!(descendants(&widget)
                .iter()
                .any(|widget| widget.has_css_class("timeline-media-title")));
            assert!(descendants(&widget)
                .iter()
                .any(|widget| widget.has_css_class("timeline-media-placeholder")));
        });
    }

    #[test]
    fn cached_gif_picker_message_renders_titled_placeholder() {
        run_gtk_test(|| {
            let cached: SlackMessage = serde_json::from_value(serde_json::json!({
            "type": "message",
            "user": "U015HMNHYES",
            "text": "",
            "ts": "1789462245.306079",
            "files": null,
            "blocks": null,
            "attachments": null,
            "author": {"User": {"user_id": "U015HMNHYES"}},
            "document": {
                "nodes": [{"Image": {
                    "url": "https://media1.giphy.com/media/VF4jocEMAWVAVYRIQu/200w.gif?cid=b7&rid=200w.gif&ct=g",
                    "alt": "Oh Yeah Yes GIF by FILMRISE",
                    "title": "GIF"
                }}],
                "accessible_fallback": null
            },
            "content_version": 1
        }))
        .expect("cached GIF picker message deserializes");
            let message = crate::slack_message_wire::normalize_cached_message(cached);

            let widget = build_timeline_message_widget(
                &message,
                &test_context(),
                None,
                None,
                None,
                None,
                None,
                true,
            )
            .upcast::<Widget>();

            assert!(label_texts(&widget).iter().any(|text| text == "GIF"));
            let placeholder = descendants(&widget)
                .into_iter()
                .find(|widget| widget.has_css_class("timeline-media-placeholder"))
                .expect("pending GIF reserves a placeholder");
            assert_eq!(
                placeholder.tooltip_text().as_deref(),
                Some("Oh Yeah Yes GIF by FILMRISE")
            );
        });
    }

    #[test]
    fn downloaded_document_image_renders_picture_fitted_to_media_box() {
        run_gtk_test(|| {
            let temp_dir = TempTestDir::new("wide");
            let path = temp_dir.path().join("wide.png");
            gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, 960, 540)
                .expect("pixbuf")
                .savev(&path, "png", &[])
                .expect("png written");

            let image = crate::rich_message::MessageImage::new(
                Some("https://media1.giphy.com/media/x/giphy.gif".to_string()),
                "wide",
                Some("GIF".to_string()),
            );
            let resolved = path.clone();
            let resolve = move |_: &str| Some(resolved.clone());
            let is_failed = |_: &str| false;
            let target = Box::new(Orientation::Vertical, 0);
            crate::timeline_document_widget::DocumentRenderer::new(
                &test_context(),
                crate::timeline_media::MediaSources {
                    resolve: &resolve,
                    is_failed: &is_failed,
                },
                "1789462245.306079",
                None,
            )
            .render(&[crate::rich_message::MessageNode::Image(image)], &target);

            let picture = descendants(target.upcast_ref())
                .into_iter()
                .find_map(|widget| widget.downcast::<Picture>().ok())
                .expect("downloaded image renders a picture");
            assert_eq!(picture.size_request(), (480, 270));
            assert!(picture.has_css_class("timeline-media-image"));
        });
    }

    #[test]
    fn file_shown_as_document_image_is_not_rendered_twice() {
        run_gtk_test(|| {
            let url = "https://files.slack.com/files-pri/T1-F1/photo.png";
            let mut message = SlackMessage {
                ts: "1700000000.000500".to_string(),
                user: Some("U123".to_string()),
                files: Some(vec![SlackFile {
                    id: Some("F1".to_string()),
                    mimetype: Some("image/png".to_string()),
                    url_private: Some(url.to_string()),
                    thumb_360: Some(url.to_string()),
                    ..Default::default()
                }]),
                blocks: Some(serde_json::json!([{
                    "type": "image",
                    "alt_text": "photo",
                    "slack_file": {"url": url}
                }])),
                ..Default::default()
            };
            message.refresh_canonical_content();

            let widget = build_timeline_message_widget(
                &message,
                &test_context(),
                None,
                None,
                None,
                None,
                None,
                true,
            )
            .upcast::<Widget>();
            let all = descendants(&widget);

            assert!(!all
                .iter()
                .any(|widget| widget.has_css_class("timeline-image-container")));
            assert_eq!(
                all.iter()
                    .filter(|widget| widget.downcast_ref::<ToggleButton>().is_some())
                    .count(),
                1
            );
        });
    }

    #[test]
    fn author_header_is_a_menu_target_for_people_only() {
        run_gtk_test(|| {
            let message = SlackMessage {
                ts: "1700000000.000100".to_string(),
                user: Some("U123".to_string()),
                text: Some("hi".to_string()),
                ..Default::default()
            };

            let seen: Rc<RefCell<Vec<String>>> = Rc::default();
            let recorder = seen.clone();
            let on_action: Rc<dyn Fn(TimelineAction)> = Rc::new(move |action| {
                recorder.borrow_mut().push(format!("{action:?}"));
            });

            let widget = build_timeline_message_widget(
                &message,
                &test_context(),
                None,
                None,
                None,
                None,
                Some(&on_action),
                true,
            );
            let button = find_menu_button(widget.upcast_ref()).expect("author menu button");
            assert!(button.menu_model().is_some());
            let mut focusable = 0;
            let mut stack = vec![widget.clone().upcast::<Widget>()];
            while let Some(current) = stack.pop() {
                if current.is::<gtk::ToggleButton>() && current.is_focusable() {
                    focusable += 1;
                }
                let mut child = current.first_child();
                while let Some(next) = child {
                    child = next.next_sibling();
                    stack.push(next);
                }
            }
            assert_eq!(focusable, 1, "only the name button is focusable");
            button.activate_action("author.profile", None).unwrap();
            button.activate_action("author.message", None).unwrap();
            assert_eq!(
                *seen.borrow(),
                ["ShowProfile(\"U123\")", "MessageUser(\"U123\")"]
            );

            // Yourself: Message is disabled, Profile stays available.
            let mut own = test_context();
            own.current_user_id = Some("U123".to_string());
            let widget = build_timeline_message_widget(
                &message,
                &own,
                None,
                None,
                None,
                None,
                Some(&on_action),
                true,
            );
            let button = find_menu_button(widget.upcast_ref()).expect("author menu button");
            seen.borrow_mut().clear();
            button.activate_action("author.message", None).unwrap();
            assert!(seen.borrow().is_empty());
            button.activate_action("author.profile", None).unwrap();
            assert_eq!(seen.borrow().len(), 1);

            // Bots get a plain header.
            let mut bot = test_context();
            bot.bot_user_ids = Arc::new(HashSet::from(["U123".to_string()]));
            let widget = build_timeline_message_widget(
                &message,
                &bot,
                None,
                None,
                None,
                None,
                Some(&on_action),
                true,
            );
            assert!(find_menu_button(widget.upcast_ref()).is_none());
        });
    }

    #[test]
    fn test_timeline_message_widget_all_gtk() {
        run_gtk_test(|| {
            let ctx = test_context();

            // 1. Basic message
            let msg1 = SlackMessage {
                ts: "1700000000.000100".to_string(),
                user: Some("U123".to_string()),
                text: Some("Hello *world*!".to_string()),
                reactions: Some(vec![
                    SlackReaction {
                        name: Some("thumbsup".to_string()),
                        count: Some(3),
                        users: None,
                    },
                    SlackReaction {
                        name: Some("smile".to_string()),
                        count: Some(1),
                        users: None,
                    },
                ]),
                reply_count: Some(5),
                ..Default::default()
            };

            let widget1 =
                build_timeline_message_widget(&msg1, &ctx, None, None, None, None, None, true);
            assert_eq!(widget1.orientation(), Orientation::Vertical);

            // 2. Section blocks and fields
            let msg2 = SlackMessage {
                ts: "1700000000.000200".to_string(),
                user: Some("U123".to_string()),
                blocks: Some(serde_json::json!([
                    {
                        "type": "header",
                        "text": { "type": "plain_text", "text": "Header Title" }
                    },
                    {
                        "type": "section",
                        "text": { "type": "mrkdwn", "text": "Main section content" },
                        "fields": [
                            { "type": "mrkdwn", "text": "*Field 1*" },
                            { "type": "mrkdwn", "text": "*Field 2*" }
                        ]
                    }
                ])),
                ..Default::default()
            };

            let widget2 =
                build_timeline_message_widget(&msg2, &ctx, None, None, None, None, None, true);
            assert_eq!(widget2.orientation(), Orientation::Vertical);

            // 3. Divider, actions and context
            let msg3 = SlackMessage {
                ts: "1700000000.000300".to_string(),
                user: Some("U123".to_string()),
                blocks: Some(serde_json::json!([
                    { "type": "divider" },
                    {
                        "type": "actions",
                        "elements": [
                            { "type": "button", "text": { "type": "plain_text", "text": "Approve" } },
                            { "type": "button", "text": { "type": "plain_text", "text": "Reject" } }
                        ]
                    },
                    {
                        "type": "context",
                        "elements": [
                            { "type": "mrkdwn", "text": "Footer context info" }
                        ]
                    }
                ])),
                ..Default::default()
            };

            let widget3 =
                build_timeline_message_widget(&msg3, &ctx, None, None, None, None, None, true);
            assert_eq!(widget3.orientation(), Orientation::Vertical);

            // 4. Attachments with color border
            let msg4 = SlackMessage {
                ts: "1700000000.000400".to_string(),
                user: Some("U123".to_string()),
                attachments: Some(vec![SlackAttachment {
                    color: Some("good".to_string()),
                    pretext: Some("Pretext label".to_string()),
                    title: Some("Attachment Title".to_string()),
                    title_link: Some("https://example.com".to_string()),
                    text: Some("Attachment body text".to_string()),
                    fields: Some(vec![SlackAttachmentField {
                        title: Some("Priority".to_string()),
                        value: Some("High".to_string()),
                        short: Some(true),
                    }]),
                    ..Default::default()
                }]),
                ..Default::default()
            };

            let widget4 =
                build_timeline_message_widget(&msg4, &ctx, None, None, None, None, None, true);
            assert_eq!(widget4.orientation(), Orientation::Vertical);

            // 5. Image file
            let msg_img = SlackMessage {
                ts: "1700000000.000500".to_string(),
                user: Some("U123".to_string()),
                files: Some(vec![SlackFile {
                    id: Some("F1".to_string()),
                    name: Some("photo.png".to_string()),
                    title: Some("Sample Photo".to_string()),
                    mimetype: Some("image/png".to_string()),
                    thumb_360: Some("https://example.com/thumb.png".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            };

            let widget_img =
                build_timeline_message_widget(&msg_img, &ctx, None, None, None, None, None, true);
            assert_eq!(widget_img.orientation(), Orientation::Vertical);

            // 6. Video file
            let msg_vid = SlackMessage {
                ts: "1700000000.000600".to_string(),
                user: Some("U123".to_string()),
                files: Some(vec![SlackFile {
                    id: Some("F2".to_string()),
                    name: Some("demo.mp4".to_string()),
                    title: Some("Demo Recording".to_string()),
                    mimetype: Some("video/mp4".to_string()),
                    thumb_video: Some("https://example.com/video_thumb.png".to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            };

            let widget_vid =
                build_timeline_message_widget(&msg_vid, &ctx, None, None, None, None, None, true);
            assert_eq!(widget_vid.orientation(), Orientation::Vertical);

            // 7. Document file with size
            let msg_doc = SlackMessage {
                ts: "1700000000.000700".to_string(),
                user: Some("U123".to_string()),
                files: Some(vec![SlackFile {
                    id: Some("F3".to_string()),
                    name: Some("report.pdf".to_string()),
                    title: Some("Annual Report".to_string()),
                    mimetype: Some("application/pdf".to_string()),
                    size: Some(2097152),
                    ..Default::default()
                }]),
                ..Default::default()
            };

            let widget_doc =
                build_timeline_message_widget(&msg_doc, &ctx, None, None, None, None, None, true);
            assert_eq!(widget_doc.orientation(), Orientation::Vertical);

            // 8. Rich text blocks
            let msg_rich = SlackMessage {
                ts: "1700000000.000800".to_string(),
                user: Some("U123".to_string()),
                blocks: Some(serde_json::json!([
                    {
                        "type": "rich_text",
                        "elements": [
                            {
                                "type": "rich_text_section",
                                "elements": [
                                    { "type": "text", "text": "Rich text section ", "style": { "bold": true } },
                                    { "type": "link", "url": "https://example.com", "text": "click here" }
                                ]
                            },
                            {
                                "type": "rich_text_list",
                                "style": "bullet",
                                "indent": 1,
                                "elements": [
                                    {
                                        "type": "rich_text_section",
                                        "elements": [ { "type": "text", "text": "Bullet item 1" } ]
                                    },
                                    {
                                        "type": "rich_text_section",
                                        "elements": [ { "type": "text", "text": "Bullet item 2" } ]
                                    }
                                ]
                            },
                            {
                                "type": "rich_text_preformatted",
                                "elements": [
                                    { "type": "text", "text": "let x = 42;" }
                                ]
                            },
                            {
                                "type": "rich_text_quote",
                                "elements": [
                                    { "type": "text", "text": "A quoted phrase" }
                                ]
                            }
                        ]
                    }
                ])),
                ..Default::default()
            };

            let widget_rich =
                build_timeline_message_widget(&msg_rich, &ctx, None, None, None, None, None, true);
            assert_eq!(widget_rich.orientation(), Orientation::Vertical);

            // 9. Subtype system message
            let msg_sys = SlackMessage {
                ts: "1700000000.000900".to_string(),
                user: Some("U123".to_string()),
                subtype: Some("channel_join".to_string()),
                text: Some("joined the channel".to_string()),
                ..Default::default()
            };

            let widget_sys =
                build_timeline_message_widget(&msg_sys, &ctx, None, None, None, None, None, true);
            assert_eq!(widget_sys.orientation(), Orientation::Horizontal);

            // 10. Thread broadcast banner
            let msg_bc = SlackMessage {
                ts: "1700000000.0001000".to_string(),
                user: Some("U123".to_string()),
                text: Some("Broadcast reply".to_string()),
                is_thread_broadcast: Some(true),
                ..Default::default()
            };

            let widget_bc =
                build_timeline_message_widget(&msg_bc, &ctx, None, None, None, None, None, true);
            assert_eq!(widget_bc.orientation(), Orientation::Vertical);

            // 11. Native timeline view & update_image_asset
            let timeline_view = NativeTimelineView::new();
            timeline_view.set_messages(&[msg1, msg2], &ctx, None);
            // Two same-day messages: one leading day separator plus both rows.
            assert_eq!(timeline_view.store.n_items(), 3);
            timeline_view.update_image_asset(&ctx, HashSet::new());
            assert_eq!(timeline_view.store.n_items(), 3);

            // Moving the unread separator rebuilds rows in place.
            let separator_ts = |view: &NativeTimelineView| {
                view.context
                    .borrow()
                    .as_ref()
                    .and_then(|context| context.unread_separator_ts.clone())
            };
            let second_ts = timeline_view
                .store
                .item(2)
                .and_downcast::<TimelineMessageObject>()
                .expect("message row")
                .message()
                .ts;
            timeline_view.set_unread_separator(Some(second_ts.clone()));
            assert_eq!(separator_ts(&timeline_view), Some(second_ts));
            assert_eq!(timeline_view.store.n_items(), 3);
            timeline_view.set_unread_separator(None);
            assert_eq!(separator_ts(&timeline_view), None);
            assert_eq!(timeline_view.store.n_items(), 3);

            // 12. Custom emoji reactions & resolve_cached_asset_path
            let mut ctx_emoji = ctx.clone();
            std::sync::Arc::make_mut(&mut ctx_emoji.custom_emojis).insert(
                "party_blob".to_string(),
                "https://example.com/blob.gif".to_string(),
            );
            let msg_reaction = SlackMessage {
                ts: "1700000000.001100".to_string(),
                user: Some("U123".to_string()),
                text: Some("Reaction test".to_string()),
                reactions: Some(vec![
                    SlackReaction {
                        name: Some(":robot_face:".to_string()),
                        count: Some(5),
                        ..Default::default()
                    },
                    SlackReaction {
                        name: Some(":party_blob:".to_string()),
                        count: Some(2),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            };
            let widget_rx = build_timeline_message_widget(
                &msg_reaction,
                &ctx_emoji,
                None,
                None,
                None,
                None,
                None,
                true,
            );
            assert_eq!(widget_rx.orientation(), Orientation::Vertical);

            let path = resolve_cached_asset_path("nonexistent_key", &ctx_emoji);
            assert!(path.is_none());

            // 13. Direct disk cache hash lookup test
            let cache_dir = crate::config::image_asset_cache_dir();
            let ws_dir = cache_dir.join("test_ws_direct");
            let _ = std::fs::create_dir_all(&ws_dir);

            let test_url = "https://example.com/direct_test_img.png";
            let hash = {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update("test_ws_direct".as_bytes());
                hasher.update([0]);
                hasher.update(test_url.as_bytes());
                format!("{:x}", hasher.finalize())
            };
            let dummy_file = ws_dir.join(format!("{hash}.png"));
            std::fs::write(&dummy_file, b"test").unwrap();

            let found = resolve_cached_asset_path(test_url, &ctx);
            assert_eq!(found, Some(dummy_file.canonicalize().unwrap()));

            let _ = std::fs::remove_file(dummy_file);
            let _ = std::fs::remove_dir(ws_dir);
        });
    }

    #[test]
    fn focused_message_is_highlighted_and_scrolled_into_view() {
        run_gtk_test(|| {
            let ctx = test_context();
            let messages = (0..120)
                .rev()
                .map(|i| SlackMessage {
                    ts: format!("1700000{i:03}.000100"),
                    user: Some("U123".to_string()),
                    text: Some(format!("message {i}")),
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            let target = messages[100].ts.clone();
            let view = NativeTimelineView::new();
            let window = gtk::Window::new();
            window.set_default_size(400, 500);
            window.set_child(Some(view.widget()));
            window.present();
            view.set_messages(&messages, &ctx, Some(&target));
            pump(400);

            let highlighted = view
                .bound_items
                .borrow()
                .iter()
                .filter_map(glib::WeakRef::upgrade)
                .filter(|item| {
                    item.child()
                        .is_some_and(|child| child.has_css_class(FOCUS_TARGET_CLASS))
                })
                .filter_map(|item| item.item().and_downcast::<TimelineMessageObject>())
                .map(|obj| obj.message().ts)
                .collect::<Vec<_>>();
            assert_eq!(highlighted, vec![target.clone()]);

            let row = view
                .bound_items
                .borrow()
                .iter()
                .filter_map(glib::WeakRef::upgrade)
                .find(|item| {
                    item.item()
                        .and_downcast::<TimelineMessageObject>()
                        .is_some_and(|obj| !obj.is_day_separator() && obj.message().ts == target)
                })
                .and_then(|item| item.child())
                .expect("focused row should be bound");
            let bounds = row
                .compute_bounds(&view.list_view)
                .expect("focused row should be laid out");
            let page = view.scrolled_window.vadjustment().page_size();
            let row_center = f64::from(bounds.y() + bounds.height() / 2.0);
            assert!(
                (row_center - page / 2.0).abs() <= f64::from(bounds.height()),
                "row center {row_center} should sit mid-viewport (page {page})"
            );
            window.destroy();
        });
    }

    #[test]
    fn recheck_read_redispatches_visible_rows_and_pauses_while_inactive() {
        run_gtk_test(|| {
            let ctx = test_context();
            let messages = (0..3)
                .rev()
                .map(|i| SlackMessage {
                    ts: format!("1700000{i:03}.000100"),
                    user: Some("U123".to_string()),
                    text: Some(format!("message {i}")),
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            let marks: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
            let view = NativeTimelineView::new();
            {
                let marks = marks.clone();
                view.set_on_action(move |action| {
                    if let TimelineAction::AutoMarkRead { ts, .. } = action {
                        marks.borrow_mut().push(ts);
                    }
                });
            }
            let window = gtk::Window::new();
            window.set_default_size(400, 500);
            window.set_child(Some(view.widget()));
            window.present();
            view.set_read_mark_target("C1", None);
            view.set_messages(&messages, &ctx, None);
            pump(500);
            let newest = messages[0].ts.clone();
            assert_eq!(*marks.borrow(), vec![newest.clone()]);

            // Same rows, no scroll: only an explicit recheck marks again.
            view.recheck_read();
            pump(400);
            assert_eq!(marks.borrow().len(), 2);

            // An unfocused window never marks, even when rechecked.
            view.set_window_active(false);
            view.recheck_read();
            pump(400);
            assert_eq!(marks.borrow().len(), 2);

            // Regaining focus restarts the dwell for the visible rows.
            view.set_window_active(true);
            pump(400);
            assert_eq!(
                *marks.borrow(),
                vec![newest.clone(), newest.clone(), newest]
            );
            window.destroy();
        });
    }

    #[test]
    fn repeated_rechecks_within_the_dwell_do_not_starve_the_mark() {
        run_gtk_test(|| {
            let ctx = test_context();
            let messages = read_test_messages();
            let view = NativeTimelineView::new();
            let marks = record_auto_marks(&view);
            let window = gtk::Window::new();
            window.set_default_size(400, 500);
            window.set_child(Some(view.widget()));
            window.present();
            view.set_read_mark_target("C1", None);
            view.set_messages(&messages, &ctx, None);
            pump_until(5000, || !marks.borrow().is_empty());
            assert_eq!(marks.borrow().len(), 1);

            // Rechecks arriving faster than the dwell (e.g. a busy thread
            // catalog) must not keep restarting the pending dwell.
            for _ in 0..10 {
                view.recheck_read();
                pump(30);
            }
            assert!(
                marks.borrow().len() >= 2,
                "a dwell should complete while rechecks keep arriving"
            );
            window.destroy();
        });
    }

    #[test]
    fn hidden_stack_page_does_not_auto_mark_read() {
        run_gtk_test(|| {
            let ctx = test_context();
            let messages = read_test_messages();
            let view = NativeTimelineView::new();
            let marks = record_auto_marks(&view);
            let stack = gtk::Stack::new();
            stack.set_transition_type(gtk::StackTransitionType::None);
            stack.add_named(view.widget(), Some("timeline"));
            stack.add_named(&gtk::Label::new(Some("media")), Some("media"));
            let window = gtk::Window::new();
            window.set_default_size(400, 500);
            window.set_child(Some(&stack));
            window.present();
            view.set_read_mark_target("C1", None);
            view.set_messages(&messages, &ctx, None);
            pump_until(5000, || !marks.borrow().is_empty());
            assert_eq!(marks.borrow().len(), 1);

            // A dwell started while visible must not fire once hidden.
            view.recheck_read();
            stack.set_visible_child_name("media");
            pump(400);
            assert_eq!(marks.borrow().len(), 1);

            // Refocus while another page shows: still nothing to mark.
            view.set_window_active(false);
            view.set_window_active(true);
            view.recheck_read();
            pump(400);
            assert_eq!(marks.borrow().len(), 1);
            window.destroy();
        });
    }

    #[test]
    fn in_place_updates_keep_scroll_and_focus_on_clicked_row() {
        run_gtk_test(|| {
            let ctx = test_context();
            // Newest first, as conversations are stored; all on one day.
            let messages = (0..150)
                .rev()
                .map(|i| SlackMessage {
                    ts: format!("1700000{i:03}.000100"),
                    user: Some("U123".to_string()),
                    text: Some(format!("message {i}")),
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            let view = NativeTimelineView::new();
            let window = gtk::Window::new();
            window.set_default_size(400, 500);
            window.set_child(Some(view.widget()));
            present_and_wait_for_layout(&window);
            view.set_messages(&messages, &ctx, None);
            wait_until_at_bottom(&view);
            pump(300);

            let vadj = view.scrolled_window.vadjustment();
            let bottom = vadj.value();

            // Click-like focus on a visible row near the bottom.
            let focused = view
                .bound_items
                .borrow()
                .iter()
                .filter_map(glib::WeakRef::upgrade)
                .filter(|item| item.position() + 3 >= view.store.n_items())
                .find_map(|item| item.child()?.parent())
                .expect("visible row");
            assert!(focused.grab_focus());
            pump(100);
            let first_item = view.store.item(0);

            view.update_image_asset(&ctx, messages.iter().map(|m| m.ts.clone()).collect());
            pump(300);
            let focused_ts = messages[1].ts.clone();
            view.set_unread_separator(Some(focused_ts));
            pump(300);

            assert!(
                (vadj.value() - bottom).abs() < 1.0,
                "no jump: {} vs {bottom}",
                vadj.value()
            );
            assert!(view
                .list_view
                .state_flags()
                .contains(gtk::StateFlags::FOCUS_WITHIN));
            assert_eq!(view.store.item(0), first_item, "store items are kept");
            window.destroy();
        });
    }

    #[test]
    fn perf_guard_image_arrival_rebinds_only_rows_that_use_it() {
        run_gtk_test(|| {
            let ctx = test_context();
            let messages = (0..40)
                .rev()
                .map(|i| SlackMessage {
                    ts: format!("1700000{i:03}.000100"),
                    user: Some("U123".to_string()),
                    text: Some(format!("message {i}")),
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            let view = NativeTimelineView::new();
            let window = gtk::Window::new();
            window.set_default_size(400, 500);
            window.set_child(Some(view.widget()));
            window.present();
            view.set_messages(&messages, &ctx, None);
            pump(300);
            let before = bound_row_widgets(&view);
            let (first, second) = (before[0].0.clone(), before[1].0.clone());

            // Two arrivals in one idle coalesce into one rebind of both rows.
            view.update_image_asset(&ctx, HashSet::from([first.clone()]));
            view.update_image_asset(&ctx, HashSet::from([second.clone()]));
            pump(50);
            let mut rebound = rebound_rows(&view, &before);
            rebound.sort();
            let mut expected = vec![first, second];
            expected.sort();
            assert_eq!(rebound, expected);

            // An arrival nothing shows rebinds nothing.
            let before = bound_row_widgets(&view);
            view.update_image_asset(&ctx, HashSet::new());
            pump(50);
            assert_eq!(rebound_rows(&view, &before), Vec::<String>::new());
            window.destroy();
        });
    }

    #[test]
    fn perf_guard_re_render_rebinds_only_rows_whose_inputs_changed() {
        run_gtk_test(|| {
            let ctx = test_context();
            let messages = (0..40)
                .rev()
                .map(|i| SlackMessage {
                    ts: format!("1700000{i:03}.000100"),
                    user: Some("U123".to_string()),
                    text: Some(format!("message {i}")),
                    ..Default::default()
                })
                .collect::<Vec<_>>();
            let view = NativeTimelineView::new();
            let window = gtk::Window::new();
            window.set_default_size(400, 500);
            window.set_child(Some(view.widget()));
            window.present();
            view.set_messages(&messages, &ctx, None);
            pump(300);
            let before = bound_row_widgets(&view);
            assert!(!before.is_empty(), "rows are bound");

            view.set_messages(&messages, &ctx.clone(), None);
            pump(50);
            assert_eq!(rebound_rows(&view, &before), Vec::<String>::new());

            // Image, read mark and scroll intent changes have their own paths.
            let mut images = ctx.clone();
            images
                .image_assets
                .insert("https://example.com/a.png".into(), PathBuf::from("/a"));
            images
                .failed_image_urls
                .insert("https://example.com/b.png".into());
            images.last_read = Some(messages[5].ts.clone());
            images.timeline_scroll = crate::message_markup::TimelineScrollBehavior::Bottom;
            view.set_messages(&messages, &images, None);
            pump(50);
            assert_eq!(rebound_rows(&view, &before), Vec::<String>::new());

            // Moving the unread separator rebuilds only the row that gains it.
            let anchor = before[before.len() / 2].0.clone();
            let mut unread = images.clone();
            unread.unread_separator_ts = Some(anchor.clone());
            view.set_messages(&messages, &unread, None);
            pump(50);
            assert_eq!(rebound_rows(&view, &before), vec![anchor]);

            // A directory change reaches every row.
            let before = bound_row_widgets(&view);
            let mut renamed = unread.clone();
            renamed.user_names = Arc::new(HashMap::from([("U123".to_string(), "Bob".to_string())]));
            view.set_messages(&messages, &renamed, None);
            pump(50);
            assert_eq!(rebound_rows(&view, &before).len(), before.len());
            window.destroy();
        });
    }

    #[test]
    fn perf_guard_binding_timeline_rows_decodes_no_static_images_on_the_gtk_thread() {
        run_gtk_test(|| {
            let decodes = ManualDecodes::install();
            let assets = AssetCacheTestDir::new("decode-guard");
            let emoji_dir = TempTestDir::new("decode-guard-emoji");
            let emoji_path = emoji_dir.path().join("guard.png");
            save_test_image(&emoji_path, "png", 64, 64);
            let emoji_url = "https://example.com/decode-guard-emoji.png".to_string();
            crate::emoji_picker_window::invalidate_or_set_custom_emoji_cached_file(
                &emoji_url,
                emoji_path.clone(),
            );
            let mut ctx = test_context();
            ctx.custom_emojis = Arc::new(HashMap::from([("guard".to_string(), emoji_url)]));
            let messages = (0..60)
                .rev()
                .map(|i| {
                    let mut message = SlackMessage {
                        ts: format!("1700000{i:03}.000100"),
                        user: Some("U123".to_string()),
                        text: Some(format!("message {i} :guard: here")),
                        ..Default::default()
                    };
                    if i % 2 == 0 {
                        let url = format!("https://files.slack.com/files-pri/T1-F{i}/photo.png");
                        let path = assets.0.join(format!("photo-{i}.png"));
                        save_test_image(&path, "png", 40 + i, 30);
                        ctx.image_assets.insert(url.clone(), path);
                        message.files = Some(vec![SlackFile {
                            id: Some(format!("F{i}")),
                            mimetype: Some("image/png".to_string()),
                            url_private: Some(url),
                            ..Default::default()
                        }]);
                    }
                    message
                })
                .collect::<Vec<_>>();
            let view = NativeTimelineView::new();
            let window = gtk::Window::new();
            window.set_default_size(400, 500);
            window.set_child(Some(view.widget()));
            window.present();

            let before = crate::debug::IMAGE_DECODES.get();
            view.set_messages(&messages, &ctx, None);
            pump(300);
            assert_eq!(
                crate::debug::IMAGE_DECODES.get() - before,
                0,
                "binding rows must leave static decodes to the workers"
            );
            let running = decodes.0.running();
            assert!(
                (1..=decode::MAX_IN_FLIGHT).contains(&running.len()),
                "mapped pictures request decodes, a few at a time: {running:?}"
            );

            // Completion paints in place: no row is rebound.
            let rows = bound_row_widgets(&view);
            let finished = decodes.finish_all();
            pump(50);
            assert_eq!(rebound_rows(&view, &rows), Vec::<String>::new());
            let emoji_decodes = finished.iter().filter(|key| key.path == emoji_path).count();
            assert_eq!(emoji_decodes, 1, "one decode serves every inline emoji");
            let pictures: Vec<_> = rows
                .iter()
                .flat_map(|(_, row)| deferred_pictures(row))
                .filter(|(picture, _)| picture.is_mapped())
                .collect();
            assert!(!pictures.is_empty(), "rows show deferred pictures");
            for (_, deferred) in &pictures {
                assert!(
                    deferred.texture().is_some(),
                    "no visible image stays pending"
                );
            }
            window.destroy();
        });
    }

    #[test]
    fn deferred_picture_decodes_only_while_mapped() {
        run_gtk_test(|| {
            let decodes = ManualDecodes::install();
            let temp_dir = TempTestDir::new("deferred-mapped");
            let path = temp_dir.path().join("photo.png");
            save_test_image(&path, "png", 120, 80);

            let picture = decode::deferred_texture_picture(&path);
            let deferred = picture
                .paintable()
                .and_downcast::<decode::DeferredTexture>()
                .expect("deferred paintable");
            // Sized from the header before any decode, so nothing relayouts.
            assert_eq!(
                (deferred.intrinsic_width(), deferred.intrinsic_height()),
                (120, 80)
            );
            assert!(
                decodes.0.running().is_empty(),
                "unmapped: nothing requested"
            );

            let window = gtk::Window::new();
            window.set_child(Some(&picture));
            window.present();
            pump_until(500, || !decodes.0.running().is_empty());
            assert_eq!(decodes.0.running().len(), 1);

            // Unmapped before the decode lands: cached, not applied.
            window.set_visible(false);
            decodes.finish_all();
            assert!(deferred.texture().is_none());

            // Mapped again: served from the cache at once, no second decode.
            window.present();
            pump_until(500, || deferred.texture().is_some());
            assert!(deferred.texture().is_some());
            assert!(decodes.0.running().is_empty());
            assert_eq!(
                (deferred.intrinsic_width(), deferred.intrinsic_height()),
                (120, 80)
            );
            window.destroy();
        });
    }

    #[test]
    fn thread_footer_needs_an_open_thread_handler() {
        run_gtk_test(|| {
            let ctx = test_context();
            let msg = SlackMessage {
                ts: "1700000000.000950".to_string(),
                user: Some("U123".to_string()),
                text: Some("root".to_string()),
                reply_count: Some(1),
                ..Default::default()
            };
            let has_footer = |widget: Box| {
                find_descendant(&widget.upcast(), &|w| w.has_css_class("thread-reply-pill"))
                    .is_some()
            };
            let open_thread: Rc<dyn Fn(String)> = Rc::new(|_| {});
            assert!(has_footer(build_timeline_message_widget(
                &msg,
                &ctx,
                None,
                Some(&open_thread),
                None,
                None,
                None,
                true,
            )));
            assert!(!has_footer(build_timeline_message_widget(
                &msg, &ctx, None, None, None, None, None, true,
            )));
        });
    }

    #[test]
    fn message_text_links_dispatch_open_link() {
        run_gtk_test(|| {
            let ctx = test_context();
            let permalink = "https://acme.slack.com/archives/C1/p1700000002000200?thread_ts=1700000001.000100&cid=C1";
            let msg = SlackMessage {
                ts: "1700000000.000970".to_string(),
                user: Some("U123".to_string()),
                text: Some(format!(
                    "look\n<{}|{}>",
                    permalink.replace('&', "&amp;"),
                    "here"
                )),
                ..Default::default()
            };
            let received: Rc<RefCell<Vec<TimelineAction>>> = Rc::new(RefCell::new(Vec::new()));
            let sink = received.clone();
            let on_action: ActionHandler = Rc::new(move |action| sink.borrow_mut().push(action));
            let widget = build_timeline_message_widget(
                &msg,
                &ctx,
                None,
                None,
                None,
                None,
                Some(&on_action),
                true,
            );
            let label = find_descendant(&widget.upcast(), &|w| {
                w.downcast_ref::<Label>()
                    .is_some_and(|label| label.label().contains("<a href="))
            })
            .and_downcast::<Label>()
            .expect("message text label with a link");
            let handled: bool = label.emit_by_name("activate-link", &[&permalink]);
            assert!(handled);
            let actions = received.borrow();
            assert!(
                matches!(actions.as_slice(), [TimelineAction::OpenLink(uri)] if uri == permalink),
                "{actions:?}"
            );
        });
    }

    #[test]
    fn rich_message_links_and_quote_footer_dispatch_open_link() {
        run_gtk_test(|| {
            let ctx = test_context();
            let permalink = "https://acme.slack.com/archives/C1/p1700000002000200?thread_ts=1700000001.000100&cid=C1";
            let mut msg: SlackMessage = serde_json::from_value(serde_json::json!({
                "type": "message",
                "ts": "1700000000.000980",
                "user": "U123",
                "text": "see",
                "blocks": [{
                    "type": "rich_text",
                    "elements": [{
                        "type": "rich_text_section",
                        "elements": [
                            {"type": "text", "text": "see "},
                            {
                                "type": "message_mention",
                                "channel_id": "C1",
                                "message_ts": "1700000002.000200",
                                "thread_ts": "1700000001.000100",
                                "url": permalink
                            }
                        ]
                    }]
                }],
                "attachments": [{
                    "is_msg_unfurl": true,
                    "is_reply_unfurl": true,
                    "ts": "1700000002.000200",
                    "author_name": "Alice",
                    "channel_id": "C1",
                    "from_url": permalink,
                    "text": "quoted reply",
                    "footer": "Thread in Slack conversation"
                }]
            }))
            .expect("message deserializes");
            msg.refresh_canonical_content();

            let received: Rc<RefCell<Vec<TimelineAction>>> = Rc::new(RefCell::new(Vec::new()));
            let sink = received.clone();
            let on_action: ActionHandler = Rc::new(move |action| sink.borrow_mut().push(action));
            let widget = build_timeline_message_widget(
                &msg,
                &ctx,
                None,
                None,
                None,
                None,
                Some(&on_action),
                true,
            );

            let mut labels = Vec::new();
            collect_link_labels(&widget.upcast(), &mut labels);
            let hrefs = labels
                .iter()
                .map(|label| {
                    let markup = label.label();
                    let start = markup.find("href=\"").expect("link label has an href") + 6;
                    let end = start + markup[start..].find('"').expect("href is closed");
                    let href = markup[start..end].replace("&amp;", "&");
                    let handled: bool = label.emit_by_name("activate-link", &[&href]);
                    assert!(handled, "{href} should be handled in Conduit");
                    href
                })
                .collect::<Vec<_>>();
            assert!(hrefs
                .iter()
                .any(|href| href.starts_with("conduit://message?")));
            assert!(hrefs.iter().any(|href| href == permalink));
            let opened = received
                .borrow()
                .iter()
                .filter_map(|action| match action {
                    TimelineAction::OpenLink(uri) => Some(uri.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(opened, hrefs);
        });
    }

    #[test]
    fn file_card_download_dispatches_download_action() {
        run_gtk_test(|| {
            let ctx = test_context();
            let msg = SlackMessage {
                ts: "1700000000.000960".to_string(),
                user: Some("U123".to_string()),
                files: Some(vec![SlackFile {
                    name: Some("report.pdf".to_string()),
                    mimetype: Some("application/pdf".to_string()),
                    url_private_download: Some(
                        "https://files.slack.com/files-pri/T1-F1/download/report.pdf".to_string(),
                    ),
                    ..Default::default()
                }]),
                ..Default::default()
            };
            let received: Rc<RefCell<Option<TimelineAction>>> = Rc::new(RefCell::new(None));
            let sink = received.clone();
            let on_action: ActionHandler = Rc::new(move |action| {
                *sink.borrow_mut() = Some(action);
            });
            let widget = build_timeline_message_widget(
                &msg,
                &ctx,
                None,
                None,
                None,
                None,
                Some(&on_action),
                true,
            );
            let button = find_descendant(&widget.upcast(), &|w| {
                w.tooltip_text().as_deref() == Some("Download file")
            })
            .and_downcast::<Button>()
            .expect("download button");
            assert!(button.is_sensitive());
            button.emit_clicked();
            let action = received.borrow_mut().take();
            match action {
                Some(TimelineAction::DownloadFile { url, name }) => {
                    assert_eq!(
                        url,
                        "https://files.slack.com/files-pri/T1-F1/download/report.pdf"
                    );
                    assert_eq!(name, "report.pdf");
                }
                other => panic!("expected DownloadFile, got {other:?}"),
            }
        });
    }

    #[test]
    fn test_reaction_row_is_adw_wrap_box() {
        run_gtk_test(|| {
            let ctx = test_context();
            let msg = SlackMessage {
                ts: "1700000000.000900".to_string(),
                user: Some("U123".to_string()),
                text: Some("hi".to_string()),
                reactions: Some(vec![
                    SlackReaction {
                        name: Some("thumbsup".to_string()),
                        count: Some(2),
                        users: None,
                    },
                    SlackReaction {
                        name: Some("smile".to_string()),
                        count: Some(1),
                        users: None,
                    },
                ]),
                ..Default::default()
            };
            let widget =
                build_timeline_message_widget(&msg, &ctx, None, None, None, None, None, true);
            let row = find_descendant(&widget.clone().upcast(), &|w| {
                w.has_css_class("reaction-row")
            })
            .expect("reaction row");
            let wrap = row.downcast::<adw::WrapBox>().expect("adw::WrapBox");
            assert_eq!(wrap.child_spacing(), 4);
            let mut pills = 0;
            let mut child = wrap.first_child();
            while let Some(c) = child {
                assert!(c.is::<Button>());
                assert!(c.has_css_class("reaction-pill"));
                pills += 1;
                child = c.next_sibling();
            }
            assert_eq!(pills, 3);
        });
    }

    #[test]
    fn test_create_message_text_widget_with_custom_emoji() {
        run_gtk_test(|| {
            let context = test_context();
            let pango = "Hi <b><conduit-custom-emoji name=\"heart-sparkle\" url=\"https://example.com/heart.gif\"/></b> there";
            let widget = create_message_text_widget(pango, &context, None);
            let view = widget.downcast::<TextView>().ok();
            assert!(view.is_some(), "expected TextView for custom emoji markup");
        });
    }

    #[test]
    fn test_create_message_text_widget_with_user_mention() {
        run_gtk_test(|| {
            let context = test_context();
            let pango = "<a href=\"conduit-user://U123\"><span background=\"#D6ECFF\" foreground=\"#1264A3\"> @Alice </span></a>";
            let widget = create_message_text_widget(pango, &context, None);
            let label = widget.downcast::<Label>().ok();
            assert!(
                label.is_some(),
                "expected Label for standard mention markup"
            );
        });
    }

    #[test]
    fn clear_timeline_view_caches_resets_thread_local_caches() {
        run_gtk_test(|| {
            set_media_collapsed("1700000000.000100", "file:0", true);
            assert!(is_media_collapsed("1700000000.000100", "file:0"));

            let pixbuf =
                gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, 8, 8).unwrap();
            let texture = gtk::gdk::Texture::for_pixbuf(&pixbuf);
            let path = PathBuf::from("/tmp/conduit-cache-test.png");
            TEXTURE_CACHE.with(|cache| cache.borrow_mut().insert(path.clone(), texture.clone()));
            REACTION_TEXTURE_CACHE.with(|cache| cache.borrow_mut().insert(path.clone(), texture));

            clear_timeline_view_caches();

            assert!(!is_media_collapsed("1700000000.000100", "file:0"));
            assert!(TEXTURE_CACHE.with(|cache| cache.borrow_mut().get(&path).is_none()));
            assert!(REACTION_TEXTURE_CACHE.with(|cache| cache.borrow_mut().get(&path).is_none()));
        });
    }

    #[test]
    fn test_build_store_items_grouped_headers() {
        run_gtk_test(|| {
            let msg1 = SlackMessage {
                ts: "1700000000.000100".to_string(),
                user: Some("U123".to_string()),
                text: Some("msg 1".to_string()),
                ..Default::default()
            };
            let msg2 = SlackMessage {
                ts: "1700000010.000100".to_string(),
                user: Some("U123".to_string()),
                text: Some("msg 2".to_string()),
                ..Default::default()
            };
            let msg3 = SlackMessage {
                ts: "1700000020.000100".to_string(),
                user: Some("U123".to_string()),
                text: Some("msg 3".to_string()),
                ..Default::default()
            };

            // Messages passed to timeline are newest-first (Slack API order).
            // Without unread separator: msg1 has header, msg2 and msg3 hide header
            let (items, _) =
                build_store_items(&[msg3.clone(), msg2.clone(), msg1.clone()], None, None);
            let msg_objs: Vec<TimelineMessageObject> = items
                .into_iter()
                .filter_map(|item| item.downcast::<TimelineMessageObject>().ok())
                .filter(|obj| !obj.is_day_separator())
                .collect();
            assert_eq!(msg_objs.len(), 3);
            assert_eq!(msg_objs[0].message().ts, "1700000000.000100");
            assert!(!msg_objs[0].hide_header());
            assert_eq!(msg_objs[1].message().ts, "1700000010.000100");
            assert!(msg_objs[1].hide_header());
            assert_eq!(msg_objs[2].message().ts, "1700000020.000100");
            assert!(msg_objs[2].hide_header());

            // With unread separator on msg2: msg2 must NOT hide header
            let (items, _) = build_store_items(
                &[msg3.clone(), msg2.clone(), msg1.clone()],
                None,
                Some("1700000010.000100"),
            );
            let msg_objs: Vec<TimelineMessageObject> = items
                .into_iter()
                .filter_map(|item| item.downcast::<TimelineMessageObject>().ok())
                .filter(|obj| !obj.is_day_separator())
                .collect();
            assert_eq!(msg_objs.len(), 3);
            assert!(!msg_objs[0].hide_header());
            assert!(!msg_objs[1].hide_header());
            assert!(msg_objs[2].hide_header());
        });
    }

    #[test]
    fn test_build_timeline_message_widget_grouped_hides_header() {
        run_gtk_test(|| {
            let ctx = test_context();
            let msg = SlackMessage {
                ts: "1700000000.000100".to_string(),
                user: Some("U123".to_string()),
                text: Some("grouped message body".to_string()),
                ..Default::default()
            };

            // show_header = true
            let normal_widget =
                build_timeline_message_widget(&msg, &ctx, None, None, None, None, None, true);
            assert!(!normal_widget.has_css_class("timeline-grouped-message"));
            assert!(descendants(&normal_widget.clone().upcast())
                .iter()
                .any(|w| w.has_css_class("timeline-avatar")));

            // show_header = false
            let grouped_widget =
                build_timeline_message_widget(&msg, &ctx, None, None, None, None, None, false);
            assert!(grouped_widget.has_css_class("timeline-grouped-message"));
            assert!(!descendants(&grouped_widget.clone().upcast())
                .iter()
                .any(|w| w.has_css_class("timeline-avatar")));
        });
    }

    #[test]
    fn test_resolve_reaction_emoji_widget_unicode() {
        run_gtk_test(|| {
            let context = test_context();
            let catalog = crate::emoji::EmojiCatalog::new(&context.custom_emojis);
            let widget = resolve_reaction_emoji_widget("thumbsup", "+1", &context, &catalog);
            let label = widget.downcast::<Label>().ok();
            assert!(label.is_some(), "expected Label for unicode reaction");
        });
    }

    #[test]
    fn test_resolve_reaction_emoji_widget_uncached_custom() {
        run_gtk_test(|| {
            let mut context = test_context();
            let mut emojis = HashMap::new();
            emojis.insert(
                "custom_cat".to_string(),
                "https://example.com/cat.png".to_string(),
            );
            context.custom_emojis = Arc::new(emojis);
            let catalog = crate::emoji::EmojiCatalog::new(&context.custom_emojis);
            let widget =
                resolve_reaction_emoji_widget("custom_cat", ":custom_cat:", &context, &catalog);
            let bin = widget.downcast::<adw::Bin>().ok();
            assert!(
                bin.is_some(),
                "expected adw::Bin placeholder container for uncached custom emoji"
            );
        });
    }

    #[test]
    fn test_reaction_and_quick_bar_images_sized_to_32px() {
        run_gtk_test(|| {
            assert_eq!(REACTION_EMOJI_SIZE, 32);

            // 1. Pixbuf scaling scales to 32px max
            let large_pixbuf =
                gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, 128, 64).unwrap();
            let scaled = scale_reaction_pixbuf(&large_pixbuf);
            assert_eq!(scaled.width(), 32);
            assert_eq!(scaled.height(), 16);

            // 2. Custom emoji picture sizing and paintable bounds
            let temp_dir = TempTestDir::new("reaction-test");
            let img_path = temp_dir.path().join("custom.png");
            gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, 96, 96)
                .expect("pixbuf")
                .savev(&img_path, "png", &[])
                .expect("save");

            let widget = load_custom_emoji_picture(&img_path);
            let pic = widget.downcast::<Picture>().expect("expected Picture");
            assert_eq!(pic.width_request(), 32);
            assert_eq!(pic.height_request(), 32);
            assert!(pic.can_shrink());
            let paintable = pic.paintable().expect("expected paintable");
            assert_eq!(paintable.intrinsic_width(), 32);
            assert_eq!(paintable.intrinsic_height(), 32);

            // 3. Add-reaction pill icon: themed symbolic icon, emoji-height box
            let add_widget = load_add_reaction_widget();
            let add_icon = add_widget.downcast::<Image>().expect("expected Image");
            assert_eq!(add_icon.icon_name().as_deref(), Some(ADD_REACTION_ICON));
            assert_eq!(add_icon.pixel_size(), ADD_REACTION_ICON_SIZE);
            assert_eq!(add_icon.height_request(), 32);

            // 4. Quick bar button sizing
            let quick_bar = Box::new(Orientation::Horizontal, 2);
            let on_action: ActionSlot = Rc::new(RefCell::new(None));
            let hover_gen = Rc::new(Cell::new(0));
            let hovered = Rc::new(RefCell::new(None));
            let popover = Rc::new(RefCell::new(None));
            let motion = gtk::EventControllerMotion::new();
            let state = crate::timeline::quick_actions::QuickBarState {
                on_action: &on_action,
                hover_generation: &hover_gen,
                hovered: &hovered,
                current_popover: &popover,
                bar_motion: &motion,
            };

            let mut context = test_context();
            let img_url = "https://example.com/custom.png".to_string();
            crate::emoji_picker_window::invalidate_or_set_custom_emoji_cached_file(
                &img_url,
                img_path.clone(),
            );
            let mut emojis = HashMap::new();
            emojis.insert("test_custom".to_string(), img_url.clone());
            context.custom_emojis = Arc::new(emojis);

            let reactions = vec![crate::emoji::EmojiEntry {
                name: "test_custom".to_string(),
                label: "test_custom".to_string(),
                category: "Workspace",
                value: crate::emoji::EmojiValue::CustomImage(img_url),
            }];

            crate::timeline::quick_actions::rebuild_quick_bar(
                &quick_bar,
                "1700000000.000100",
                &reactions,
                &context,
                &state,
            );

            let btn = quick_bar
                .first_child()
                .and_then(|w| w.downcast::<Button>().ok())
                .expect("expected reaction button");
            let btn_child = btn.child().expect("button child");
            let btn_pic = btn_child.downcast::<Picture>().expect("picture child");
            assert_eq!(btn_pic.width_request(), 32);
            assert_eq!(btn_pic.height_request(), 32);
            assert!(btn_pic.can_shrink());

            // Quick bar add-reaction uses the same emoji-sized icon as the pills.
            let add_btn = quick_bar
                .observe_children()
                .into_iter()
                .filter_map(|w| w.ok()?.downcast::<Button>().ok())
                .find(|b| b.tooltip_text().as_deref() == Some("Add reaction..."))
                .expect("add reaction button");
            let add_child = add_btn.child().expect("button child");
            let add_child = add_child.downcast::<Image>().expect("expected Image");
            assert_eq!(add_child.icon_name().as_deref(), Some(ADD_REACTION_ICON));
            assert_eq!(add_child.pixel_size(), ADD_REACTION_ICON_SIZE);
        });
    }

    #[test]
    fn test_small_png_and_jpeg_reaction_emoji_decode_in_process_at_32px_layout() {
        run_gtk_test(|| {
            let temp_dir = TempTestDir::new("reaction-in-process");
            for (name, format) in [("wide.png", "png"), ("wide.jpg", "jpeg")] {
                let path = temp_dir.path().join(name);
                save_test_image(&path, format, 96, 48);
                let widget = load_custom_emoji_picture(&path);
                let pic = widget.downcast::<Picture>().expect("expected Picture");
                let paintable = pic.paintable().expect("expected paintable");
                // GTK scales at render; layout still sees the 32 px fit.
                assert_eq!(cached_reaction_size(&path), (96, 48), "{name}");
                assert_eq!(paintable.intrinsic_width(), 32, "{name}");
                assert_eq!(paintable.intrinsic_height(), 16, "{name}");
                assert_eq!(pic.width_request(), 32);
                assert_eq!(pic.height_request(), 32);
            }
        });
    }

    #[test]
    fn test_large_png_reaction_emoji_keeps_the_pixbuf_scaled_path() {
        run_gtk_test(|| {
            let temp_dir = TempTestDir::new("reaction-pixbuf");
            let path = temp_dir.path().join("big.png");
            save_test_image(&path, "png", 256, 128);
            let widget = load_custom_emoji_picture(&path);
            let pic = widget.downcast::<Picture>().expect("expected Picture");
            let paintable = pic.paintable().expect("expected paintable");
            assert_eq!(cached_reaction_size(&path), (32, 16));
            assert_eq!(paintable.intrinsic_width(), 32);
            assert_eq!(paintable.intrinsic_height(), 16);
        });
    }

    #[test]
    fn perf_guard_reaction_emoji_decodes_once_per_file() {
        run_gtk_test(|| {
            let temp_dir = TempTestDir::new("reaction-once");
            let path = temp_dir.path().join("once.png");
            save_test_image(&path, "png", 64, 64);
            let before = crate::debug::IMAGE_DECODES.get();
            for _ in 0..40 {
                load_custom_emoji_picture(&path);
            }
            assert_eq!(crate::debug::IMAGE_DECODES.get() - before, 1);
        });
    }

    #[test]
    fn test_build_code_block_widget_single_line_no_lang() {
        run_gtk_test(|| {
            let widget = build_code_block_widget("let x = 42;", None);
            let container = widget
                .clone()
                .downcast::<Box>()
                .expect("expected Box container");
            assert!(container.has_css_class("code-block"));
            assert!(container.has_css_class("monospace"));
            // Single-line code without lang should have no header row.
            let header = find_descendant(&widget, &|w| w.has_css_class("code-block-header"));
            assert!(
                header.is_none(),
                "single-line code with no lang should not have a header"
            );
        });
    }

    #[test]
    fn test_build_code_block_widget_multiline_with_lang() {
        run_gtk_test(|| {
            let code = "fn main() {\n    println!(\"hello\");\n}";
            let widget = build_code_block_widget(code, Some("rs"));
            let container = widget
                .clone()
                .downcast::<Box>()
                .expect("expected Box container");
            assert!(container.has_css_class("code-block"));
            assert!(container.has_css_class("monospace"));
            // Multi-line code with lang should have a header row with badge and copy button.
            let header = find_descendant(&widget, &|w| w.has_css_class("code-block-header"));
            assert!(
                header.is_some(),
                "multiline code with lang should have a header"
            );
            let copy_btn = find_descendant(&widget, &|w| w.has_css_class("code-block-copy"));
            assert!(copy_btn.is_some(), "header should include a copy button");
        });
    }

    #[test]
    fn test_render_text_content_with_code_block() {
        run_gtk_test(|| {
            let target = Box::new(Orientation::Vertical, 0);
            let context = test_context();
            let text = "Check this code:\n```rs\nlet a = 1;\nlet b = 2;\n```\nDone.";
            render_text_content(text, &target, &context, None);
            let code_block = find_descendant(&target.upcast::<Widget>(), &|w| {
                w.has_css_class("code-block")
            });
            assert!(code_block.is_some(), "expected rendered code block widget");
        });
    }

    #[test]
    fn avatar_never_loads_untrusted_url_as_local_path() {
        run_gtk_test(|| {
            // A real, loadable PNG on disk: the old fallback would have opened it
            // straight from the Slack-provided `icon_url`.
            let temp_dir = TempTestDir::new("avatar-local-path");
            let png = temp_dir.path().join("avatar.png");
            gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, 4, 4)
                .unwrap()
                .savev(&png, "png", &[])
                .unwrap();

            for url in [png.to_str().unwrap(), "/dev/zero"] {
                let message = SlackMessage {
                    ts: "1700000000.000100".to_string(),
                    icons: Some(crate::models::SlackIcons {
                        image_72: Some(url.to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                assert_eq!(message.avatar_url(), Some(url));
                let avatar = build_avatar_widget(&message, &test_context())
                    .downcast::<adw::Avatar>()
                    .unwrap();
                assert!(avatar.custom_image().is_none(), "{url}");
            }
        });
    }

    #[test]
    fn scrolling_near_top_requests_older_page_and_prepend_keeps_position() {
        run_gtk_test(|| {
            // Newest first, as conversations are stored; all on one day.
            fn page(range: std::ops::Range<u32>) -> Vec<SlackMessage> {
                range
                    .rev()
                    .map(|i| SlackMessage {
                        ts: format!("1700000{i:03}.000100"),
                        user: Some("U123".to_string()),
                        text: Some(format!("message {i}")),
                        ..Default::default()
                    })
                    .collect()
            }
            let ctx = test_context();
            let requests: Rc<RefCell<Vec<String>>> = Rc::default();
            let view = NativeTimelineView::new();
            {
                let requests = requests.clone();
                view.set_on_action(move |action| {
                    if let TimelineAction::LoadOlder {
                        channel_id,
                        thread_ts,
                    } = action
                    {
                        requests
                            .borrow_mut()
                            .push(format!("{channel_id}:{thread_ts:?}"));
                    }
                });
            }
            let window = gtk::Window::new();
            window.set_default_size(400, 500);
            window.set_child(Some(view.widget()));
            present_and_wait_for_layout(&window);
            view.set_read_mark_target("C1", None);
            view.set_messages(&page(100..200), &ctx, None);
            wait_until_at_bottom(&view);
            // Past the 150ms settle, whose paging check must find the bottom.
            pump(300);
            assert!(requests.borrow().is_empty(), "opens at the newest message");

            let vadj = view.scrolled_window.vadjustment();
            vadj.set_value(vadj.lower() + vadj.page_size() / 4.0);
            pump_until(2000, || !requests.borrow().is_empty());
            assert_eq!(*requests.borrow(), vec!["C1:None".to_string()]);

            // While the page is in flight, further scrolling does not re-request.
            view.set_loading_older(true);
            vadj.set_value(vadj.value() + 1.0);
            pump(100);
            assert_eq!(requests.borrow().len(), 1);

            // GtkListView keeps its anchor row when rows land above it, so the
            // offset grows by exactly the prepended height.
            let (value_before, upper_before) = (vadj.value(), vadj.upper());
            let mut prepend = ctx.clone();
            prepend.timeline_scroll =
                crate::message_markup::TimelineScrollBehavior::PreservePrepend;
            view.set_messages(&page(0..200), &prepend, None);
            view.finish_loading_older("C1", None);
            pump_until(2000, || vadj.upper() > upper_before);
            // Past the settle, whose paging check must see the anchored offset.
            pump(300);

            let grown = vadj.upper() - upper_before;
            assert!(grown > 0.0, "older rows were prepended");
            assert!(
                (vadj.value() - value_before - grown).abs() < 2.0,
                "position moved: {value_before} -> {} after growing {grown}",
                vadj.value()
            );
            assert_eq!(
                requests.borrow().len(),
                1,
                "a full page above the anchor is not near the top"
            );
            window.destroy();
        });
    }

    #[test]
    fn failed_older_page_backs_off_instead_of_rerequesting() {
        run_gtk_test(|| {
            let messages: Vec<SlackMessage> = (0..200u32)
                .rev()
                .map(|i| SlackMessage {
                    ts: format!("1700000{i:03}.000100"),
                    user: Some("U123".to_string()),
                    text: Some(format!("message {i}")),
                    ..Default::default()
                })
                .collect();
            let ctx = test_context();
            let requests = Rc::new(Cell::new(0u32));
            let view = NativeTimelineView::new();
            {
                let requests = requests.clone();
                let handle = view.clone();
                view.set_on_action(move |action| {
                    if let TimelineAction::LoadOlder { .. } = action {
                        requests.set(requests.get() + 1);
                        // As the window does: spinner up while the page loads.
                        handle.set_loading_older(true);
                    }
                });
            }
            let window = gtk::Window::new();
            window.set_default_size(400, 500);
            window.set_child(Some(view.widget()));
            present_and_wait_for_layout(&window);
            view.set_read_mark_target("C1", None);
            view.set_messages(&messages, &ctx, None);
            wait_until_at_bottom(&view);
            // Past the 150ms settle, whose paging check must find the bottom.
            pump(300);
            assert_eq!(requests.get(), 0, "opens at the newest message");

            let vadj = view.scrolled_window.vadjustment();
            vadj.set_value(vadj.lower() + vadj.page_size() / 4.0);
            pump_until(2000, || requests.get() > 0);
            assert_eq!(requests.get(), 1);

            // The page fails while the user still sits near the top: no retry
            // on the idle re-check, nor on further scrolling during cooldown.
            view.fail_loading_older("C1", None);
            pump(200);
            vadj.set_value(vadj.value() + 1.0);
            pump(200);
            assert_eq!(requests.get(), 1, "a failed page is not retried at once");

            // A failure for another surface leaves paging alone.
            view.fail_loading_older("C2", None);

            // Switching away and back clears the cooldown.
            view.set_read_mark_target("C2", None);
            view.set_read_mark_target("C1", None);
            vadj.set_value(vadj.value() - 1.0);
            pump_until(2000, || requests.get() > 1);
            assert_eq!(requests.get(), 2, "a fresh surface pages again");
            window.destroy();
        });
    }
}
