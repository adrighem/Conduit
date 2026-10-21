use std::path::{Path, PathBuf};

use adw::prelude::*;
use gtk::{glib, Box, Button, Image, Label, Orientation, Picture, Widget};

use super::actions::ToggleReactionCallback;
use super::cache::{resolve_cached_asset_path, REACTION_TEXTURE_CACHE};
use super::row::register_timeline_css;
use crate::message_markup::MessageRenderContext;
use crate::models::SlackMessage;

pub const REACTION_EMOJI_SIZE: i32 = 32;

pub const ADD_REACTION_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" width="32" height="32" fill="currentColor"><path fill="currentColor" d="M10.28 4.117a7 7 0 1 0 5.604 5.604A2.5 2.5 0 0 1 13 7.25v-.251h-.251a2.5 2.5 0 0 1-2.47-2.883Z"/><path fill="currentColor" fill-rule="evenodd" d="M15.5 1a.75.75 0 0 1 .75.75v2h2a.75.75 0 0 1 0 1.5h-2v2a.75.75 0 0 1-1.5 0v-2h-2a.75.75 0 0 1 0-1.5h2v-2A.75.75 0 0 1 15.5 1Zm-13 10a6.5 6.5 0 0 1 7.166-6.466.75.75 0 0 0 .152-1.493 8 8 0 1 0 7.14 7.139.75.75 0 0 0-1.492.152A6.525 6.525 0 0 1 15.5 11a6.5 6.5 0 1 1-13 0Zm4.25-.5a1.25 1.25 0 1 0 0-2.5 1.25 1.25 0 0 0 0 2.5Zm4.5 0a1.25 1.25 0 1 0 0-2.5 1.25 1.25 0 0 0 0 2.5ZM9 15c1.277 0 2.553-.724 3.06-2.173.148-.426-.209-.827-.66-.827H6.6c-.452 0-.808.4-.66.827C6.448 14.276 7.724 15 9 15Z" clip-rule="evenodd"/></svg>"#;

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

pub fn load_custom_emoji_picture(path: &Path) -> Widget {
    let paintable =
        crate::animated_image::animated_paintable(path, REACTION_EMOJI_SIZE).or_else(|| {
            REACTION_TEXTURE_CACHE.with(|cache| {
                let mut map = cache.borrow_mut();
                if let Some(texture) = map.get(path) {
                    return Some(texture.upcast());
                }
                if !crate::animated_image::within_decode_budget(path) {
                    return None;
                }
                let pixbuf = gdk_pixbuf::Pixbuf::from_file(path).ok()?;
                let scaled = scale_reaction_pixbuf(&pixbuf);
                let texture = gtk::gdk::Texture::for_pixbuf(&scaled);
                map.insert(path.to_path_buf(), texture.clone());
                Some(texture.upcast())
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

pub fn load_add_reaction_widget() -> Widget {
    let pic = Picture::new();
    pic.set_size_request(32, 32);
    pic.set_can_shrink(true);
    pic.set_content_fit(gtk::ContentFit::Cover);
    pic.add_css_class("reaction-emoji-image");

    if let Ok(loader) = gdk_pixbuf::PixbufLoader::with_type("svg") {
        if loader.write(ADD_REACTION_SVG.as_bytes()).is_ok() && loader.close().is_ok() {
            if let Some(pixbuf) = loader.pixbuf() {
                if let Some(scaled) = pixbuf.scale_simple(32, 32, gdk_pixbuf::InterpType::Bilinear)
                {
                    let texture = gtk::gdk::Texture::for_pixbuf(&scaled);
                    pic.set_paintable(Some(&texture));
                    return pic.upcast::<Widget>();
                }
            }
        }
    }
    let icon = Image::from_icon_name("list-add-symbolic");
    icon.set_pixel_size(32);
    icon.add_css_class("reaction-emoji-image");
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
