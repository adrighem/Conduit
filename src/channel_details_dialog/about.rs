//! About tab: topic, description and creator rows, channel ID, text editing.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;

use super::{current_for, Callbacks, DetailsInput, Inner};
use crate::channel_details::created_by_text;
use crate::runtime_mailbox::ConversationTextField;

pub(super) struct AboutWidgets {
    pub(super) topic: adw::ActionRow,
    pub(super) purpose: adw::ActionRow,
    pub(super) created: adw::ActionRow,
    pub(super) edit_buttons: Vec<gtk::Button>,
    /// Raw mrkdwn, so editing keeps `:emoji:` shortcodes intact.
    pub(super) raw_topic: Rc<RefCell<String>>,
    pub(super) raw_purpose: Rc<RefCell<String>>,
    pub(super) creator_id: Rc<RefCell<Option<String>>>,
}

fn expand_action_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    row.set_use_markup(false);
    row.set_title(title);
    row.set_subtitle_selectable(true);
    row.add_css_class("property");
    row
}

pub(super) fn build_about(
    input: &DetailsInput,
    callbacks: &Callbacks,
) -> (adw::PreferencesPage, AboutWidgets) {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    let topic = expand_action_row(&gettext("Topic"));
    let purpose = expand_action_row(&gettext("Description"));
    let created = expand_action_row(&gettext("Created by"));
    let mut edit_buttons = Vec::new();
    let raw_topic: Rc<RefCell<String>> = Rc::default();
    let raw_purpose: Rc<RefCell<String>> = Rc::default();
    let creator_id: Rc<RefCell<Option<String>>> = Rc::default();
    let editable = [
        (
            &topic,
            ConversationTextField::Topic,
            gettext("Edit topic"),
            &raw_topic,
        ),
        (
            &purpose,
            ConversationTextField::Purpose,
            gettext("Edit description"),
            &raw_purpose,
        ),
    ];
    for (row, field, label, raw) in editable {
        let button = gtk::Button::from_icon_name("document-edit-symbolic");
        button.add_css_class("flat");
        button.set_valign(gtk::Align::Center);
        button.set_tooltip_text(Some(&label));
        button.update_property(&[gtk::accessible::Property::Label(&label)]);
        row.add_suffix(&button);
        edit_buttons.push(button.clone());
        let channel_id = input.channel_id.clone();
        let callbacks = callbacks.clone();
        let row_title = row.title().to_string();
        let raw = raw.clone();
        button.connect_clicked(move |button| {
            let current = raw.borrow().clone();
            edit_text_dialog(button, &row_title, &current, &callbacks, &channel_id, field);
        });
    }
    group.add(&topic);
    group.add(&purpose);
    group.add(&created);
    page.add(&group);
    {
        let creator_id = creator_id.clone();
        let profile = callbacks.on_profile.clone();
        created.connect_activated(move |_| {
            if let Some(id) = creator_id.borrow().clone() {
                profile(id);
            }
        });
    }

    let footer = adw::PreferencesGroup::new();
    let id_row = expand_action_row(&gettext("Channel ID"));
    id_row.set_subtitle(&input.channel_id);
    let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
    copy.add_css_class("flat");
    copy.set_valign(gtk::Align::Center);
    let tooltip = gettext("Copy channel ID");
    copy.set_tooltip_text(Some(&tooltip));
    copy.update_property(&[gtk::accessible::Property::Label(&tooltip)]);
    id_row.add_suffix(&copy);
    footer.add(&id_row);
    page.add(&footer);
    let channel_id = input.channel_id.clone();
    copy.connect_clicked(move |button| {
        button.clipboard().set_text(&channel_id);
        if let Some(inner) = current_for(&channel_id) {
            inner
                .toasts
                .add_toast(adw::Toast::new(&gettext("Channel ID copied")));
        }
    });
    (
        page,
        AboutWidgets {
            topic,
            purpose,
            created,
            edit_buttons,
            raw_topic,
            raw_purpose,
            creator_id,
        },
    )
}

pub(super) fn edit_text_dialog(
    anchor: &gtk::Button,
    label: &str,
    initial: &str,
    callbacks: &Callbacks,
    channel_id: &str,
    field: ConversationTextField,
) {
    let heading = gettext("Edit {field}").replace("{field}", &label.to_lowercase());
    let alert = adw::AlertDialog::new(Some(&heading), None);
    let entry = gtk::Entry::new();
    entry.set_text(initial);
    entry.set_max_length(250);
    entry.set_activates_default(true);
    entry.update_property(&[gtk::accessible::Property::Label(label)]);
    alert.set_extra_child(Some(&entry));
    alert.add_response("cancel", &gettext("Cancel"));
    alert.add_response("save", &gettext("Save"));
    alert.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    alert.set_default_response(Some("save"));
    alert.set_close_response("cancel");
    let callbacks = callbacks.clone();
    let channel_id = channel_id.to_string();
    let initial = initial.to_string();
    alert.connect_response(Some("save"), move |_, _| {
        let text = entry.text().trim().to_string();
        if text == initial.trim() {
            return;
        }
        if let Some(inner) = current_for(&channel_id) {
            if let Some(about) = &inner.about {
                about
                    .edit_buttons
                    .iter()
                    .for_each(|b| b.set_sensitive(false));
            }
        }
        (callbacks.on_edit)(channel_id.clone(), field, text);
    });
    alert.present(Some(anchor));
}

impl Inner {
    pub(super) fn update_about(&self, about: &AboutWidgets, input: &DetailsInput) {
        let plain = &self.callbacks.plain_text;
        for (row, raw, store, empty) in [
            (
                &about.topic,
                &input.about.topic,
                &about.raw_topic,
                gettext("Add a topic"),
            ),
            (
                &about.purpose,
                &input.about.purpose,
                &about.raw_purpose,
                gettext("Add a description"),
            ),
        ] {
            *store.borrow_mut() = raw.clone();
            let text = plain(raw);
            row.set_subtitle(if text.is_empty() { &empty } else { &text });
            if text.is_empty() {
                row.add_css_class("dim-label");
            } else {
                row.remove_css_class("dim-label");
            }
        }
        about
            .edit_buttons
            .iter()
            .for_each(|b| b.set_sensitive(true));

        let creator_name = input
            .about
            .creator_id
            .as_deref()
            .map(|id| (self.callbacks.user_name)(id).unwrap_or_else(|| id.to_string()));
        let text = created_by_text(
            creator_name.as_deref(),
            input.about.created,
            input.utc_offset_secs,
        );
        match text {
            Some(text) => {
                about.created.set_subtitle(&text);
                about.created.set_visible(true);
            }
            None if input.loaded => about.created.set_visible(false),
            None => about.created.set_subtitle(&gettext("Loading...")),
        }
        *about.creator_id.borrow_mut() = input.about.creator_id.clone();
        about
            .created
            .set_activatable(input.about.creator_id.is_some());
    }
}
