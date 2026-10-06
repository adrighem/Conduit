//! Settings tab: leave channel.

use adw::prelude::*;
use gettextrs::gettext;

use super::{close, Callbacks, DetailsInput};

pub(super) fn build_settings(input: &DetailsInput, callbacks: &Callbacks) -> gtk::Widget {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    group.set_description(Some(&if input.is_private {
        gettext("You will need an invitation to rejoin this private channel.")
    } else {
        gettext("You can rejoin this channel later.")
    }));
    let leave = adw::ButtonRow::new();
    leave.set_title(&gettext("Leave channel"));
    leave.add_css_class("destructive-action");
    group.add(&leave);
    page.add(&group);

    let channel_id = input.channel_id.clone();
    let title = input.title.clone();
    let is_private = input.is_private;
    let callbacks = callbacks.clone();
    leave.connect_activated(move |button| {
        let heading = gettext("Leave {name}?").replace("{name}", &title);
        let body = if is_private {
            gettext("You won't be able to rejoin this private channel unless someone invites you again.")
        } else {
            gettext("You will stop receiving messages from this channel.")
        };
        let alert = adw::AlertDialog::new(Some(&heading), Some(&body));
        alert.add_response("cancel", &gettext("Cancel"));
        alert.add_response("leave", &gettext("Leave channel"));
        alert.set_response_appearance("leave", adw::ResponseAppearance::Destructive);
        alert.set_default_response(Some("cancel"));
        alert.set_close_response("cancel");
        let channel_id = channel_id.clone();
        let callbacks = callbacks.clone();
        alert.connect_response(Some("leave"), move |_, _| {
            (callbacks.on_leave)(channel_id.clone());
            close(&channel_id);
        });
        alert.present(Some(button));
    });
    page.upcast()
}
