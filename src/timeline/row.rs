use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{
    gdk_pixbuf, glib, pango, Box, Button, CssProvider, Image, Label, Orientation, Picture,
    Separator, ToggleButton, Widget,
};

use crate::message_html::MessageHtmlContext;
use crate::models::{SlackFile, SlackMessage};

use super::actions::{
    ActionHandler, HoverEvent, OpenMediaCallback, TimelineAction, ToggleReactionCallback,
};
use super::cache::{
    get_or_load_texture, is_media_collapsed, resolve_cached_asset_path, set_media_collapsed,
};
use super::reactions::build_reactions_widget;
use super::text::{create_message_text_widget, render_text_content};

pub const TIMELINE_AVATAR_SIZE: i32 = 36;
pub const FOCUS_TARGET_CLASS: &str = "timeline-focus-target";
pub const FOCUS_TARGET_DURATION: std::time::Duration = std::time::Duration::from_secs(3);

pub fn register_timeline_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if !gtk::is_initialized() {
            return;
        }
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };
        let provider = CssProvider::new();
        provider.load_from_string(
            r#"
            .blockquote { border-left: 3px solid #888888; padding-left: 8px; margin-left: 4px; }
            .author-menu-button { padding: 2px; margin: -2px; min-width: 0; min-height: 0; }
            listview.conduit-timeline > row { padding: 6px 12px; }
            avatar.timeline-avatar { border-radius: 8px; }
            .author-menu-button > button { padding: 0; min-width: 0; min-height: 0; }
            .reaction-pill { padding: 2px 6px; min-width: 0; min-height: 0; border-radius: 12px; }
            .reaction-pill label { min-width: 0; }
            .reaction-emoji-unicode { font-size: 32px; line-height: 1; }
            .reaction-emoji-image { margin-top: 3px; margin-bottom: 3px; }
            .timeline-attachment { border-left: 3px solid #e0e0e0; padding-left: 8px; margin-left: 4px; }
            .timeline-media-image { border-radius: 8px; }
            .timeline-media-placeholder {
                border-radius: 8px;
                background-color: alpha(currentColor, 0.08);
            }
            .timeline-media-title { padding: 0 4px; min-height: 0; font-size: smaller; }
            .timeline-video-play-icon {
                background-color: rgba(0, 0, 0, 0.55);
                border-radius: 9999px;
                padding: 12px;
                color: #ffffff;
            }
            .thread-reply-pill {
                background-color: #D6ECFF;
                color: #1264A3;
                border-radius: 12px;
                padding: 2px 10px;
            }
            .thread-reply-active { font-weight: bold; }
            .reaction-pill-active { background-color: #D6ECFF; color: #1264A3; }
            .timeline-focus-target { background-color: alpha(@accent_bg_color, 0.18); border-radius: 8px; transition: background-color 800ms ease-out; }
            .day-separator-line {
                background-color: color-mix(in srgb, currentColor 15%, transparent);
                min-height: 1px;
            }
            .unread-separator-line { background-color: #1264A3; min-height: 2px; }
            .unread-separator-label { color: #1264A3; font-weight: bold; }
            .timeline-text-view,
            .timeline-text-view:focus,
            .timeline-text-view text {
                background-color: transparent;
                outline: none;
                box-shadow: none;
                padding: 0;
                margin: 0;
            }
            .code-block {
                border-radius: 6px;
                border: 1px solid alpha(currentColor, 0.15);
                background-color: alpha(currentColor, 0.05);
                padding: 6px 8px;
            }
            .code-block-header {
                padding-bottom: 4px;
                margin-bottom: 2px;
                border-bottom: 1px solid alpha(currentColor, 0.08);
            }
            .code-block-lang {
                font-size: 0.85em;
                font-weight: 600;
            }
            .code-block-copy {
                padding: 2px 4px;
                min-height: 0;
                min-width: 0;
            }
            .code-block sourceview,
            .code-block textview {
                background-color: transparent;
            }
            "#,
        );
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

/// `slot` must be stable and unique per message (e.g. `"file:{index}"` vs
/// `"attachment:{index}"`) so the collapsed state survives row rebuilds and
/// files/attachments at the same index don't collide in the same key space.
pub fn wrap_collapsible_media(media: Widget, ts: &str, slot: &str, title: Option<&str>) -> Box {
    let container = Box::new(Orientation::Vertical, 2);
    container.set_halign(gtk::Align::Start);

    let collapsed = is_media_collapsed(ts, slot);

    let icon_for = |expanded: bool| {
        if expanded {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        }
    };
    let toggle = ToggleButton::new();
    toggle.set_focus_on_click(false);
    // Titled media ("GIF ▾") shows dim title text followed by the caret, like
    // Slack; untitled media keeps the compact icon-only circular toggle.
    let caret = Image::from_icon_name(icon_for(!collapsed));
    if let Some(title) = title {
        let title_row = Box::new(Orientation::Horizontal, 2);
        let label = Label::new(Some(title));
        label.set_ellipsize(pango::EllipsizeMode::End);
        title_row.append(&label);
        title_row.append(&caret);
        toggle.set_child(Some(&title_row));
        toggle.add_css_class("timeline-media-title");
        toggle.add_css_class("dim-label");
    } else {
        toggle.set_child(Some(&caret));
        toggle.add_css_class("circular");
    }
    toggle.add_css_class("flat");
    toggle.set_halign(gtk::Align::Start);
    toggle.set_tooltip_text(Some("Show or hide image"));
    toggle.set_active(!collapsed);

    media.set_visible(!collapsed);

    let ts_owned = ts.to_string();
    let slot_owned = slot.to_string();
    let media_weak = media.downgrade();
    toggle.connect_toggled(move |btn| {
        let expanded = btn.is_active();
        caret.set_icon_name(Some(icon_for(expanded)));
        if let Some(media) = media_weak.upgrade() {
            media.set_visible(expanded);
        }
        set_media_collapsed(&ts_owned, &slot_owned, !expanded);
    });

    container.append(&toggle);
    container.append(&media);
    container
}

/// Loads `path` as an animated `Picture` when it's a multi-frame image (GIF/
/// animated WebP), falling back to the shared static-texture cache otherwise.
/// Generalizes the animation driver already used for custom emoji reactions
/// (see `load_custom_emoji_picture`) to arbitrary message/attachment images.
pub fn load_animated_or_static_picture(
    path: &Path,
    width: i32,
    height: i32,
    content_fit: gtk::ContentFit,
) -> Widget {
    if let Ok(anim) = gdk_pixbuf::PixbufAnimation::from_file(path) {
        if !anim.is_static_image() {
            let iter = anim.iter(None);
            let first_pixbuf = iter.pixbuf();
            let pic = Picture::for_paintable(&gtk::gdk::Texture::for_pixbuf(&first_pixbuf));
            pic.set_size_request(width, height);
            pic.set_can_shrink(true);
            pic.set_content_fit(content_fit);

            let start_animation = move |p: &Picture, anim_obj: &gdk_pixbuf::PixbufAnimation| {
                let iter = anim_obj.iter(None);
                let last_update = Rc::new(Cell::new(std::time::Instant::now()));
                let delay_ms = iter
                    .delay_time()
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(60)
                    .max(60);
                p.add_tick_callback(move |p, _frame_clock| {
                    if !p.is_mapped() {
                        return glib::ControlFlow::Break;
                    }
                    if last_update.get().elapsed().as_millis() as u64 >= delay_ms {
                        last_update.set(std::time::Instant::now());
                        iter.advance(std::time::SystemTime::now());
                        p.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&iter.pixbuf())));
                    }
                    glib::ControlFlow::Continue
                });
            };

            let anim_for_map = anim.clone();
            pic.connect_map(move |p| {
                start_animation(p, &anim_for_map);
            });
            if pic.is_mapped() {
                start_animation(&pic, &anim);
            }

            return pic.upcast::<Widget>();
        }
    }

    let pic = if let Some(tex) = get_or_load_texture(path) {
        Picture::for_paintable(&tex)
    } else {
        Picture::for_filename(path)
    };
    pic.set_size_request(width, height);
    pic.set_can_shrink(true);
    pic.set_content_fit(content_fit);
    pic.upcast::<Widget>()
}

pub fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn render_files(
    files: &[SlackFile],
    root_box: &Box,
    context: &MessageHtmlContext,
    on_open_media: Option<&OpenMediaCallback>,
    on_action: Option<&ActionHandler>,
    ts: &str,
) {
    let files_box = Box::new(Orientation::Vertical, 6);
    files_box.set_halign(gtk::Align::Start);

    for (index, file) in files.iter().enumerate() {
        let is_video = file
            .mimetype
            .as_deref()
            .is_some_and(|m| m.starts_with("video/"))
            || file.thumb_video.is_some()
            || file.supported_media_kind() == Some("video");

        let is_image = file
            .mimetype
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
            || file.thumb_360.is_some()
            || file.thumb_480.is_some()
            || file.thumb_720.is_some()
            || file.thumb_1024.is_some()
            || file.url_static_preview.is_some()
            || file.preview_url().is_some();

        let title = file.display_title().to_string();

        if is_video {
            let video_candidates: Vec<&str> = [
                file.thumb_video.as_deref(),
                file.thumb_1024.as_deref(),
                file.thumb_720.as_deref(),
                file.thumb_480.as_deref(),
                file.thumb_360.as_deref(),
                file.url_static_preview.as_deref(),
                file.preview_url(),
                file.url_private.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect();

            let local_thumb = video_candidates.iter().find_map(|&url| {
                resolve_cached_asset_path(url, context).or_else(|| {
                    if std::path::Path::new(url).exists() {
                        Some(std::path::PathBuf::from(url))
                    } else {
                        None
                    }
                })
            });

            let video_url = file
                .media_url()
                .or(file.url_private_download.as_deref())
                .or(file.url_private.as_deref())
                .unwrap_or("");

            let container = Box::new(Orientation::Vertical, 4);
            container.add_css_class("timeline-video-container");
            container.set_halign(gtk::Align::Start);

            let overlay = gtk::Overlay::new();

            if let Some(path) = local_thumb {
                let pic = if let Some(tex) = get_or_load_texture(&path) {
                    Picture::for_paintable(&tex)
                } else {
                    Picture::for_filename(&path)
                };
                pic.set_content_fit(gtk::ContentFit::ScaleDown);
                pic.set_size_request(600, 340);
                pic.set_tooltip_text(Some(&title));
                overlay.set_child(Some(&pic));
            } else {
                let poster_box = Box::new(Orientation::Vertical, 0);
                poster_box.set_size_request(360, 200);
                poster_box.add_css_class("card");
                poster_box.add_css_class("rounded");
                poster_box.set_tooltip_text(Some(&title));
                overlay.set_child(Some(&poster_box));
            }

            let play_icon = Image::from_icon_name("media-playback-start-symbolic");
            play_icon.set_pixel_size(24);
            play_icon.add_css_class("timeline-video-play-icon");
            play_icon.set_valign(gtk::Align::Center);
            play_icon.set_halign(gtk::Align::Center);
            play_icon.set_can_target(false);
            overlay.add_overlay(&play_icon);

            container.append(&overlay);

            if !video_url.is_empty() {
                let gesture = gtk::GestureClick::new();
                let cb = on_open_media.cloned();
                let url = video_url.to_string();
                let name = title.clone();
                gesture.connect_pressed(move |_, _, _, _| {
                    if let Some(cb) = &cb {
                        cb(crate::window::MediaGalleryItem {
                            url: url.clone(),
                            name: name.clone(),
                            kind: crate::window::MediaKind::Video,
                        });
                    }
                });
                container.add_controller(gesture);
                container.set_cursor_from_name(Some("pointer"));
            }

            files_box.append(&wrap_collapsible_media(
                container.upcast::<Widget>(),
                ts,
                &format!("file:{index}"),
                None,
            ));
        } else if is_image {
            let container = Box::new(Orientation::Vertical, 2);
            container.add_css_class("timeline-image-container");
            container.set_halign(gtk::Align::Start);

            let candidates: Vec<&str> = [
                file.preview_url(),
                file.url_static_preview.as_deref(),
                file.thumb_1024.as_deref(),
                file.thumb_720.as_deref(),
                file.thumb_480.as_deref(),
                file.thumb_360.as_deref(),
                file.thumb_160.as_deref(),
                file.url_private.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect();

            let local_path = candidates.iter().find_map(|&url| {
                resolve_cached_asset_path(url, context).or_else(|| {
                    if std::path::Path::new(url).exists() {
                        Some(std::path::PathBuf::from(url))
                    } else {
                        None
                    }
                })
            });

            let img_widget: Widget = if let Some(path) = local_path {
                let pic =
                    load_animated_or_static_picture(&path, 400, 300, gtk::ContentFit::ScaleDown);
                pic.add_css_class("rounded");
                pic
            } else {
                let icon = Image::from_icon_name("image-x-generic-symbolic");
                icon.set_pixel_size(48);
                icon.upcast::<Widget>()
            };

            img_widget.set_tooltip_text(Some(&title));
            container.append(&img_widget);

            if let Some(media_url) = file.media_url() {
                let gesture = gtk::GestureClick::new();
                let cb = on_open_media.cloned();
                let url = media_url.to_string();
                let name = title.clone();
                gesture.connect_pressed(move |_, _, _, _| {
                    if let Some(cb) = &cb {
                        cb(crate::window::MediaGalleryItem {
                            url: url.clone(),
                            name: name.clone(),
                            kind: crate::window::MediaKind::Image,
                        });
                    }
                });
                container.add_controller(gesture);
                container.set_cursor_from_name(Some("pointer"));
            }

            files_box.append(&wrap_collapsible_media(
                container.upcast::<Widget>(),
                ts,
                &format!("file:{index}"),
                None,
            ));
        } else {
            let file_card = Box::new(Orientation::Horizontal, 8);
            file_card.add_css_class("file-card");
            file_card.set_halign(gtk::Align::Start);

            let mime = file.mimetype.as_deref().unwrap_or("");
            let icon_name = if mime.contains("zip")
                || mime.contains("tar")
                || mime.contains("archive")
                || mime.contains("compressed")
            {
                "package-x-generic-symbolic"
            } else {
                "text-x-generic-symbolic"
            };

            let icon = Image::from_icon_name(icon_name);
            icon.set_pixel_size(24);
            file_card.append(&icon);

            let info_box = Box::new(Orientation::Vertical, 2);
            info_box.set_hexpand(true);

            let title_label = Label::new(None);
            title_label.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(&title)));
            title_label.set_xalign(0.0);
            title_label.set_ellipsize(pango::EllipsizeMode::End);
            info_box.append(&title_label);

            if let Some(bytes) = file.size {
                let size_str = format_file_size(bytes);
                let size_label = Label::new(Some(&size_str));
                size_label.add_css_class("dim-label");
                size_label.set_xalign(0.0);
                info_box.append(&size_label);
            }

            file_card.append(&info_box);

            let download_btn = Button::from_icon_name("document-save-symbolic");
            download_btn.set_focus_on_click(false);
            download_btn.add_css_class("flat");
            download_btn.set_tooltip_text(Some("Download file"));
            match (on_action, file.download_url()) {
                (Some(on_action), Some(url)) => {
                    let on_action = on_action.clone();
                    let url = url.to_string();
                    let name = file
                        .name
                        .as_deref()
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or(&title)
                        .to_string();
                    download_btn.connect_clicked(move |_| {
                        on_action(TimelineAction::DownloadFile {
                            url: url.clone(),
                            name: name.clone(),
                        });
                    });
                }
                _ => download_btn.set_sensitive(false),
            }
            file_card.append(&download_btn);

            files_box.append(&file_card);
        }
    }

    root_box.append(&files_box);
}

pub fn build_avatar_widget(message: &SlackMessage, context: &MessageHtmlContext) -> Widget {
    // A fixed-size avatar: a bare Picture would grow to the image's natural
    // size in the row.
    let avatar = adw::Avatar::new(TIMELINE_AVATAR_SIZE, None, false);
    avatar.add_css_class("timeline-avatar");
    let user_avatar_url = context
        .display_user_id(message)
        .and_then(|user_id| context.user_avatar_urls.get(user_id));
    let avatar_source_url = user_avatar_url
        .map(String::as_str)
        .or_else(|| message.avatar_url());

    if let Some(url) = avatar_source_url {
        let local_path = resolve_cached_asset_path(url, context)
            .or_else(|| Path::new(url).exists().then(|| PathBuf::from(url)));
        let texture = local_path.and_then(|path| {
            get_or_load_texture(&path).or_else(|| gtk::gdk::Texture::from_filename(&path).ok())
        });
        if let Some(texture) = texture {
            avatar.set_custom_image(Some(texture.upcast_ref::<gtk::gdk::Paintable>()));
        }
    }
    avatar.upcast::<Widget>()
}

pub fn build_thread_replies_widget(
    message: &SlackMessage,
    context: &MessageHtmlContext,
    on_open_thread: Option<&Rc<dyn Fn(String)>>,
) -> Option<Box> {
    let reply_count = message.reply_count.filter(|count| *count > 0)?;
    let cb = on_open_thread?;

    let thread_footer = Box::new(Orientation::Horizontal, 6);
    let count_str = if reply_count == 1 {
        "1 reply".to_string()
    } else {
        format!("{reply_count} replies")
    };
    let label_text = if let Some(latest_ts) = &message.latest_reply {
        if let Some((_m, _full, short)) = crate::message_html::localized_timestamp_parts(latest_ts)
        {
            format!("{count_str}   Last reply {short}")
        } else {
            count_str
        }
    } else {
        count_str
    };
    let reply_button = Button::with_label(&label_text);
    reply_button.set_focus_on_click(false);
    reply_button.add_css_class("flat");
    reply_button.add_css_class("thread-reply-pill");
    let replied = message.reply_users.as_deref().is_some_and(|users| {
        context
            .current_user_id
            .as_deref()
            .is_some_and(|me| users.iter().any(|u| u == me))
    });
    if replied {
        reply_button.add_css_class("thread-reply-active");
    }
    let cb = cb.clone();
    let ts = message.ts.clone();
    reply_button.connect_clicked(move |_| {
        cb(ts.clone());
    });
    thread_footer.append(&reply_button);
    Some(thread_footer)
}

/// Full-width rule with the day label ("Today", "15 Sep", ...) centered on
/// it. Deliberately distinct from the red/accent unread separator.
pub fn day_separator_widget(ts: &str) -> Box {
    let label_text = crate::message_html::slack_ts_datetime(ts)
        .zip(gtk::glib::DateTime::now_local().ok())
        .and_then(|(dt, now)| crate::day_label::day_label(&dt, &now))
        .unwrap_or_default();

    let row = Box::new(Orientation::Horizontal, 12);
    row.set_margin_top(8);
    row.set_margin_bottom(8);
    row.set_margin_start(12);
    row.set_margin_end(12);
    row.update_property(&[gtk::accessible::Property::Label(&label_text)]);

    let append_line = || {
        let line = Separator::new(Orientation::Horizontal);
        line.add_css_class("day-separator-line");
        line.set_valign(gtk::Align::Center);
        line.set_hexpand(true);
        row.append(&line);
    };
    append_line();
    let label = Label::new(Some(&label_text));
    label.add_css_class("dim-label");
    label.add_css_class("caption");
    label.add_css_class("heading");
    row.append(&label);
    append_line();
    register_timeline_css();
    row
}

pub fn unread_separator_widget() -> Box {
    let separator_row = Box::new(Orientation::Horizontal, 6);
    separator_row.set_margin_top(4);
    separator_row.set_margin_bottom(4);

    let line = Separator::new(Orientation::Horizontal);
    line.add_css_class("unread-separator-line");
    line.set_valign(gtk::Align::Center);
    line.set_hexpand(true);
    separator_row.append(&line);

    let label = Label::new(Some(&gettextrs::gettext("New")));
    label.add_css_class("caption");
    label.add_css_class("unread-separator-label");
    separator_row.append(&label);

    separator_row
}

/// Wraps `content` with the unread separator above it when `message_ts`
/// matches the frozen anchor in `context`. Must wrap in an outer VERTICAL
/// container regardless of `content`'s own orientation (e.g. the
/// horizontal system-message row), otherwise the separator would be laid
/// out side-by-side with the row instead of above it.
pub fn wrap_with_unread_separator_if_needed(
    content: Box,
    message_ts: &str,
    context: &MessageHtmlContext,
) -> Box {
    // Tag the row with its message ts (repurposing the CSS/accessible widget
    // name as a plain string tag, not used for CSS selection anywhere in this
    // app) so the visibility-based read-marking walk can recover which
    // message a given realized row widget corresponds to.
    content.set_widget_name(message_ts);
    if context.unread_separator_ts.as_deref() != Some(message_ts) {
        return content;
    }
    let outer = Box::new(Orientation::Vertical, 0);
    outer.append(&unread_separator_widget());
    outer.append(&content);
    outer.set_widget_name(message_ts);
    outer
}

/// Body, document nodes and files, all driven by the canonical
/// [`crate::rich_message::MessageDocument`] so cached and freshly fetched
/// messages render identically.
fn render_message_content(
    message: &SlackMessage,
    context: &MessageHtmlContext,
    on_open_media: Option<&OpenMediaCallback>,
    on_action: Option<&ActionHandler>,
    root_box: &Box,
) {
    let document = message.rendered_document();
    if !crate::timeline_document_widget::document_replaces_text(&document) {
        let content_text = message.text.as_deref().unwrap_or("");
        if !content_text.trim().is_empty() {
            render_text_content(content_text, root_box, context, on_action);
        }
    }

    let resolve = |url: &str| resolve_cached_asset_path(url, context);
    let is_failed = |url: &str| context.failed_image_urls.contains(url);
    let sources = crate::timeline_media::MediaSources {
        resolve: &resolve,
        is_failed: &is_failed,
    };
    crate::timeline_document_widget::DocumentRenderer::new(
        context,
        sources,
        &message.ts,
        on_action,
    )
    .render(document.nodes(), root_box);

    let files = message
        .files
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|file| !SlackMessage::document_renders_file(&document, file))
        .cloned()
        .collect::<Vec<_>>();
    if !files.is_empty() {
        render_files(
            &files,
            root_box,
            context,
            on_open_media,
            on_action,
            &message.ts,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_timeline_message_widget(
    message: &SlackMessage,
    context: &MessageHtmlContext,
    on_open_media: Option<&OpenMediaCallback>,
    on_open_thread: Option<&Rc<dyn Fn(String)>>,
    on_toggle_reaction: Option<&ToggleReactionCallback>,
    on_hover: Option<&Rc<dyn Fn(HoverEvent)>>,
    on_author_action: Option<&ActionHandler>,
    show_header: bool,
) -> Box {
    register_timeline_css();
    if let Some(subtype) = message.subtype.as_deref() {
        if matches!(
            subtype,
            "channel_join" | "channel_leave" | "channel_topic" | "channel_purpose"
        ) {
            let root_box = Box::new(Orientation::Horizontal, 6);
            root_box.add_css_class("system-message");

            let icon_name = match subtype {
                "channel_join" | "channel_leave" => "emblem-shared-symbolic",
                _ => "dialog-information-symbolic",
            };
            let icon = Image::from_icon_name(icon_name);
            icon.set_pixel_size(16);
            root_box.append(&icon);

            let sys_text = message.text.as_deref().unwrap_or(match subtype {
                "channel_join" => "joined the channel",
                "channel_leave" => "left the channel",
                "channel_topic" => "set the channel topic",
                "channel_purpose" => "set the channel purpose",
                _ => "system event",
            });

            let pango = crate::message_html::mrkdwn_to_pango(sys_text, context);
            let text_widget =
                create_message_text_widget(&format!("<i>{}</i>", pango), context, on_author_action);
            text_widget.add_css_class("dim-label");
            root_box.append(&text_widget);

            return wrap_with_unread_separator_if_needed(root_box, &message.ts, context);
        }
    }

    let root_box = Box::new(Orientation::Vertical, 6);

    if message.is_thread_broadcast == Some(true)
        || message.subtype.as_deref() == Some("thread_broadcast")
    {
        let broadcast_box = Box::new(Orientation::Horizontal, 4);
        broadcast_box.add_css_class("thread-broadcast-banner");
        let pill_label = Label::new(Some("also sent to channel"));
        pill_label.add_css_class("dim-label");
        pill_label.add_css_class("pill");
        broadcast_box.append(&pill_label);
        root_box.append(&broadcast_box);
    }

    // Avatar column beside a body column holding header, content, reactions
    // and the thread footer.
    let columns = Box::new(Orientation::Horizontal, 10);
    columns.add_css_class("timeline-message-columns");
    let body_box = Box::new(Orientation::Vertical, 4);
    body_box.set_hexpand(true);

    if show_header {
        let header_box = Box::new(Orientation::Horizontal, 8);
        let avatar_widget = build_avatar_widget(message, context);

        let author_name = context
            .display_user_id(message)
            .and_then(|user_id| {
                context
                    .user_full_names
                    .get(user_id)
                    .cloned()
                    .or_else(|| context.user_names.get(user_id).cloned())
            })
            .unwrap_or_else(|| message.author_label());

        let author_label = Label::new(None);
        author_label.set_markup(&format!(
            "<b>{}</b>",
            glib::markup_escape_text(&author_name)
        ));
        author_label.set_xalign(0.0);

        let author_user_id = context.display_user_id(message);
        let menu_access = crate::author_menu::author_menu_access(
            author_user_id,
            context.current_user_id.as_deref(),
            &context.bot_user_ids,
        );
        avatar_widget.set_valign(gtk::Align::Start);
        match (on_author_action, author_user_id) {
            (Some(on_action), Some(user_id)) if menu_access.any() => {
                let menu_button = |child: &Widget| {
                    let button = crate::author_menu::author_menu_button(
                        child,
                        user_id,
                        &author_name,
                        menu_access,
                        on_action.clone(),
                    );
                    button.set_valign(gtk::Align::Start);
                    button
                };
                // The name button is the keyboard and screen reader entry point;
                // the avatar one is a pointer-only duplicate.
                let avatar_button = menu_button(&avatar_widget);
                avatar_button.set_can_focus(false);
                avatar_button.set_focusable(false);
                if let Some(inner) = avatar_button.first_child() {
                    // MenuButton delegates focus to its inner toggle.
                    inner.set_can_focus(false);
                    inner.set_focusable(false);
                }
                avatar_button.update_state(&[gtk::accessible::State::Hidden(true)]);
                columns.append(&avatar_button);
                header_box.append(&menu_button(author_label.upcast_ref()));
            }
            _ => {
                columns.append(&avatar_widget);
                header_box.append(&author_label);
            }
        }

        let timestamp_label = Label::new(None);
        if let Some((_machine, full, short)) =
            crate::message_html::localized_timestamp_parts(&message.ts)
        {
            timestamp_label.set_text(&short);
            timestamp_label.set_tooltip_text(Some(&full));
        } else {
            timestamp_label.set_text(&message.ts);
        }
        timestamp_label.add_css_class("dim-label");
        timestamp_label.add_css_class("caption");
        timestamp_label.set_xalign(0.0);
        header_box.append(&timestamp_label);

        body_box.append(&header_box);
    } else {
        root_box.add_css_class("timeline-grouped-message");
        let spacer = Box::new(Orientation::Horizontal, 0);
        spacer.set_width_request(TIMELINE_AVATAR_SIZE);
        spacer.set_valign(gtk::Align::Start);
        spacer.set_can_focus(false);
        spacer.set_focusable(false);
        spacer.update_state(&[gtk::accessible::State::Hidden(true)]);
        columns.append(&spacer);
    }

    columns.append(&body_box);
    root_box.append(&columns);

    render_message_content(message, context, on_open_media, on_author_action, &body_box);

    if let Some(wrap_box) = build_reactions_widget(message, context, on_toggle_reaction) {
        body_box.append(&wrap_box);
    }

    if let Some(thread_footer) = build_thread_replies_widget(message, context, on_open_thread) {
        body_box.append(&thread_footer);
    }

    if let Some(cb) = on_hover {
        let motion = gtk::EventControllerMotion::new();
        let cb_enter = cb.clone();
        let ts = message.ts.clone();
        let reactions = crate::message_html::recent_reactions(context);
        let row_for_enter = root_box.clone().upcast::<Widget>();
        motion.connect_enter(move |_, _, _| {
            cb_enter(HoverEvent::Enter {
                row: row_for_enter.clone(),
                ts: ts.clone(),
                reactions: reactions.clone(),
            });
        });
        let cb_leave = cb.clone();
        motion.connect_leave(move |_| {
            cb_leave(HoverEvent::Leave);
        });
        root_box.add_controller(motion);
    }

    wrap_with_unread_separator_if_needed(root_box, &message.ts, context)
}
