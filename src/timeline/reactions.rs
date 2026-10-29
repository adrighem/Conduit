use std::path::{Path, PathBuf};

use adw::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;
use gtk::{glib, Box, Button, Image, Label, Orientation, Picture, Widget};

use super::actions::ToggleReactionCallback;
use super::cache::{resolve_cached_asset_path, REACTION_TEXTURE_CACHE};
use super::row::register_timeline_css;
use crate::message_markup::MessageRenderContext;
use crate::models::SlackMessage;

pub const REACTION_EMOJI_SIZE: i32 = 32;

/// Themed symbolic icon (gresource) for every "Add reaction..." button.
pub const ADD_REACTION_ICON: &str = "reaction-add-symbolic";
/// Glyph size matching the visual weight of a 32px emoji.
pub const ADD_REACTION_ICON_SIZE: i32 = 24;

/// Wrapping row for chips and pills: natural-width children, 4px gaps.
pub fn new_chip_wrap_box() -> adw::WrapBox {
    adw::WrapBox::builder()
        .child_spacing(4)
        .line_spacing(4)
        .build()
}

/// Returns `(width, height)` scaled so the longest side equals `max`.
pub fn fit_within(width: i32, height: i32, max: i32) -> (i32, i32) {
    let longest = width.max(height).max(1);
    let scale = |side: i32| {
        ((side as i64 * max as i64 + longest as i64 / 2) / longest as i64).max(1) as i32
    };
    (scale(width), scale(height))
}

/// Scales `pixbuf` so its longest side is `REACTION_EMOJI_SIZE`, keeping the
/// aspect ratio so non-square custom emoji are not stretched.
pub fn scale_reaction_pixbuf(pixbuf: &gdk_pixbuf::Pixbuf) -> gdk_pixbuf::Pixbuf {
    let (width, height) = fit_within(pixbuf.width(), pixbuf.height(), REACTION_EMOJI_SIZE);
    pixbuf
        .scale_simple(width, height, gdk_pixbuf::InterpType::Bilinear)
        .unwrap_or_else(|| pixbuf.clone())
}

/// Longest side up to which a PNG or JPEG reaction emoji is decoded by GTK in
/// process and scaled at render. Slack custom emoji are at most 128 px; larger
/// files take the pixbuf path, which caches only its 32 px result.
const IN_PROCESS_EMOJI_MAX_SIDE: u32 = 128;

mod imp {
    use std::cell::OnceCell;

    use gtk::gdk;
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    use super::{fit_within, REACTION_EMOJI_SIZE};

    /// A texture that reports the `REACTION_EMOJI_SIZE` box it fits in as its
    /// intrinsic size, so layout matches a pre-scaled emoji while GTK does
    /// the scaling when it snapshots.
    #[derive(Default)]
    pub struct FittedTexture {
        pub(super) texture: OnceCell<gdk::Texture>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FittedTexture {
        const NAME: &'static str = "ConduitFittedTexture";
        type Type = super::FittedTexture;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for FittedTexture {}

    impl FittedTexture {
        fn fitted_size(&self) -> (i32, i32) {
            self.texture.get().map_or((0, 0), |texture| {
                fit_within(texture.width(), texture.height(), REACTION_EMOJI_SIZE)
            })
        }
    }

    impl PaintableImpl for FittedTexture {
        fn flags(&self) -> gdk::PaintableFlags {
            gdk::PaintableFlags::SIZE | gdk::PaintableFlags::CONTENTS
        }

        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            if let Some(texture) = self.texture.get() {
                texture.snapshot(snapshot, width, height);
            }
        }

        fn intrinsic_width(&self) -> i32 {
            self.fitted_size().0
        }

        fn intrinsic_height(&self) -> i32 {
            self.fitted_size().1
        }
    }
}

glib::wrapper! {
    pub struct FittedTexture(ObjectSubclass<imp::FittedTexture>)
        @implements gtk::gdk::Paintable;
}

/// `texture` as a paintable whose intrinsic size is its `REACTION_EMOJI_SIZE`
/// fit. Textures that already have that size are used as they are.
fn fit_reaction_texture(texture: gtk::gdk::Texture) -> gtk::gdk::Paintable {
    let size = fit_within(texture.width(), texture.height(), REACTION_EMOJI_SIZE);
    if size == (texture.width(), texture.height()) {
        return texture.upcast();
    }
    let fitted: FittedTexture = glib::Object::new();
    let _ = fitted.imp().texture.set(texture);
    fitted.upcast()
}

/// Decodes the emoji at `path`. Small PNG and JPEG go through GTK's own
/// decoders, at the file's size; everything else through gdk-pixbuf (glycin),
/// scaled to `REACTION_EMOJI_SIZE` at once.
fn decode_reaction_texture(path: &Path) -> Option<gtk::gdk::Texture> {
    use crate::image_header::ImageFormat;
    let in_process = matches!(
        crate::image_header::declared_header(path),
        Some((ImageFormat::Png | ImageFormat::Jpeg, width, height))
            if width.max(height) <= IN_PROCESS_EMOJI_MAX_SIDE
    );
    if in_process {
        return crate::debug::trace_image_decode(
            "texture",
            path,
            crate::debug::ImageLoader::Texture,
            || gtk::gdk::Texture::from_filename(path),
        )
        .ok();
    }
    let scaled = crate::debug::trace_image_decode(
        "pixbuf",
        path,
        crate::debug::ImageLoader::Pixbuf,
        || {
            let pixbuf = gdk_pixbuf::Pixbuf::from_file(path).ok()?;
            Some(scale_reaction_pixbuf(&pixbuf))
        },
    )?;
    Some(gtk::gdk::Texture::for_pixbuf(&scaled))
}

pub fn load_custom_emoji_picture(path: &Path) -> Widget {
    let paintable =
        crate::animated_image::animated_paintable(path, REACTION_EMOJI_SIZE).or_else(|| {
            REACTION_TEXTURE_CACHE.with(|cache| {
                let mut map = cache.borrow_mut();
                if let Some(texture) = map.get(path) {
                    return Some(fit_reaction_texture(texture));
                }
                if !crate::animated_image::within_decode_budget(path) {
                    return None;
                }
                let texture = decode_reaction_texture(path)?;
                map.insert(path.to_path_buf(), texture.clone());
                Some(fit_reaction_texture(texture))
            })
        });
    let pic = match paintable {
        Some(paintable) => Picture::for_paintable(&paintable),
        // Failed or over the decode budget: decoding again would not help.
        None => Picture::new(),
    };
    pic.set_size_request(32, 32);
    pic.set_can_shrink(true);
    pic.set_content_fit(gtk::ContentFit::Contain);
    pic.add_css_class("reaction-emoji-image");
    pic.upcast::<Widget>()
}

/// Add-reaction icon for the quick bar and reaction pills: a symbolic icon
/// sized to sit with the emoji next to it, boxed to the emoji height.
pub fn load_add_reaction_widget() -> Widget {
    let icon = Image::from_icon_name(ADD_REACTION_ICON);
    icon.set_pixel_size(ADD_REACTION_ICON_SIZE);
    icon.set_size_request(REACTION_EMOJI_SIZE, REACTION_EMOJI_SIZE);
    icon.upcast::<Widget>()
}

pub fn resolve_reaction_emoji_widget(
    clean_name: &str,
    raw_name: &str,
    context: &MessageRenderContext,
    emoji_catalog: &crate::emoji::EmojiCatalog,
) -> Widget {
    match emoji_catalog
        .resolve(clean_name)
        .or_else(|| emoji_catalog.resolve(raw_name))
    {
        Some(crate::emoji::EmojiValue::Unicode(ch)) => {
            let lbl = Label::new(Some(ch));
            lbl.add_css_class("reaction-emoji-unicode");
            lbl.upcast()
        }
        Some(crate::emoji::EmojiValue::CustomImage(ref url)) => {
            if let Some(path) = crate::emoji_picker_window::resolve_custom_emoji_cached_file(url)
                .or_else(|| resolve_cached_asset_path(url, context))
            {
                load_custom_emoji_picture(&path)
            } else {
                let bin = adw::Bin::new();
                bin.set_size_request(32, 32);
                bin.set_halign(gtk::Align::Center);
                bin.set_valign(gtk::Align::Center);
                let icon = Image::from_icon_name("image-missing-symbolic");
                icon.set_pixel_size(32);
                icon.add_css_class("reaction-emoji-image");
                bin.set_child(Some(&icon));

                let bin_weak = bin.downgrade();
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
                crate::emoji_picker_window::request_custom_emoji_file(url, move |path| {
                    let _ = tx.send(path);
                });
                glib::spawn_future_local(async move {
                    let Some(path) = rx.recv().await else {
                        return;
                    };
                    if let Some(bin) = bin_weak.upgrade() {
                        let pic = load_custom_emoji_picture(&path);
                        bin.set_child(Some(&pic));
                    }
                });

                bin.upcast()
            }
        }
        None => {
            if let Some(unicode) = emojis::get(clean_name).or_else(|| emojis::get(raw_name)) {
                let lbl = Label::new(Some(unicode.as_str()));
                lbl.add_css_class("reaction-emoji-unicode");
                lbl.upcast()
            } else if !clean_name.is_ascii() {
                let lbl = Label::new(Some(clean_name));
                lbl.add_css_class("reaction-emoji-unicode");
                lbl.upcast()
            } else {
                let icon = Image::from_icon_name("image-missing-symbolic");
                icon.set_pixel_size(32);
                icon.add_css_class("reaction-emoji-image");
                icon.upcast()
            }
        }
    }
}

pub fn build_reactions_widget(
    message: &SlackMessage,
    context: &MessageRenderContext,
    on_toggle_reaction: Option<&ToggleReactionCallback>,
) -> Option<adw::WrapBox> {
    let reactions = message.reactions.as_deref().filter(|r| !r.is_empty())?;
    register_timeline_css();
    let wrap_box = new_chip_wrap_box();
    wrap_box.add_css_class("reaction-row");

    let emoji_catalog = crate::emoji::EmojiCatalog::new(&context.custom_emojis);

    for r in reactions {
        let raw_name = r.name.as_deref().unwrap_or("reaction");
        let count = r.count.unwrap_or(1);
        let clean_name = raw_name.trim_matches(':');

        let emoji_widget =
            resolve_reaction_emoji_widget(clean_name, raw_name, context, &emoji_catalog);

        let has_self = if let Some(ref my_id) = context.current_user_id {
            r.users.as_ref().is_some_and(|users| users.contains(my_id))
        } else {
            false
        };

        let box_widget = Box::new(Orientation::Horizontal, 3);
        box_widget.append(&emoji_widget);

        let label = Label::new(Some(&count.to_string()));
        box_widget.append(&label);

        let pill_button = Button::new();
        pill_button.set_focus_on_click(false);
        pill_button.set_child(Some(&box_widget));
        pill_button.add_css_class("reaction-pill");
        pill_button.add_css_class("flat");
        if has_self {
            pill_button.add_css_class("reaction-pill-active");
        }

        if let Some(cb) = on_toggle_reaction {
            let cb = cb.clone();
            let message_ts = message.ts.clone();
            let clean_name_str = clean_name.to_string();
            pill_button.connect_clicked(move |_| {
                cb(message_ts.clone(), clean_name_str.clone(), !has_self);
            });
        }

        let tooltip_text = if let Some(users) = &r.users {
            let reactor_names: Vec<&str> = users
                .iter()
                .filter_map(|user_id| context.user_names.get(user_id).map(|s| s.as_str()))
                .collect();
            if !reactor_names.is_empty() {
                format!(":{clean_name}: reacted by {}", reactor_names.join(", "))
            } else {
                format!(":{clean_name}:")
            }
        } else {
            format!(":{clean_name}:")
        };
        pill_button.set_tooltip_text(Some(&tooltip_text));

        pill_button.set_halign(gtk::Align::Start);
        pill_button.set_valign(gtk::Align::Center);
        wrap_box.append(&pill_button);
    }

    let add_pic = load_add_reaction_widget();
    let add_btn = Button::new();
    add_btn.set_focus_on_click(false);
    add_btn.set_child(Some(&add_pic));
    add_btn.add_css_class("reaction-pill");
    add_btn.add_css_class("flat");
    add_btn.set_tooltip_text(Some("Add reaction..."));
    add_btn.set_halign(gtk::Align::Start);
    add_btn.set_valign(gtk::Align::Center);

    if let Some(cb) = on_toggle_reaction {
        let weak_add_btn = add_btn.downgrade();
        let custom_emojis = context.custom_emojis.clone();
        let message_ts = message.ts.clone();
        let cb = cb.clone();

        add_btn.connect_clicked(move |_| {
            let Some(btn) = weak_add_btn.upgrade() else {
                return;
            };
            let cb = cb.clone();
            let message_ts = message_ts.clone();
            let picker = crate::emoji_picker_window::EmojiPickerWindow::new(
                &btn,
                &custom_emojis,
                move |selected_name| {
                    let clean = selected_name.trim().trim_matches(':');
                    if !clean.is_empty() {
                        cb(message_ts.clone(), clean.to_string(), true);
                    }
                },
            );
            picker.present();
        });
    }

    wrap_box.append(&add_btn);

    Some(wrap_box)
}
