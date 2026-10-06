//! Native image widgets for message documents: Block Kit image blocks,
//! unfurl images and the small inline icons of context blocks.
//!
//! Downloads are driven elsewhere (the window requests every
//! `MessageDocument::image_urls` entry and rebuilds rows once an asset lands),
//! so these builders only map "cached file present / failed / pending" to a
//! picture, an alt-text line or a sized placeholder.

use std::path::{Path, PathBuf};

use adw::prelude::*;
use gtk::{Box, Image, Label, Orientation, Widget};

use crate::rich_message::MessageImage;
use crate::timeline_message_widget::{load_animated_or_static_picture, wrap_collapsible_media};

/// Largest box an inline image may occupy; mirrors Slack's timeline cap.
pub(crate) const MAX_IMAGE_WIDTH: i32 = 480;
pub(crate) const MAX_IMAGE_HEIGHT: i32 = 360;
/// Reserved size while neither the asset nor Slack's declared size is known.
const PLACEHOLDER_SIZE: (i32, i32) = (240, 160);
const CONTEXT_ICON_SIZE: i32 = 16;

/// Maps an image URL to its downloaded file, if the asset cache has it.
pub(crate) type AssetResolver<'a> = &'a dyn Fn(&str) -> Option<PathBuf>;

/// What the renderer needs to know about one image URL right now.
pub(crate) struct MediaSources<'a> {
    pub(crate) resolve: AssetResolver<'a>,
    pub(crate) is_failed: &'a dyn Fn(&str) -> bool,
}

/// Scales `width`x`height` into the timeline media box, keeping the aspect
/// ratio and never upscaling.
pub(crate) fn fit_image_size(width: u32, height: u32) -> (i32, i32) {
    if width == 0 || height == 0 {
        return PLACEHOLDER_SIZE;
    }
    let (width, height) = (f64::from(width), f64::from(height));
    let scale = (f64::from(MAX_IMAGE_WIDTH) / width)
        .min(f64::from(MAX_IMAGE_HEIGHT) / height)
        .min(1.0);
    (
        ((width * scale).round() as i32).max(1),
        ((height * scale).round() as i32).max(1),
    )
}

fn declared_size(image: &MessageImage) -> Option<(u32, u32)> {
    Some((image.width?, image.height?))
}

fn file_size(path: &Path) -> Option<(u32, u32)> {
    let (_, width, height) = gdk_pixbuf::Pixbuf::file_info(path)?;
    Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?))
}

fn loadable_url(image: &MessageImage) -> Option<&str> {
    image.url.as_deref().filter(|url| {
        url::Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
    })
}

/// A full-width message image: optional "Title ▾" collapse row, then the
/// picture (animated when the asset is a GIF), a sized placeholder while it
/// downloads, or an alt-text line when it cannot be shown.
pub(crate) fn image_block(
    image: &MessageImage,
    sources: &MediaSources<'_>,
    ts: &str,
    slot: &str,
) -> Widget {
    let media = match loadable_url(image) {
        Some(url) => match (sources.resolve)(url) {
            Some(path) => picture(image, &path),
            None if (sources.is_failed)(url) => alt_text(image),
            None => placeholder(image),
        },
        None => alt_text(image),
    };
    media.set_tooltip_text(Some(&image.alt));
    let title = image
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty());
    wrap_collapsible_media(media, ts, slot, title).upcast()
}

fn picture(image: &MessageImage, path: &Path) -> Widget {
    let (width, height) = file_size(path)
        .or_else(|| declared_size(image))
        .map(|(width, height)| fit_image_size(width, height))
        .unwrap_or(PLACEHOLDER_SIZE);
    let picture = load_animated_or_static_picture(path, width, height, gtk::ContentFit::Contain);
    picture.add_css_class("timeline-media-image");
    picture.set_overflow(gtk::Overflow::Hidden);
    picture.set_halign(gtk::Align::Start);
    // The clamp keeps oversized assets at the fitted width; the picture's
    // height-for-width then follows its aspect ratio.
    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(width);
    clamp.set_tightening_threshold(width);
    clamp.set_halign(gtk::Align::Start);
    clamp.set_child(Some(&picture));
    clamp.upcast()
}

fn placeholder(image: &MessageImage) -> Widget {
    let (width, height) = declared_size(image)
        .map(|(width, height)| fit_image_size(width, height))
        .unwrap_or(PLACEHOLDER_SIZE);
    let frame = Box::new(Orientation::Vertical, 0);
    frame.add_css_class("timeline-media-placeholder");
    frame.set_size_request(width, height);
    frame.set_halign(gtk::Align::Start);
    let icon = Image::from_icon_name("image-x-generic-symbolic");
    icon.set_pixel_size(32);
    icon.add_css_class("dim-label");
    icon.set_vexpand(true);
    icon.set_valign(gtk::Align::Center);
    frame.append(&icon);
    frame.upcast()
}

fn alt_text(image: &MessageImage) -> Widget {
    let label = Label::new(Some(&format!("Image: {}", image.alt)));
    label.add_css_class("dim-label");
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.upcast()
}

/// The 16px inline icon of a context block (e.g. the giphy logo). Pending or
/// failed assets keep the slot with a neutral symbolic icon.
pub(crate) fn context_icon(image: &MessageImage, sources: &MediaSources<'_>) -> Widget {
    let path = loadable_url(image).and_then(|url| (sources.resolve)(url));
    let widget = match path {
        Some(path) => load_animated_or_static_picture(
            &path,
            CONTEXT_ICON_SIZE,
            CONTEXT_ICON_SIZE,
            gtk::ContentFit::Contain,
        ),
        None => {
            let icon = Image::from_icon_name("image-x-generic-symbolic");
            icon.set_pixel_size(CONTEXT_ICON_SIZE);
            icon.add_css_class("dim-label");
            icon.upcast()
        }
    };
    widget.set_valign(gtk::Align::Center);
    widget.set_tooltip_text(Some(&image.alt));
    widget
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_small_images_at_natural_size() {
        assert_eq!(fit_image_size(200, 150), (200, 150));
    }

    #[test]
    fn fit_caps_wide_images_by_width() {
        assert_eq!(fit_image_size(960, 540), (480, 270));
    }

    #[test]
    fn fit_caps_tall_images_by_height() {
        assert_eq!(fit_image_size(400, 720), (200, 360));
    }

    #[test]
    fn fit_falls_back_to_placeholder_for_degenerate_sizes() {
        assert_eq!(fit_image_size(0, 100), PLACEHOLDER_SIZE);
    }
}
