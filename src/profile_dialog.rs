//! Native user profile dialog (AdwDialog) opened from the timeline author menu.
//!
//! The view model and its formatting are pure functions; the dialog widget
//! lives in a thread-local so that at most one profile is shown at a time and
//! runtime events can update it in place by user id.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;
use gtk::glib;

use crate::emoji::{EmojiCatalog, EmojiValue};
use crate::models::SlackUser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    Email,
    Phone,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailRow {
    pub(crate) label: String,
    pub(crate) value: String,
    pub(crate) kind: RowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusLine {
    pub(crate) text: String,
    pub(crate) expiry: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProfileViewModel {
    pub(crate) display_name: String,
    /// Real name and pronouns joined for the header, when present.
    pub(crate) identity_line: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) status: Option<StatusLine>,
    pub(crate) local_time: Option<String>,
    pub(crate) rows: Vec<DetailRow>,
}

/// Everything the window knows about the profile being shown.
pub(crate) struct ProfileInput {
    pub(crate) user: SlackUser,
    pub(crate) avatar_path: Option<PathBuf>,
    pub(crate) custom_emojis: HashMap<String, String>,
    pub(crate) can_message: bool,
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// `HH:MM` of a unix timestamp shifted by `offset_secs`.
fn clock_text(unix_secs: i64, offset_secs: i64) -> String {
    let day_secs = (unix_secs + offset_secs).rem_euclid(86_400);
    format!("{:02}:{:02}", day_secs / 3600, day_secs % 3600 / 60)
}

/// "14:05 local time" for the user's UTC offset, if Slack provided one.
pub(crate) fn local_time_text(tz_offset: Option<i64>, now: i64) -> Option<String> {
    let offset = tz_offset?;
    Some(gettext("{time} local time").replace("{time}", &clock_text(now, offset)))
}

/// "Until 15:30" for a status ending later today (in the viewer's zone), or
/// "Until <date> 15:30" for a later day. `None` when it never expires or has
/// already expired.
pub(crate) fn status_expiry_text(expiration: i64, now: i64, viewer_offset: i64) -> Option<String> {
    if expiration <= 0 || expiration <= now {
        return None;
    }
    let same_day =
        (expiration + viewer_offset).div_euclid(86_400) == (now + viewer_offset).div_euclid(86_400);
    let clock = clock_text(expiration, viewer_offset);
    let when = if same_day {
        clock
    } else {
        let date = glib::DateTime::from_unix_utc(expiration + viewer_offset)
            .ok()
            .and_then(|date| date.format("%x").ok())
            .map(|date| date.to_string())
            .unwrap_or_default();
        format!("{date} {clock}")
    };
    Some(gettext("Until {when}").replace("{when}", &when))
}

pub(crate) fn profile_view_model(
    user: &SlackUser,
    custom_emojis: &HashMap<String, String>,
    now: i64,
    viewer_offset: i64,
) -> ProfileViewModel {
    let profile = user.profile.as_ref();
    let display_name = user
        .display_name()
        .unwrap_or_else(|| gettext("Unknown person"));
    let real_name = user
        .full_name()
        .filter(|name| !name.eq_ignore_ascii_case(&display_name));
    let pronouns = non_empty(profile.and_then(|p| p.pronouns.as_deref()));
    let identity_line = match (real_name, pronouns) {
        (Some(name), Some(pronouns)) => Some(format!("{name} · {pronouns}")),
        (name, pronouns) => name.or(pronouns),
    };

    let status = user
        .status()
        .filter(|status| status.active_at(now))
        .map(|status| {
            let emoji = (!status.emoji_name().is_empty())
                .then(
                    || match EmojiCatalog::new(custom_emojis).resolve(status.emoji_name()) {
                        Some(EmojiValue::Unicode(glyph)) => glyph.to_string(),
                        _ => String::new(),
                    },
                )
                .unwrap_or_default();
            let text = [emoji.as_str(), status.text.trim()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            StatusLine {
                text: if text.is_empty() {
                    status.accessible_text()
                } else {
                    text
                },
                expiry: status_expiry_text(status.expiration, now, viewer_offset),
            }
        });

    let mut rows = Vec::new();
    let mut push = |label: String, value: Option<String>, kind: RowKind| {
        if let Some(value) = value {
            rows.push(DetailRow { label, value, kind });
        }
    };
    push(
        gettext("Email"),
        non_empty(profile.and_then(|p| p.email.as_deref())),
        RowKind::Email,
    );
    push(
        gettext("Phone"),
        non_empty(profile.and_then(|p| p.phone.as_deref())),
        RowKind::Phone,
    );
    push(
        gettext("Skype"),
        non_empty(profile.and_then(|p| p.skype.as_deref())),
        RowKind::Text,
    );
    push(
        gettext("Location"),
        non_empty(
            profile
                .and_then(|p| p.location.as_deref())
                .or(user.tz_label.as_deref()),
        ),
        RowKind::Text,
    );
    push(
        gettext("About"),
        non_empty(profile.and_then(|p| p.about.as_deref())),
        RowKind::Text,
    );
    if let Some(profile) = profile {
        let mut fields = profile.fields.iter().collect::<Vec<_>>();
        fields.sort_by_key(|(id, field)| field.label.as_deref().unwrap_or(id).to_lowercase());
        for (id, field) in fields {
            push(
                field.label.clone().unwrap_or_else(|| id.clone()),
                non_empty(field.display_value()),
                RowKind::Text,
            );
        }
    }

    ProfileViewModel {
        display_name,
        identity_line,
        title: non_empty(profile.and_then(|p| p.title.as_deref())),
        status,
        local_time: local_time_text(user.tz_offset, now),
        rows,
    }
}

#[derive(Clone)]
struct ProfileDialog {
    dialog: adw::Dialog,
    user_id: String,
    stack: gtk::Stack,
    toasts: adw::ToastOverlay,
    error_page: adw::StatusPage,
    on_message: Rc<dyn Fn(String)>,
}

thread_local! {
    static CURRENT: RefCell<Option<ProfileDialog>> = const { RefCell::new(None) };
}

fn current_for(user_id: &str) -> Option<ProfileDialog> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .filter(|dialog| dialog.user_id == user_id)
            .cloned()
    })
}

fn now_and_viewer_offset() -> (i64, i64) {
    let now = glib::DateTime::now_local().ok();
    (
        now.as_ref().map_or(0, glib::DateTime::to_unix),
        now.map_or(0, |now| now.utc_offset().as_seconds()),
    )
}

/// Presents the profile dialog for `user_id`, replacing any open one. Shows
/// `initial` right away when the user is already known, otherwise a spinner
/// until [`update`] delivers the profile.
pub(crate) fn present(
    parent: &impl IsA<gtk::Widget>,
    user_id: &str,
    initial: Option<ProfileInput>,
    on_message: Rc<dyn Fn(String)>,
) {
    let previous = CURRENT.with(|current| current.borrow_mut().take());
    if let Some(previous) = previous {
        previous.dialog.force_close();
    }

    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.update_property(&[gtk::accessible::Property::Label(&gettext(
        "Loading profile",
    ))]);
    let error_page = adw::StatusPage::new();
    error_page.set_icon_name(Some("dialog-error-symbolic"));
    error_page.set_title(&gettext("Could not load profile"));
    let stack = gtk::Stack::new();
    stack.add_named(&spinner, Some("loading"));
    stack.add_named(&error_page, Some("error"));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&toasts));
    let dialog = adw::Dialog::new();
    dialog.set_title(&gettext("Profile"));
    dialog.set_content_width(420);
    dialog.set_content_height(560);
    dialog.set_child(Some(&toolbar));

    let this = ProfileDialog {
        dialog: dialog.clone(),
        user_id: user_id.to_string(),
        stack,
        toasts,
        error_page,
        on_message,
    };
    CURRENT.with(|current| *current.borrow_mut() = Some(this.clone()));
    dialog.connect_closed(glib::clone!(
        #[weak(rename_to = closed)]
        dialog,
        move |_| {
            CURRENT.with(|current| {
                let is_current = current
                    .borrow()
                    .as_ref()
                    .is_some_and(|c| c.dialog == closed);
                if is_current {
                    current.borrow_mut().take();
                }
            });
        }
    ));
    if let Some(input) = initial {
        this.show_profile(&input);
    }
    dialog.present(Some(parent));
}

/// Replaces the content of the open dialog for `user_id` with `input`.
pub(crate) fn update(user_id: &str, input: &ProfileInput) {
    if let Some(dialog) = current_for(user_id) {
        dialog.show_profile(input);
    }
}

/// Shows an error page if the dialog for `user_id` has nothing to show yet.
pub(crate) fn show_error(user_id: &str, message: &str) {
    if let Some(dialog) = current_for(user_id) {
        if dialog.stack.visible_child_name().as_deref() == Some("loading") {
            dialog.error_page.set_description(Some(message));
            dialog.stack.set_visible_child_name("error");
        }
    }
}

impl ProfileDialog {
    fn show_profile(&self, input: &ProfileInput) {
        let (now, viewer_offset) = now_and_viewer_offset();
        let model = profile_view_model(&input.user, &input.custom_emojis, now, viewer_offset);
        let page = self.build_page(&model, input);
        if let Some(old) = self.stack.child_by_name("profile") {
            self.stack.remove(&old);
        }
        self.stack.add_named(&page, Some("profile"));
        self.stack.set_visible_child_name("profile");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{SlackProfileField, SlackUserProfile};

    const NOW: i64 = 1_700_000_000; // 2023-11-14 22:13:20 UTC

    fn user(profile: SlackUserProfile) -> SlackUser {
        SlackUser {
            id: Some("U1".into()),
            real_name: Some("Alice Doe".into()),
            profile: Some(profile),
            ..Default::default()
        }
    }

    #[test]
    fn local_time_uses_tz_offset() {
        assert_eq!(
            local_time_text(Some(3600), NOW).as_deref(),
            Some("23:13 local time")
        );
        assert_eq!(
            local_time_text(Some(-8 * 3600), NOW).as_deref(),
            Some("14:13 local time")
        );
        assert_eq!(
            local_time_text(Some(2 * 3600), NOW).as_deref(),
            Some("00:13 local time")
        );
        assert_eq!(local_time_text(None, NOW), None);
    }

    #[test]
    fn status_expiry_is_hidden_when_unset_or_past() {
        assert_eq!(status_expiry_text(0, NOW, 0), None);
        assert_eq!(status_expiry_text(NOW - 1, NOW, 0), None);
    }

    #[test]
    fn status_expiry_shows_clock_today_and_date_later() {
        assert_eq!(
            status_expiry_text(NOW + 1800, NOW, 0).as_deref(),
            Some("Until 22:43")
        );
        let later = status_expiry_text(NOW + 86_400, NOW, 0).unwrap();
        assert!(
            later.starts_with("Until ") && later.ends_with(" 22:13"),
            "{later}"
        );
        // 22:43 UTC is already the next day for a viewer at UTC+2.
        let shifted = status_expiry_text(NOW + 1800, NOW, 2 * 3600).unwrap();
        assert!(shifted.ends_with(" 00:43"), "{shifted}");
    }

    #[test]
    fn missing_fields_are_hidden() {
        let model = profile_view_model(&user(SlackUserProfile::default()), &HashMap::new(), NOW, 0);
        assert_eq!(model.display_name, "Alice Doe");
        assert_eq!(model.identity_line, None);
        assert_eq!(model.title, None);
        assert_eq!(model.status, None);
        assert_eq!(model.local_time, None);
        assert!(model.rows.is_empty());
    }

    #[test]
    fn populated_profile_formats_header_and_rows() {
        let mut profile = SlackUserProfile {
            display_name: Some("ali".into()),
            title: Some(" Engineer ".into()),
            pronouns: Some("she/her".into()),
            email: Some("a@example.com".into()),
            phone: Some("  ".into()),
            status_text: Some("Out sailing".into()),
            status_emoji: Some(":sailboat:".into()),
            status_expiration: Some(NOW + 1800),
            ..Default::default()
        };
        profile.fields.insert(
            "Xf2".into(),
            SlackProfileField {
                value: Some("v".into()),
                alt: Some("Team B".into()),
                label: Some("Squad".into()),
            },
        );
        let mut user = user(profile);
        user.tz_offset = Some(3600);
        user.tz_label = Some("Central European Time".into());
        let model = profile_view_model(&user, &HashMap::new(), NOW, 0);
        assert_eq!(model.display_name, "ali");
        assert_eq!(model.identity_line.as_deref(), Some("Alice Doe · she/her"));
        assert_eq!(model.title.as_deref(), Some("Engineer"));
        let status = model.status.unwrap();
        assert!(status.text.ends_with("Out sailing"));
        assert_eq!(status.expiry.as_deref(), Some("Until 22:43"));
        assert_eq!(model.local_time.as_deref(), Some("23:13 local time"));
        let rows: Vec<_> = model
            .rows
            .iter()
            .map(|r| (r.label.as_str(), r.value.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("Email", "a@example.com"),
                ("Location", "Central European Time"),
                ("Squad", "Team B"),
            ]
        );
        assert_eq!(model.rows[0].kind, RowKind::Email);
    }

    #[test]
    fn expired_status_is_omitted() {
        let profile = SlackUserProfile {
            status_text: Some("Lunch".into()),
            status_expiration: Some(NOW - 10),
            ..Default::default()
        };
        assert_eq!(
            profile_view_model(&user(profile), &HashMap::new(), NOW, 0).status,
            None
        );
    }
}
