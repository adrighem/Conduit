//! Visual tests: render real widgets offscreen and measure the drawn pixels.
//!
//! Size assertions on `pixel_size` or `size_request` cannot catch an icon
//! whose artwork leaves half its canvas empty, or one stretched to fill its
//! box. These tests rasterize the widget and compare ink bounding boxes, so
//! they check what the user actually sees. Each render is also saved as a PNG
//! under `target/visual-tests/` for eyeballing.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib, graphene, Box, Orientation, Widget};

use super::actions::ActionSlot;
use super::quick_actions::{rebuild_quick_bar, QuickBarState};
use super::tests::{run_gtk_test, test_context, TempTestDir};

/// Alpha above which a pixel counts as drawn.
const INK_ALPHA: u8 = 64;

/// Ink bounding box of one widget, in pixels.
#[derive(Debug, Clone, Copy)]
struct Ink {
    width: u32,
    height: u32,
}

/// Straight-alpha RGBA pixels of a rendered widget.
struct Raster {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Raster {
    /// Ink bounding box, or `None` when nothing is drawn.
    fn ink(&self) -> Option<Ink> {
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0, 0);
        for y in 0..self.height {
            for x in 0..self.width {
                let alpha = self.rgba[((y * self.width + x) * 4 + 3) as usize];
                if alpha > INK_ALPHA {
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
        }
        (min_x != u32::MAX).then(|| Ink {
            width: max_x - min_x + 1,
            height: max_y - min_y + 1,
        })
    }
}

/// Registers the source tree's icons, which the app loads from its gresource.
fn register_source_icons() {
    let display = gdk::Display::default().expect("display");
    let theme = gtk::IconTheme::for_display(&display);
    let icons = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/icons");
    theme.add_search_path(icons);
}

/// Puts `content` in a window and spins the main loop until it is laid out.
fn show_in_window(content: &impl IsA<Widget>) -> gtk::Window {
    let window = gtk::Window::new();
    window.set_child(Some(content));
    window.present();
    let deadline = Instant::now() + Duration::from_secs(5);
    let context = glib::MainContext::default();
    while !(content.is_mapped() && content.width() > 0) {
        assert!(Instant::now() < deadline, "widget was never laid out");
        context.iteration(false);
    }
    // Let icon and texture loads settle before rendering.
    for _ in 0..20 {
        context.iteration(false);
    }
    window
}

/// Rasterizes `widget` with the window's renderer, saving it as `name`.png
/// when a name is given.
fn render(window: &gtk::Window, widget: &impl IsA<Widget>, name: Option<&str>) -> Raster {
    let (width, height) = (widget.width(), widget.height());
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    let node = snapshot.to_node().expect("widget drew nothing");
    let renderer = window.renderer().expect("renderer");
    let bounds = graphene::Rect::new(0.0, 0.0, width as f32, height as f32);
    let texture = renderer.render_texture(&node, Some(&bounds));

    if let Some(name) = name {
        let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/visual-tests");
        std::fs::create_dir_all(&out_dir).expect("create visual test output dir");
        texture
            .save_to_png(out_dir.join(format!("{name}.png")))
            .expect("save render");
    }

    let mut downloader = gdk::TextureDownloader::new(&texture);
    downloader.set_format(gdk::MemoryFormat::R8g8b8a8);
    let (bytes, stride) = downloader.download_bytes();
    let row = width as usize * 4;
    let rgba = bytes
        .chunks(stride)
        .flat_map(|line| line[..row].iter().copied())
        .collect();
    Raster {
        width: width as u32,
        height: height as u32,
        rgba,
    }
}

/// Ink box of each quick bar button's content, keyed by tooltip. Content is
/// rendered alone so button chrome (hover, global CSS) never counts as ink.
fn ink_by_tooltip(window: &gtk::Window, bar: &Box) -> HashMap<String, Ink> {
    let mut inks = HashMap::new();
    let mut child = bar.first_child();
    while let Some(widget) = child {
        if let (Some(tooltip), Some(content)) = (widget.tooltip_text(), widget.first_child()) {
            let raster = render(window, &content, None);
            let ink = raster
                .ink()
                .unwrap_or_else(|| panic!("{tooltip} button drew nothing"));
            inks.insert(tooltip.to_string(), ink);
        }
        child = widget.next_sibling();
    }
    inks
}

/// Runs `f` on the GTK test thread. Plain `cargo test` skips it without a
/// display; under `CONDUIT_REQUIRE_DISPLAY` (meson's Xvfb run) a skip fails.
fn run_visual_test<F: FnOnce() + Send + 'static>(f: F) {
    let ran = Arc::new(AtomicBool::new(false));
    let ran_inner = ran.clone();
    run_gtk_test(move || {
        f();
        ran_inner.store(true, Ordering::SeqCst);
    });
    if std::env::var_os("CONDUIT_REQUIRE_DISPLAY").is_some() {
        assert!(ran.load(Ordering::SeqCst), "GTK could not open a display");
    }
}

#[test]
fn quick_bar_add_reaction_sits_between_emoji_and_action_icons() {
    run_visual_test(|| {
        register_source_icons();
        // Same stylesheet as the app, so results do not depend on test order.
        super::row::register_timeline_css();

        // A fully opaque custom emoji gives a font-independent 32px reference.
        let temp_dir = TempTestDir::new("visual-quick-bar");
        let img_path = temp_dir.path().join("solid.png");
        let pixbuf =
            gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 64, 64).expect("pixbuf");
        pixbuf.fill(0x3366ccff);
        pixbuf.savev(&img_path, "png", &[]).expect("save");
        let img_url = "https://example.com/visual-solid.png".to_string();
        crate::emoji_picker_window::invalidate_or_set_custom_emoji_cached_file(
            &img_url,
            img_path.clone(),
        );
        let mut context = test_context();
        context.custom_emojis = Arc::new(HashMap::from([(
            "visual_solid".to_string(),
            img_url.clone(),
        )]));
        let reactions = vec![crate::emoji::EmojiEntry {
            name: "visual_solid".to_string(),
            label: "visual_solid".to_string(),
            category: "Workspace",
            value: crate::emoji::EmojiValue::CustomImage(img_url),
        }];

        let quick_bar = Box::new(Orientation::Horizontal, 2);
        quick_bar.add_css_class("card");
        let on_action: ActionSlot = Rc::new(RefCell::new(None));
        let state = QuickBarState {
            on_action: &on_action,
            hover_generation: &Rc::new(Cell::new(0)),
            hovered: &Rc::new(RefCell::new(None)),
            current_popover: &Rc::new(RefCell::new(None)),
            bar_motion: &gtk::EventControllerMotion::new(),
        };
        rebuild_quick_bar(
            &quick_bar,
            "1700000000.000100",
            &reactions,
            &context,
            &state,
        );

        let window = show_in_window(&quick_bar);
        render(&window, &quick_bar, Some("quick-bar"));
        let inks = ink_by_tooltip(&window, &quick_bar);
        window.destroy();

        let emoji = inks[":visual_solid:"];
        let add = inks["Add reaction..."];
        let reply = inks["Reply in thread"];
        let emoji_side = emoji.width.max(emoji.height) as f32;
        let add_side = add.width.max(add.height) as f32;
        let reply_side = reply.width.max(reply.height) as f32;

        assert!(
            emoji_side >= 32.0,
            "reference emoji should be at least 32px: {emoji:?}"
        );
        assert!(
            (0.6..=0.85).contains(&(add_side / emoji_side)),
            "add-reaction glyph {add:?} should be 60-85% of emoji {emoji:?}"
        );
        assert!(
            add_side > reply_side,
            "add-reaction glyph {add:?} should read larger than action icon {reply:?}"
        );
    });
}
