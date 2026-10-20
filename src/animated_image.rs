//! Animated image (GIF, animated WebP) support shared by the timeline,
//! reactions and the emoji picker.
//!
//! Every file is decoded once per display size into an [`AnimatedImage`]
//! paintable that all pictures showing it share. A single timer per paintable
//! advances frames at the image's own delay and stops once no widget holds the
//! paintable, so N copies of a GIF cost one decoder, one texture per frame and
//! no per-widget frame-clock ticks.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// GIFs often declare 0-10 ms delays; browsers play those at 100 ms.
const MIN_FRAME_DELAY: Duration = Duration::from_millis(20);
const DEFAULT_FRAME_DELAY: Duration = Duration::from_millis(100);
/// How often a paused animation checks whether a window regained focus.
const PAUSED_POLL: Duration = Duration::from_millis(500);
/// Frames are scaled for this display scale so they stay sharp on HiDPI.
const OVERSAMPLE: i32 = 2;
const MAX_STATIC_FILES: usize = 4096;

thread_local! {
    static PLAYERS: RefCell<HashMap<(PathBuf, i32), glib::WeakRef<AnimatedImage>>> =
        RefCell::new(HashMap::new());
    /// GIF/WebP files known to hold a single frame, so they are not decoded
    /// again on every row build.
    static STATIC_FILES: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct AnimatedImage {
        pub(super) iter: RefCell<Option<gdk_pixbuf::PixbufAnimationIter>>,
        pub(super) frame: RefCell<Option<gdk::Texture>>,
        pub(super) max_side: Cell<i32>,
        pub(super) timer: RefCell<Option<glib::SourceId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AnimatedImage {
        const NAME: &'static str = "ConduitAnimatedImage";
        type Type = super::AnimatedImage;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for AnimatedImage {
        fn dispose(&self) {
            if let Some(timer) = self.timer.take() {
                timer.remove();
            }
        }
    }

    impl PaintableImpl for AnimatedImage {
        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            if let Some(frame) = self.frame.borrow().as_ref() {
                frame.snapshot(snapshot, width, height);
            }
        }

        fn intrinsic_width(&self) -> i32 {
            self.frame
                .borrow()
                .as_ref()
                .map_or(0, |frame| frame.width())
        }

        fn intrinsic_height(&self) -> i32 {
            self.frame
                .borrow()
                .as_ref()
                .map_or(0, |frame| frame.height())
        }
    }
}

glib::wrapper! {
    pub struct AnimatedImage(ObjectSubclass<imp::AnimatedImage>)
        @implements gdk::Paintable;
}

impl AnimatedImage {
    fn new(iter: gdk_pixbuf::PixbufAnimationIter, max_side: i32) -> Self {
        let player: Self = glib::Object::new();
        player.imp().max_side.set(max_side);
        player.imp().iter.replace(Some(iter));
        player.show_current_frame();
        player.schedule_next_frame();
        player
    }

    fn show_current_frame(&self) {
        let imp = self.imp();
        let Some(pixbuf) = imp.iter.borrow().as_ref().map(|iter| iter.pixbuf()) else {
            return;
        };
        let pixbuf = scale_to_fit(&pixbuf, imp.max_side.get());
        imp.frame.replace(Some(gdk::Texture::for_pixbuf(&pixbuf)));
        self.invalidate_contents();
    }

    fn schedule_next_frame(&self) {
        // No delay: the last frame of a non-looping animation stays up.
        let Some(delay) = self
            .imp()
            .iter
            .borrow()
            .as_ref()
            .and_then(|iter| iter.delay_time())
        else {
            return;
        };
        let delay = if delay < MIN_FRAME_DELAY {
            DEFAULT_FRAME_DELAY
        } else {
            delay
        };
        self.schedule(delay);
    }

    fn schedule(&self, delay: Duration) {
        let weak = self.downgrade();
        let timer = glib::timeout_add_local_once(delay, move || {
            let Some(player) = weak.upgrade() else {
                return;
            };
            // This source has fired; dispose must not remove it again.
            player.imp().timer.take();
            player.advance();
        });
        self.imp().timer.replace(Some(timer));
    }

    fn advance(&self) {
        if !any_window_active() {
            self.schedule(PAUSED_POLL);
            return;
        }
        let changed = self
            .imp()
            .iter
            .borrow()
            .as_ref()
            .is_some_and(|iter| iter.advance(SystemTime::now()));
        if changed {
            self.show_current_frame();
        }
        self.schedule_next_frame();
    }
}

/// The shared paintable playing the animation at `path`, sized for a widget of
/// at most `display_size` logical pixels per side. `None` when the file is not
/// animated or cannot be decoded; callers then show it as a static image.
pub(crate) fn animated_paintable(path: &Path, display_size: i32) -> Option<gdk::Paintable> {
    let key = (path.to_path_buf(), display_size);
    let existing = PLAYERS.with(|players| players.borrow().get(&key).and_then(|w| w.upgrade()));
    if let Some(player) = existing {
        return Some(player.upcast());
    }
    if STATIC_FILES.with(|files| files.borrow().contains(path)) || !may_be_animated(path) {
        return None;
    }
    let animation = gdk_pixbuf::PixbufAnimation::from_file(path).ok()?;
    if animation.is_static_image() {
        STATIC_FILES.with(|files| {
            let mut files = files.borrow_mut();
            if files.len() >= MAX_STATIC_FILES {
                files.clear();
            }
            files.insert(path.to_path_buf());
        });
        return None;
    }
    let player = AnimatedImage::new(
        animation.iter(Some(SystemTime::now())),
        display_size.saturating_mul(OVERSAMPLE),
    );
    PLAYERS.with(|players| {
        let mut players = players.borrow_mut();
        players.retain(|_, weak| weak.upgrade().is_some());
        players.insert(key, player.downgrade());
    });
    Some(player.upcast())
}

/// Forgets which files were found to be static, e.g. on workspace sign-out.
pub(crate) fn clear_caches() {
    STATIC_FILES.with(|files| files.borrow_mut().clear());
}

/// Animations only advance while one of the app's windows has focus.
fn any_window_active() -> bool {
    gtk::Window::list_toplevels()
        .iter()
        .filter_map(|widget| widget.downcast_ref::<gtk::Window>())
        .any(|window| window.is_active())
}

/// Scales `pixbuf` down so its longest side is at most `max_side`.
fn scale_to_fit(pixbuf: &gdk_pixbuf::Pixbuf, max_side: i32) -> gdk_pixbuf::Pixbuf {
    let (width, height) = (pixbuf.width(), pixbuf.height());
    let longest = width.max(height);
    if max_side <= 0 || longest <= max_side {
        return pixbuf.clone();
    }
    let scale = |side: i32| ((side as i64 * max_side as i64) / longest as i64).max(1) as i32;
    pixbuf
        .scale_simple(
            scale(width),
            scale(height),
            gdk_pixbuf::InterpType::Bilinear,
        )
        .unwrap_or_else(|| pixbuf.clone())
}

/// Whether `path` is in a format that can hold several frames. Reads only the
/// header, so static images skip a full animation decode.
pub(crate) fn may_be_animated(path: &Path) -> bool {
    gdk_pixbuf::Pixbuf::file_info(path)
        .and_then(|(format, _, _)| format.name())
        .is_some_and(|name| matches!(name.as_str(), "gif" | "webp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1x1 GIF89a.
    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;";

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("conduit-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).expect("write temp image");
        path
    }

    #[test]
    fn gif_may_be_animated() {
        let path = write_temp("anim.gif", GIF);
        assert!(may_be_animated(&path));
        std::fs::remove_file(path).ok();
    }

    /// 1x1 GIF89a with two 100 ms frames.
    const ANIMATED_GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff\
        !\xf9\x04\x00\x0a\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00\
        !\xf9\x04\x00\x0a\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02L\x01\x00;";

    #[test]
    fn animated_gif_shares_one_player_per_display_size() {
        let path = write_temp("shared.gif", ANIMATED_GIF);
        let first = animated_paintable(&path, 18).expect("animated paintable");
        let second = animated_paintable(&path, 18).expect("animated paintable");
        let other_size = animated_paintable(&path, 32).expect("animated paintable");
        assert_eq!(first, second);
        assert_ne!(first, other_size);
        assert_eq!(first.intrinsic_width(), 1);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn single_frame_gif_has_no_player() {
        let path = write_temp("still.gif", GIF);
        assert!(animated_paintable(&path, 18).is_none());
        assert!(STATIC_FILES.with(|files| files.borrow().contains(&path)));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn scale_to_fit_only_shrinks() {
        let pixbuf = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, 400, 200)
            .expect("allocate pixbuf");
        let scaled = scale_to_fit(&pixbuf, 100);
        assert_eq!((scaled.width(), scaled.height()), (100, 50));
        let kept = scale_to_fit(&pixbuf, 800);
        assert_eq!((kept.width(), kept.height()), (400, 200));
    }

    #[test]
    fn png_and_unreadable_files_are_static() {
        let pixbuf = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, 1, 1)
            .expect("allocate pixbuf");
        let png = pixbuf.save_to_bufferv("png", &[]).expect("encode png");
        let png_path = write_temp("still.png", &png);
        let junk_path = write_temp("junk.bin", b"not an image");
        assert!(!may_be_animated(&png_path));
        assert!(!may_be_animated(&junk_path));
        std::fs::remove_file(png_path).ok();
        std::fs::remove_file(junk_path).ok();
    }
}
