use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{Box, Button, Orientation};

use super::actions::{dispatch_timeline_action, ActionSlot, HoveredMessage, TimelineAction};
use super::reactions::{load_add_reaction_widget, resolve_reaction_emoji_widget};
use crate::message_markup::MessageRenderContext;

/// Shared hover state the quick bar buttons need to cooperate with the row.
pub(crate) struct QuickBarState<'a> {
    pub(crate) on_action: &'a ActionSlot,
    pub(crate) hover_generation: &'a Rc<Cell<u64>>,
    pub(crate) hovered: &'a Rc<RefCell<Option<HoveredMessage>>>,
    pub(crate) current_popover: &'a Rc<RefCell<Option<gtk::Popover>>>,
    pub(crate) bar_motion: &'a gtk::EventControllerMotion,
    pub(crate) can_mark_unread: bool,
}

pub(crate) fn rebuild_quick_bar(
    quick_bar: &Box,
    ts: &str,
    reactions: &[crate::emoji::EmojiEntry],
    context: &MessageRenderContext,
    state: &QuickBarState<'_>,
) {
    let QuickBarState {
        on_action,
        hover_generation,
        hovered,
        current_popover,
        bar_motion,
        can_mark_unread,
    } = *state;
    while let Some(child) = quick_bar.first_child() {
        quick_bar.remove(&child);
    }

    let emoji_catalog = crate::emoji::EmojiCatalog::new(&context.custom_emojis);

    for entry in reactions {
        let btn = Button::new();
        btn.set_focus_on_click(false);
        let widget =
            resolve_reaction_emoji_widget(&entry.name, &entry.name, context, &emoji_catalog);
        btn.set_child(Some(&widget));
        btn.add_css_class("flat");
        btn.add_css_class("circular");
        btn.set_tooltip_text(Some(&format!(":{}:", entry.name)));
        let ts = ts.to_string();
        let name = entry.name.clone();
        let on_action = on_action.clone();
        btn.connect_clicked(move |_| {
            dispatch_timeline_action(
                &on_action,
                TimelineAction::ToggleReaction {
                    ts: ts.clone(),
                    name: name.clone(),
                    add: true,
                },
            );
        });
        quick_bar.append(&btn);
    }

    let add_btn = Button::new();
    add_btn.set_focus_on_click(false);
    add_btn.set_child(Some(&load_add_reaction_widget()));
    add_btn.add_css_class("flat");
    add_btn.add_css_class("circular");
    add_btn.set_tooltip_text(Some("Add reaction..."));
    {
        let custom_emojis = context.custom_emojis.clone();
        let ts = ts.to_string();
        let on_action = on_action.clone();
        let weak_add_btn = add_btn.downgrade();
        add_btn.connect_clicked(move |_| {
            let Some(btn) = weak_add_btn.upgrade() else {
                return;
            };
            let on_action = on_action.clone();
            let ts = ts.clone();
            let picker = crate::emoji_picker_window::EmojiPickerWindow::new(
                &btn,
                &custom_emojis,
                move |selected_name| {
                    let clean = selected_name.trim().trim_matches(':');
                    if !clean.is_empty() {
                        dispatch_timeline_action(
                            &on_action,
                            TimelineAction::ToggleReaction {
                                ts: ts.clone(),
                                name: clean.to_string(),
                                add: true,
                            },
                        );
                    }
                },
            );
            picker.present();
        });
    }
    quick_bar.append(&add_btn);

    quick_bar.append(&gtk::Separator::new(Orientation::Vertical));

    let reply_btn = Button::from_icon_name("mail-reply-sender-symbolic");
    reply_btn.set_focus_on_click(false);
    reply_btn.add_css_class("flat");
    reply_btn.add_css_class("circular");
    reply_btn.set_tooltip_text(Some("Reply in thread"));
    {
        let ts = ts.to_string();
        let on_action = on_action.clone();
        reply_btn.connect_clicked(move |_| {
            dispatch_timeline_action(&on_action, TimelineAction::OpenThread(ts.clone()));
        });
    }
    quick_bar.append(&reply_btn);

    let forward_btn = Button::from_icon_name("mail-forward-symbolic");
    forward_btn.set_focus_on_click(false);
    forward_btn.add_css_class("flat");
    forward_btn.add_css_class("circular");
    forward_btn.set_tooltip_text(Some("Forward message..."));
    {
        let ts = ts.to_string();
        let on_action = on_action.clone();
        forward_btn.connect_clicked(move |_| {
            dispatch_timeline_action(&on_action, TimelineAction::ForwardMessage(ts.clone()));
        });
    }
    quick_bar.append(&forward_btn);

    let overflow_btn = gtk::MenuButton::new();
    overflow_btn.set_focus_on_click(false);
    overflow_btn.set_icon_name("view-more-symbolic");
    overflow_btn.add_css_class("flat");
    overflow_btn.add_css_class("circular");
    overflow_btn.set_tooltip_text(Some("More actions"));

    let popover = gtk::Popover::new();
    let popover_box = Box::new(Orientation::Vertical, 0);

    let copy_link_btn = Button::with_label("Copy link");
    copy_link_btn.set_focus_on_click(false);
    copy_link_btn.add_css_class("flat");
    {
        let ts = ts.to_string();
        let on_action = on_action.clone();
        let popover_weak = popover.downgrade();
        copy_link_btn.connect_clicked(move |_| {
            dispatch_timeline_action(&on_action, TimelineAction::CopyMessageLink(ts.clone()));
            if let Some(p) = popover_weak.upgrade() {
                p.popdown();
            }
        });
    }
    popover_box.append(&copy_link_btn);

    let copy_text_btn = Button::with_label("Copy message");
    copy_text_btn.set_focus_on_click(false);
    copy_text_btn.add_css_class("flat");
    {
        let ts = ts.to_string();
        let on_action = on_action.clone();
        let popover_weak = popover.downgrade();
        copy_text_btn.connect_clicked(move |_| {
            dispatch_timeline_action(&on_action, TimelineAction::CopyMessageText(ts.clone()));
            if let Some(p) = popover_weak.upgrade() {
                p.popdown();
            }
        });
    }
    popover_box.append(&copy_text_btn);

    let copy_details_btn = Button::with_label("Copy with details");
    copy_details_btn.set_focus_on_click(false);
    copy_details_btn.add_css_class("flat");
    {
        let ts = ts.to_string();
        let on_action = on_action.clone();
        let popover_weak = popover.downgrade();
        copy_details_btn.connect_clicked(move |_| {
            dispatch_timeline_action(
                &on_action,
                TimelineAction::CopyMessageWithMetadata(ts.clone()),
            );
            if let Some(p) = popover_weak.upgrade() {
                p.popdown();
            }
        });
    }
    popover_box.append(&copy_details_btn);

    if can_mark_unread {
        let unread_btn = Button::with_label("Mark unread");
        unread_btn.set_focus_on_click(false);
        unread_btn.add_css_class("flat");
        {
            let ts = ts.to_string();
            let on_action = on_action.clone();
            let popover_weak = popover.downgrade();
            unread_btn.connect_clicked(move |_| {
                dispatch_timeline_action(&on_action, TimelineAction::MarkUnread(ts.clone()));
                if let Some(p) = popover_weak.upgrade() {
                    p.popdown();
                }
            });
        }
        popover_box.append(&unread_btn);
    }

    popover.set_child(Some(&popover_box));
    *current_popover.borrow_mut() = Some(popover.clone());

    {
        popover.connect_show(move |p| {
            crate::debug::log(
                "quickbar",
                &format!("popover show is_visible={}", p.is_visible()),
            );
        });
    }
    {
        let hover_generation = hover_generation.clone();
        let hovered = hovered.clone();
        let bar_motion = bar_motion.clone();
        let quick_bar_weak = quick_bar.downgrade();
        popover.connect_closed(move |p| {
            let contains_pointer = bar_motion.contains_pointer();
            crate::debug::log(
                "quickbar",
                &format!(
                    "popover closed is_visible={} bar_contains_pointer={contains_pointer}",
                    p.is_visible()
                ),
            );
            // The pointer never crosses the bar's boundary while the popover
            // has the grab, so no fresh `Enter` arrives here to tell us the
            // user is still hovering - ask the bar's own motion controller
            // directly instead of guessing from stale crossing events.
            if !contains_pointer {
                hover_generation.set(hover_generation.get().wrapping_add(1));
                *hovered.borrow_mut() = None;
                if let Some(bar) = quick_bar_weak.upgrade() {
                    bar.set_visible(false);
                    crate::debug::log("quickbar", "popover closed -> hiding quick bar");
                }
            } else {
                crate::debug::log(
                    "quickbar",
                    "popover closed -> pointer still over bar, keeping it visible",
                );
            }
        });
    }

    overflow_btn.set_popover(Some(&popover));
    quick_bar.append(&overflow_btn);
}
