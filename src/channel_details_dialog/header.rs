//! Dialog header: private lock, title, muted marker and star toggle.

use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;

use super::{DetailsInput, Inner};

/// Returns the header row plus the muted marker and star toggle it contains.
pub(super) fn build_header(input: &DetailsInput) -> (gtk::Box, gtk::Box, gtk::ToggleButton) {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_margin_top(6);
    header.set_margin_bottom(6);
    header.set_margin_start(18);
    header.set_margin_end(18);
    if input.is_private {
        let lock = gtk::Image::from_icon_name("system-lock-screen-symbolic");
        lock.set_tooltip_text(Some(&gettext("Private")));
        header.append(&lock);
    }
    let title = gtk::Label::new(Some(&input.title));
    title.add_css_class("title-2");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_selectable(true);
    header.append(&title);

    let muted_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    muted_box.add_css_class("dim-label");
    muted_box.append(&gtk::Image::from_icon_name(
        "notifications-disabled-symbolic",
    ));
    muted_box.append(&gtk::Label::new(Some(&gettext("Muted"))));
    header.append(&muted_box);

    let star_button = gtk::ToggleButton::new();
    star_button.add_css_class("flat");
    star_button.set_valign(gtk::Align::Center);
    header.append(&star_button);
    (header, muted_box, star_button)
}

impl Inner {
    pub(super) fn connect_star(self: &Rc<Self>) {
        let channel_id = self.channel_id.clone();
        let on_star = self.callbacks.on_star.clone();
        let weak = Rc::downgrade(self);
        self.star_button.connect_toggled(move |_| {
            let Some(inner) = weak.upgrade() else { return };
            if inner.updating_star.get() {
                return;
            }
            let active = inner.star_button.is_active();
            inner.sync_star_appearance(active);
            on_star(channel_id.clone(), active);
        });
    }

    pub(super) fn sync_star_appearance(&self, starred: bool) {
        self.star_button.set_icon_name(if starred {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });
        let label = if starred {
            gettext("Remove star")
        } else {
            gettext("Star conversation")
        };
        self.star_button.set_tooltip_text(Some(&label));
        self.star_button
            .update_property(&[gtk::accessible::Property::Label(&label)]);
    }
}
