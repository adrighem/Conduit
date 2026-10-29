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
use std::sync::Arc;

use gtk::prelude::*;
use gtk::{gdk, graphene, Box, Orientation, Widget};

use super::actions::ActionSlot;
use super::quick_actions::{rebuild_quick_bar, QuickBarState};
use super::tests::{pump, pump_until, run_gtk_test, test_context, TempTestDir};
use super::text::{create_message_text_widget, render_text_content};
use crate::message_markup::MessageRenderContext;

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
    let laid_out = || content.is_mapped() && content.width() > 0;
    pump_until(5000, laid_out);
    assert!(laid_out(), "widget was never laid out");
    // Let icon and texture loads settle before rendering. Static images
    // decode on worker threads, so wait for those to land.
    pump(50);
    pump_until(5000, || !has_pending_decode(content.upcast_ref()));
    window
}

/// Whether a picture under `widget` still waits on an off-thread decode.
fn has_pending_decode(widget: &Widget) -> bool {
    find_widget(widget, &|candidate| {
        candidate
            .downcast_ref::<gtk::Picture>()
            .and_then(|picture| picture.paintable())
            .and_downcast::<crate::timeline::cache::decode::DeferredPaintable>()
            .is_some_and(|deferred| deferred.content().is_none())
    })
    .is_some()
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

/// Context with `:visual_solid:`, a fully opaque custom emoji whose ink box
/// is exactly its rendered size: a font-independent size reference.
fn solid_emoji_context(temp_dir: &TempTestDir) -> (MessageRenderContext, String) {
    let img_path = temp_dir.path().join("solid.png");
    let pixbuf =
        gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 64, 64).expect("pixbuf");
    pixbuf.fill(0x3366ccff);
    pixbuf.savev(&img_path, "png", &[]).expect("save");
    let img_url = "https://example.com/visual-solid.png".to_string();
    crate::emoji_picker_window::invalidate_or_set_custom_emoji_cached_file(&img_url, img_path);
    let mut context = test_context();
    context.custom_emojis = Arc::new(HashMap::from([(
        "visual_solid".to_string(),
        img_url.clone(),
    )]));
    (context, img_url)
}

/// First descendant of `widget` (itself included) matching `predicate`.
fn find_widget(widget: &Widget, predicate: &dyn Fn(&Widget) -> bool) -> Option<Widget> {
    if predicate(widget) {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(found) = find_widget(&current, predicate) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

/// Whether a unicode emoji glyph's ink side is within 10% (at least 2px) of
/// the custom emoji `size`; glyph artwork varies per emoji.
fn near_emoji_size(ink: u32, size: u32) -> bool {
    ink.abs_diff(size) <= (size / 10).max(2)
}

/// Larger side of the ink drawn by `widget` rendered alone.
fn ink_side(window: &gtk::Window, widget: &impl IsA<Widget>) -> u32 {
    let ink = render(window, widget, None)
        .ink()
        .expect("widget drew nothing");
    ink.width.max(ink.height)
}

/// Tests that create GTK widgets. They run only when
/// `CONDUIT_GRAPHICAL_TESTS` is set (see `run_gtk_test`).
mod graphical {
    use super::*;

    #[test]
    fn quick_bar_emoji_render_at_20px_with_add_reaction_at_action_icon_size() {
        run_gtk_test(|| {
            register_source_icons();
            // Same stylesheet as the app, so results do not depend on test order.
            crate::timeline::row::register_timeline_css();

            let temp_dir = TempTestDir::new("visual-quick-bar");
            let (context, img_url) = solid_emoji_context(&temp_dir);
            let reactions = vec![
                crate::emoji::EmojiEntry {
                    name: "visual_solid".to_string(),
                    label: "visual_solid".to_string(),
                    category: "Workspace",
                    value: crate::emoji::EmojiValue::CustomImage(img_url),
                },
                crate::emoji::EmojiEntry {
                    name: "white_check_mark".to_string(),
                    label: "white check mark".to_string(),
                    category: "Symbols",
                    value: crate::emoji::EmojiValue::Unicode("✅"),
                },
            ];

            let quick_bar = Box::new(Orientation::Horizontal, 2);
            quick_bar.add_css_class("card");
            let on_action: ActionSlot = Rc::new(RefCell::new(None));
            let state = QuickBarState {
                on_action: &on_action,
                hover_generation: &Rc::new(Cell::new(0)),
                hovered: &Rc::new(RefCell::new(None)),
                current_popover: &Rc::new(RefCell::new(None)),
                bar_motion: &gtk::EventControllerMotion::new(),
                can_mark_unread: true,
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
            let bar_height = quick_bar.height();
            window.destroy();

            let side = |name: &str| {
                let ink = inks[name];
                ink.width.max(ink.height)
            };
            let size = crate::timeline::reactions::QUICK_BAR_EMOJI_SIZE as u32;
            assert_eq!(side(":visual_solid:"), size, "custom quick bar emoji");
            assert!(
                near_emoji_size(side(":white_check_mark:"), size),
                "unicode quick bar emoji {} should match custom ({size}px)",
                side(":white_check_mark:")
            );
            let add = side("Add reaction...");
            let reply = side("Reply in thread");
            assert!(
                add >= reply && add < size,
                "add-reaction glyph {add} should match action icons ({reply}) and stay below emoji ({size})"
            );
            // The emoji no longer set the bar height; the icon buttons do.
            assert!(bar_height <= 40, "quick bar is {bar_height}px tall");
        });
    }

    #[test]
    fn message_emoji_render_inline_at_22px_and_jumbo_at_reaction_size() {
        run_gtk_test(|| {
            crate::timeline::row::register_timeline_css();
            let temp_dir = TempTestDir::new("visual-message-emoji");
            let (context, _) = solid_emoji_context(&temp_dir);
            let is_picture = |w: &Widget| w.is::<gtk::Picture>();

            // Inline: emoji next to text, custom and unicode at the same size.
            let inline = Box::new(Orientation::Vertical, 0);
            render_text_content("hi :visual_solid: there", &inline, &context, None);
            let unicode = Box::new(Orientation::Vertical, 0);
            let pango = crate::message_markup::mrkdwn_to_pango(":smile:", &context);
            unicode.append(&create_message_text_widget(&pango, &context, None));
            // Emoji-only: rendered with the reaction widgets.
            let jumbo = Box::new(Orientation::Vertical, 0);
            render_text_content(":visual_solid: :smile:", &jumbo, &context, None);

            let page = Box::new(Orientation::Vertical, 8);
            page.set_size_request(320, -1);
            page.append(&inline);
            page.append(&unicode);
            page.append(&jumbo);
            let window = show_in_window(&page);
            render(&window, &page, Some("message-emoji"));

            let inline_custom =
                find_widget(inline.upcast_ref(), &is_picture).expect("inline picture");
            let inline_custom = ink_side(&window, &inline_custom);
            let inline_unicode = ink_side(&window, &unicode);
            let jumbo_custom = find_widget(jumbo.upcast_ref(), &is_picture).expect("jumbo picture");
            let jumbo_custom = ink_side(&window, &jumbo_custom);
            let jumbo_unicode = find_widget(jumbo.upcast_ref(), &|w| w.is::<gtk::Label>())
                .expect("jumbo unicode label");
            let jumbo_unicode = ink_side(&window, &jumbo_unicode);
            window.destroy();

            let inline_size = crate::message_markup::INLINE_EMOJI_SIZE as u32;
            let jumbo_size = crate::timeline::reactions::REACTION_EMOJI_SIZE as u32;
            assert_eq!(inline_custom, inline_size, "inline custom emoji");
            assert!(
                jumbo_custom >= jumbo_size,
                "jumbo custom emoji {jumbo_custom} should match reactions ({jumbo_size}px)"
            );
            assert!(
                near_emoji_size(inline_unicode, inline_custom),
                "inline unicode {inline_unicode} should match inline custom {inline_custom}"
            );
            assert!(
                near_emoji_size(jumbo_unicode, jumbo_custom),
                "jumbo unicode {jumbo_unicode} should match jumbo custom {jumbo_custom}"
            );
        });
    }
}
