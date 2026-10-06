//! Widget construction for the profile page shown inside the dialog.

use adw::prelude::*;
use gettextrs::gettext;
use gtk::glib;

use super::model::{profile_view_model, DetailRow, ProfileInput, ProfileViewModel, RowKind};
use super::ProfileDialog;

fn now_and_viewer_offset() -> (i64, i64) {
    let now = glib::DateTime::now_local().ok();
    (
        now.as_ref().map_or(0, glib::DateTime::to_unix),
        now.map_or(0, |now| now.utc_offset().as_seconds()),
    )
}

impl ProfileDialog {
    pub(super) fn show_profile(&self, input: &ProfileInput) {
        let (now, viewer_offset) = now_and_viewer_offset();
        let model = profile_view_model(&input.user, &input.custom_emojis, now, viewer_offset);
        let page = self.build_page(&model, input);
        let old = self.stack.child_by_name("profile");
        // Removing a subtree that holds keyboard focus makes GTK defer a
        // focus move that can walk freed widgets (gtk_widget_is_ancestor
        // criticals). Park focus on the dialog first, restore it after.
        let had_focus = old.as_ref().is_some_and(|old| {
            self.dialog
                .focus()
                .is_some_and(|focus| focus == *old || focus.is_ancestor(old))
        });
        if had_focus {
            self.dialog.set_focus(None::<&gtk::Widget>);
        }
        if let Some(old) = old {
            self.stack.remove(&old);
        }
        self.stack.add_named(&page, Some("profile"));
        self.stack.set_visible_child_name("profile");
        if had_focus {
            page.child_focus(gtk::DirectionType::TabForward);
        }
    }

    fn toast_copied(&self, what: &str) {
        let message = gettext("{item} copied").replace("{item}", what);
        self.toasts.add_toast(adw::Toast::new(&message));
    }

    fn copy(&self, text: &str, what: &str) {
        self.dialog.clipboard().set_text(text);
        self.toast_copied(what);
    }

    fn build_page(&self, model: &ProfileViewModel, input: &ProfileInput) -> gtk::Widget {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
        content.set_margin_top(18);
        content.set_margin_bottom(24);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.append(&self.build_header(model, input));
        if !model.rows.is_empty() {
            content.append(&self.build_rows(&model.rows));
        }
        content.append(&self.build_buttons(input.can_message));

        let clamp = adw::Clamp::new();
        clamp.set_maximum_size(480);
        clamp.set_child(Some(&content));
        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
        scrolled.set_child(Some(&clamp));
        scrolled.upcast()
    }

    fn build_header(&self, model: &ProfileViewModel, input: &ProfileInput) -> gtk::Box {
        let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let avatar = adw::Avatar::new(96, Some(&model.display_name), true);
        if let Some(texture) = input
            .avatar_path
            .as_deref()
            .and_then(crate::timeline_message_widget::get_or_load_texture)
        {
            avatar.set_custom_image(Some(&texture));
        }
        avatar.set_halign(gtk::Align::Center);
        header.append(&avatar);

        let name = centered_label(&model.display_name, &["title-1"]);
        name.set_selectable(true);
        header.append(&name);
        let lines = [
            (model.identity_line.as_deref(), "dim-label"),
            (model.title.as_deref(), "dim-label"),
        ];
        for (line, class) in lines {
            if let Some(line) = line {
                header.append(&centered_label(line, &[class]));
            }
        }
        if let Some(status) = &model.status {
            let text = match &status.expiry {
                Some(expiry) => format!("{} · {expiry}", status.text),
                None => status.text.clone(),
            };
            header.append(&centered_label(&text, &[]));
        }
        if let Some(time) = &model.local_time {
            header.append(&centered_label(time, &["dim-label"]));
        }
        header
    }

    fn build_rows(&self, rows: &[DetailRow]) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::new();
        for detail in rows {
            let row = adw::ActionRow::new();
            row.set_use_markup(false);
            row.set_title(&detail.label);
            row.set_subtitle(&detail.value);
            row.set_subtitle_selectable(true);
            row.add_css_class("property");
            if detail.kind != RowKind::Text {
                let button = gtk::Button::from_icon_name("edit-copy-symbolic");
                button.set_valign(gtk::Align::Center);
                button.add_css_class("flat");
                let tooltip =
                    gettext("Copy {item}").replace("{item}", &detail.label.to_lowercase());
                button.set_tooltip_text(Some(&tooltip));
                button.update_property(&[gtk::accessible::Property::Label(&tooltip)]);
                let this = self.clone();
                let (value, label) = (detail.value.clone(), detail.label.clone());
                button.connect_clicked(move |_| this.copy(&value, &label));
                row.add_suffix(&button);
            }
            if detail.kind == RowKind::Email {
                row.set_activatable(true);
                let uri = format!("mailto:{}", detail.value);
                let dialog = self.dialog.clone();
                row.connect_activated(move |_| {
                    let window = dialog.root().and_downcast::<gtk::Window>();
                    gtk::UriLauncher::new(&uri).launch(
                        window.as_ref(),
                        None::<&gtk::gio::Cancellable>,
                        |_| {},
                    );
                });
            }
            group.add(&row);
        }
        group
    }

    fn build_buttons(&self, can_message: bool) -> gtk::Box {
        let buttons = gtk::Box::new(gtk::Orientation::Vertical, 12);
        buttons.set_halign(gtk::Align::Center);
        if can_message {
            let message = gtk::Button::with_label(&gettext("Message"));
            message.add_css_class("suggested-action");
            message.add_css_class("pill");
            let this = self.clone();
            message.connect_clicked(move |_| {
                let on_message = this.on_message.clone();
                let user_id = this.user_id.clone();
                this.dialog.close();
                on_message(user_id);
            });
            buttons.append(&message);
        }
        let copy_id = adw::ButtonRow::new();
        copy_id.set_title(&gettext("Copy Member ID"));
        copy_id.set_start_icon_name(Some("edit-copy-symbolic"));
        let this = self.clone();
        copy_id.connect_activated(move |_| this.copy(&this.user_id, &gettext("Member ID")));
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        list.set_size_request(260, -1);
        list.append(&copy_id);
        buttons.append(&list);
        buttons
    }
}

fn centered_label(text: &str, classes: &[&str]) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_wrap(true);
    label.set_justify(gtk::Justification::Center);
    label.set_halign(gtk::Align::Center);
    for class in classes {
        label.add_css_class(class);
    }
    label
}
