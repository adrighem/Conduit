/* status_dialog.rs
 *
 * Copyright 2026 Vincent van Adrighem
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use adw::prelude::*;
use gettextrs::gettext;
use gtk::{gio, glib};

use crate::emoji::{
    EmojiCatalog, EmojiEntry, EmojiPickerModel, EmojiPickerQuery, EmojiPickerResult,
    EmojiPickerResultEntry, EmojiPickerResultValueKind, EmojiValue, EMOJI_PICKER_CATEGORIES,
    EMOJI_PICKER_MAX_QUERY_CHARS, EMOJI_PICKER_PROTOCOL_VERSION, EMOJI_PICKER_RESULT_LIMIT,
};
use crate::models::SlackUserStatus;

#[derive(Debug, Clone)]
pub(crate) struct StatusDialogState {
    pub(crate) dialog: adw::AlertDialog,
    pub(crate) status_entry: adw::EntryRow,
    pub(crate) emoji_picker: StatusEmojiPicker,
    pub(crate) expiration_choice_count: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingStatusUpdate {
    pub(crate) requested: SlackUserStatus,
    pub(crate) dialog_draft: SlackUserStatus,
    pub(crate) clearing: bool,
}

impl PendingStatusUpdate {
    #[allow(dead_code)]
    pub(crate) fn new(
        requested: SlackUserStatus,
        dialog_draft: SlackUserStatus,
        clearing: bool,
    ) -> Self {
        Self {
            requested,
            dialog_draft,
            clearing,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusExpirationChoice {
    Never,
    Minutes30,
    Hour1,
    Hours4,
    Today,
    ThisWeek,
    Existing(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserStatusPresentation {
    pub(crate) subtitle: String,
    pub(crate) accessible_text: String,
}

pub(crate) type StatusEmojiChoiceHandler = Rc<dyn Fn(Option<EmojiPickerResultEntry>)>;

#[derive(Debug, Clone)]
pub(crate) struct StatusEmojiPickerModel {
    emojis: EmojiPickerModel,
}

impl StatusEmojiPickerModel {
    pub(crate) fn new(custom_emojis: &HashMap<String, String>, selected_emoji: &str) -> Self {
        let catalog = EmojiCatalog::new(custom_emojis);
        let catalog_entries = catalog.entries();
        let workspace_names = catalog_entries
            .iter()
            .filter(|entry| entry.category == "Workspace")
            .map(|entry| entry.name.clone())
            .collect::<HashSet<_>>();
        let mut seen = HashSet::new();
        let mut entries = catalog_entries
            .into_iter()
            .filter(|entry| entry.category == "Workspace" || !workspace_names.contains(&entry.name))
            .filter(|entry| seen.insert(entry.name.clone()))
            .collect::<Vec<_>>();

        let selected_emoji = selected_emoji.trim().trim_matches(':');
        if !selected_emoji.is_empty() && seen.insert(selected_emoji.to_string()) {
            entries.push(EmojiEntry {
                name: selected_emoji.to_string(),
                label: selected_emoji.replace(['_', '-'], " "),
                category: "Current status",
                value: catalog
                    .resolve(selected_emoji)
                    .unwrap_or_else(|| EmojiValue::CustomImage(String::new())),
            });
        }

        Self {
            emojis: EmojiPickerModel::new(entries),
        }
    }

    pub(crate) fn choice_count(&self) -> usize {
        self.emojis.entries().len() + 1
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        name.is_empty() || self.emojis.entries().iter().any(|entry| entry.name == name)
    }

    pub(crate) fn selected_entry(&self, name: &str) -> Option<EmojiPickerResultEntry> {
        self.emojis
            .entries()
            .iter()
            .find(|entry| entry.name == name)
            .map(EmojiPickerResultEntry::from)
    }

    pub(crate) fn page(
        &self,
        query: &str,
        category: Option<&str>,
        offset: usize,
    ) -> EmojiPickerResult {
        self.emojis
            .query(&EmojiPickerQuery {
                version: EMOJI_PICKER_PROTOCOL_VERSION,
                generation: 1,
                query: query.chars().take(EMOJI_PICKER_MAX_QUERY_CHARS).collect(),
                category: category.map(str::to_string),
                offset,
            })
            .expect("status emoji picker creates valid bounded queries")
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StatusEmojiPickerPage {
    grid: gtk::FlowBox,
    empty_label: gtk::Label,
    category_bar: gtk::Widget,
    page_controls: gtk::Widget,
    page_status: gtk::Label,
    previous: gtk::Button,
    next: gtk::Button,
    visible_choices: Rc<RefCell<Vec<EmojiPickerResultEntry>>>,
    total: Rc<Cell<usize>>,
    has_previous: Rc<Cell<bool>>,
    has_more: Rc<Cell<bool>>,
}

impl StatusEmojiPickerPage {
    pub(crate) fn clear(&self) {
        while let Some(child) = self.grid.first_child() {
            self.grid.remove(&child);
        }
        self.visible_choices.borrow_mut().clear();
        self.total.set(0);
        self.has_previous.set(false);
        self.has_more.set(false);
        self.page_status.set_label("");
        self.page_controls.set_visible(false);
        self.empty_label.set_visible(false);
    }

    pub(crate) fn populate(
        &self,
        source: &StatusEmojiPickerModel,
        query: &str,
        category: &str,
        offset: usize,
        selected_name: &str,
    ) {
        let category = query.trim().is_empty().then_some(category);
        let result = source.page(query, category, offset);
        self.clear();
        for entry in &result.entries {
            self.grid.insert(&status_emoji_picker_choice(entry), -1);
        }
        self.visible_choices.replace(result.entries);
        self.total.set(result.total);
        self.has_previous.set(result.has_previous);
        self.has_more.set(result.has_more);
        self.previous.set_sensitive(result.has_previous);
        self.next.set_sensitive(result.has_more);
        self.category_bar.set_visible(query.trim().is_empty());
        self.empty_label
            .set_visible(self.visible_choices.borrow().is_empty());
        self.page_controls
            .set_visible(result.has_previous || result.has_more);
        let end = result.offset + self.visible_choices.borrow().len();
        let page_label = if result.total == 0 {
            String::new()
        } else {
            format!("{}-{end} / {}", result.offset + 1, result.total)
        };
        self.page_status.set_label(&page_label);
        self.grid.unselect_all();
        if let Some(index) = self
            .visible_choices
            .borrow()
            .iter()
            .position(|choice| choice.name == selected_name)
        {
            if let Some(child) = self.grid.child_at_index(index as i32) {
                self.grid.select_child(&child);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StatusEmojiPicker {
    pub(crate) row: adw::ActionRow,
    pub(crate) selected_preview: gtk::Box,
    pub(crate) popover: gtk::Popover,
    pub(crate) search: gtk::SearchEntry,
    pub(crate) page: StatusEmojiPickerPage,
    pub(crate) source: Rc<RefCell<StatusEmojiPickerModel>>,
    pub(crate) selected_name: Rc<RefCell<String>>,
    pub(crate) active_category: Rc<RefCell<String>>,
    pub(crate) offset: Rc<Cell<usize>>,
    pub(crate) category_count: usize,
}

impl StatusEmojiPicker {
    pub(crate) fn new(
        custom_emojis: &HashMap<String, String>,
        selected_emoji: &str,
        on_selected: impl Fn(&str) + 'static,
    ) -> Self {
        Self::new_with_options(
            custom_emojis,
            selected_emoji,
            Some(gettext("No emoji")),
            gettext("Choose a status emoji"),
            on_selected,
        )
    }

    pub(crate) fn new_for_composer(
        custom_emojis: &HashMap<String, String>,
        on_selected: impl Fn(&str) + 'static,
    ) -> Self {
        Self::new_with_options(
            custom_emojis,
            "",
            None,
            gettext("Insert emoji"),
            on_selected,
        )
    }

    pub(crate) fn new_with_options(
        custom_emojis: &HashMap<String, String>,
        selected_emoji: &str,
        clear_label: Option<String>,
        tooltip: String,
        on_selected: impl Fn(&str) + 'static,
    ) -> Self {
        let selected_name = Rc::new(RefCell::new(
            selected_emoji.trim().trim_matches(':').to_string(),
        ));
        let source = Rc::new(RefCell::new(StatusEmojiPickerModel::new(
            custom_emojis,
            selected_emoji,
        )));
        let visible_choices = Rc::new(RefCell::new(Vec::new()));
        let active_category = Rc::new(RefCell::new(EMOJI_PICKER_CATEGORIES[0].to_string()));
        let offset = Rc::new(Cell::new(0_usize));

        let grid = gtk::FlowBox::new();
        grid.set_activate_on_single_click(true);
        grid.set_column_spacing(4);
        grid.set_row_spacing(4);
        grid.set_homogeneous(true);
        grid.set_min_children_per_line(6);
        grid.set_max_children_per_line(8);
        grid.set_selection_mode(gtk::SelectionMode::Single);
        grid.update_property(&[gtk::accessible::Property::Label(&gettext(
            "Status emoji choices",
        ))]);

        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some(&gettext("Search emoji")));
        search.update_property(&[gtk::accessible::Property::Label(&gettext("Search emoji"))]);
        search.set_key_capture_widget(Some(&grid));

        let category_box = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        let mut category_buttons = Vec::new();
        let mut first_category_button: Option<gtk::ToggleButton> = None;
        for category in EMOJI_PICKER_CATEGORIES {
            let category_button = gtk::ToggleButton::with_label(category);
            category_button.add_css_class("flat");
            if let Some(first) = first_category_button.as_ref() {
                category_button.set_group(Some(first));
            } else {
                category_button.set_active(true);
                first_category_button = Some(category_button.clone());
            }
            category_box.append(&category_button);
            category_buttons.push(((*category).to_string(), category_button));
        }
        let category_scroller = gtk::ScrolledWindow::new();
        category_scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
        category_scroller.set_child(Some(&category_box));

        let empty_label = gtk::Label::new(Some(&gettext("No emoji found")));
        empty_label.add_css_class("dim-label");
        empty_label.set_margin_top(16);
        empty_label.set_margin_bottom(16);
        empty_label.set_visible(false);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_min_content_width(420);
        scroller.set_min_content_height(280);
        scroller.set_max_content_height(360);
        scroller.set_propagate_natural_height(true);
        scroller.set_child(Some(&grid));

        let previous = gtk::Button::with_label(&gettext("Previous"));
        previous.add_css_class("flat");
        let page_status = gtk::Label::new(None);
        page_status.set_hexpand(true);
        page_status.add_css_class("dim-label");
        let next = gtk::Button::with_label(&gettext("Next"));
        next.add_css_class("flat");
        let page_controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        page_controls.append(&previous);
        page_controls.append(&page_status);
        page_controls.append(&next);
        page_controls.set_visible(false);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let heading = gtk::Label::new(Some(&gettext("Choose emoji")));
        heading.add_css_class("heading");
        heading.set_xalign(0.0);
        heading.set_hexpand(true);
        header.append(&heading);
        let clear_button = clear_label.map(|label| {
            let button = gtk::Button::with_label(&label);
            button.add_css_class("flat");
            header.append(&button);
            button
        });

        let picker_content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        picker_content.set_size_request(480, -1);
        picker_content.set_margin_top(8);
        picker_content.set_margin_bottom(8);
        picker_content.set_margin_start(8);
        picker_content.set_margin_end(8);
        picker_content.append(&header);
        picker_content.append(&search);
        picker_content.append(&category_scroller);
        picker_content.append(&scroller);
        picker_content.append(&empty_label);
        picker_content.append(&page_controls);

        let popover = gtk::Popover::new();
        popover.set_autohide(true);
        popover.set_position(gtk::PositionType::Left);
        popover.set_child(Some(&picker_content));

        let button = gtk::MenuButton::new();
        button.set_direction(gtk::ArrowType::Left);
        button.set_icon_name("pan-down-symbolic");
        button.set_popover(Some(&popover));
        button.set_tooltip_text(Some(&tooltip));
        button.set_valign(gtk::Align::Center);
        button.update_property(&[gtk::accessible::Property::Label(&tooltip)]);

        let row = adw::ActionRow::builder().title(gettext("Emoji")).build();
        let selected_preview = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        selected_preview.set_valign(gtk::Align::Center);
        row.add_prefix(&selected_preview);
        row.add_suffix(&button);
        row.set_activatable_widget(Some(&button));
        let selection = source.borrow().selected_entry(&selected_name.borrow());
        update_status_emoji_selected_preview(&selected_preview, &row, selection.as_ref());

        let page = StatusEmojiPickerPage {
            grid: grid.clone(),
            empty_label,
            category_bar: category_scroller.upcast(),
            page_controls: page_controls.upcast(),
            page_status,
            previous: previous.clone(),
            next: next.clone(),
            visible_choices: visible_choices.clone(),
            total: Rc::new(Cell::new(0)),
            has_previous: Rc::new(Cell::new(false)),
            has_more: Rc::new(Cell::new(false)),
        };

        {
            let source = source.clone();
            let page = page.clone();
            let selected_name = selected_name.clone();
            let active_category = active_category.clone();
            let offset = offset.clone();
            let weak_popover = popover.downgrade();
            search.connect_search_changed(move |search| {
                if !weak_popover
                    .upgrade()
                    .is_some_and(|popover| popover.is_visible())
                {
                    return;
                }
                offset.set(0);
                page.populate(
                    &source.borrow(),
                    search.text().as_str(),
                    &active_category.borrow(),
                    0,
                    &selected_name.borrow(),
                );
            });
        }

        let on_selected: Rc<dyn Fn(&str)> = Rc::new(on_selected);
        let select_choice: StatusEmojiChoiceHandler = {
            let selected_name = selected_name.clone();
            let weak_preview = selected_preview.downgrade();
            let weak_row = row.downgrade();
            let weak_popover = popover.downgrade();
            let weak_search = search.downgrade();
            let on_selected = on_selected.clone();
            Rc::new(move |selection| {
                let name = selection
                    .as_ref()
                    .map(|selection| selection.name.as_str())
                    .unwrap_or_default();
                selected_name.replace(name.to_string());
                if let (Some(preview), Some(row)) = (weak_preview.upgrade(), weak_row.upgrade()) {
                    update_status_emoji_selected_preview(&preview, &row, selection.as_ref());
                }
                on_selected(name);
                if let Some(search) = weak_search.upgrade() {
                    search.set_text("");
                }
                if let Some(popover) = weak_popover.upgrade() {
                    popover.popdown();
                }
            })
        };

        {
            let visible_choices = visible_choices.clone();
            let select_choice = select_choice.clone();
            grid.connect_child_activated(move |_, child| {
                let Some(choice) = visible_choices
                    .borrow()
                    .get(child.index() as usize)
                    .cloned()
                else {
                    return;
                };
                select_choice(Some(choice));
            });
        }

        {
            let visible_choices = visible_choices.clone();
            let select_choice = select_choice.clone();
            search.connect_activate(move |search| {
                if search.text().trim().is_empty() {
                    return;
                }
                let Some(choice) = visible_choices.borrow().first().cloned() else {
                    return;
                };
                select_choice(Some(choice));
            });
        }

        {
            let weak_grid = grid.downgrade();
            let controller = gtk::EventControllerKey::new();
            controller.connect_key_pressed(move |_, key, _, _| {
                if key != gtk::gdk::Key::Down {
                    return glib::Propagation::Proceed;
                }
                if let Some(grid) = weak_grid.upgrade() {
                    if let Some(child) = grid.child_at_index(0) {
                        grid.select_child(&child);
                        child.grab_focus();
                    }
                }
                glib::Propagation::Stop
            });
            search.add_controller(controller);
        }

        {
            let weak_search = search.downgrade();
            let source = source.clone();
            let page = page.clone();
            let active_category = active_category.clone();
            let selected_name = selected_name.clone();
            let offset = offset.clone();
            popover.connect_visible_notify(move |popover| {
                if popover.is_visible() {
                    if let Some(search) = weak_search.upgrade() {
                        offset.set(0);
                        page.populate(
                            &source.borrow(),
                            search.text().as_str(),
                            &active_category.borrow(),
                            0,
                            &selected_name.borrow(),
                        );
                        search.grab_focus();
                    }
                }
            });
        }

        for (category, category_button) in category_buttons {
            let source = source.clone();
            let page = page.clone();
            let search = search.clone();
            let active_category = active_category.clone();
            let selected_name = selected_name.clone();
            let offset = offset.clone();
            category_button.connect_toggled(move |button| {
                if !button.is_active() {
                    return;
                }
                active_category.replace(category.clone());
                offset.set(0);
                if search.text().is_empty() {
                    page.populate(
                        &source.borrow(),
                        "",
                        &active_category.borrow(),
                        0,
                        &selected_name.borrow(),
                    );
                } else {
                    search.set_text("");
                }
            });
        }

        {
            let source = source.clone();
            let page = page.clone();
            let search = search.clone();
            let active_category = active_category.clone();
            let selected_name = selected_name.clone();
            let offset = offset.clone();
            previous.connect_clicked(move |_| {
                if !page.has_previous.get() {
                    return;
                }
                let next_offset = offset.get().saturating_sub(EMOJI_PICKER_RESULT_LIMIT);
                offset.set(next_offset);
                page.populate(
                    &source.borrow(),
                    search.text().as_str(),
                    &active_category.borrow(),
                    next_offset,
                    &selected_name.borrow(),
                );
            });
        }

        {
            let source = source.clone();
            let page = page.clone();
            let search = search.clone();
            let active_category = active_category.clone();
            let selected_name = selected_name.clone();
            let offset = offset.clone();
            next.connect_clicked(move |_| {
                if !page.has_more.get() {
                    return;
                }
                let next_offset = offset.get() + page.visible_choices.borrow().len();
                offset.set(next_offset);
                page.populate(
                    &source.borrow(),
                    search.text().as_str(),
                    &active_category.borrow(),
                    next_offset,
                    &selected_name.borrow(),
                );
            });
        }

        if let Some(clear_button) = clear_button {
            let select_choice = select_choice.clone();
            clear_button.connect_clicked(move |_| select_choice(None));
        }

        {
            let weak_popover = popover.downgrade();
            search.connect_stop_search(move |_| {
                if let Some(popover) = weak_popover.upgrade() {
                    popover.popdown();
                }
            });
        }

        {
            let weak_search = search.downgrade();
            let page = page.clone();
            let offset = offset.clone();
            popover.connect_closed(move |_| {
                page.clear();
                offset.set(0);
                if let Some(search) = weak_search.upgrade() {
                    search.set_text("");
                }
            });
        }

        if let Some(query) = std::env::var_os("CONDUIT_TEST_STATUS_EMOJI_QUERY") {
            let query = query.to_string_lossy();
            search.set_text(&query);
            search.emit_by_name::<()>("search-changed", &[]);
        }

        Self {
            row,
            selected_preview,
            popover,
            search,
            page,
            source,
            selected_name,
            active_category,
            offset,
            category_count: EMOJI_PICKER_CATEGORIES.len(),
        }
    }

    pub(crate) fn selected_name(&self) -> String {
        self.selected_name.borrow().clone()
    }

    pub(crate) fn selected_name_state(&self) -> Rc<RefCell<String>> {
        self.selected_name.clone()
    }

    pub(crate) fn source_choice_count(&self) -> usize {
        self.source.borrow().choice_count()
    }

    pub(crate) fn visible_choice_count(&self) -> u32 {
        self.page.visible_choices.borrow().len() as u32
    }

    pub(crate) fn first_visible_name(&self) -> Option<String> {
        self.page
            .visible_choices
            .borrow()
            .first()
            .map(|choice| choice.name.clone())
    }

    pub(crate) fn selected_visible_name(&self) -> Option<String> {
        let selected = self.page.grid.selected_children().first()?.index();
        self.page
            .visible_choices
            .borrow()
            .get(selected as usize)
            .map(|choice| choice.name.clone())
    }

    pub(crate) fn selected_summary_kind(&self) -> &'static str {
        match self.selected_preview.first_child() {
            Some(child) if child.is::<gtk::Picture>() => "custom-image",
            Some(child) if child.is::<gtk::Label>() => "unicode",
            _ => "text",
        }
    }

    pub(crate) fn category_count(&self) -> usize {
        self.category_count
    }

    pub(crate) fn page_total(&self) -> usize {
        self.page.total.get()
    }

    pub(crate) fn active_category(&self) -> String {
        self.active_category.borrow().clone()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.source.borrow().contains(name)
    }

    pub(crate) fn refresh_catalog(&self, custom_emojis: &HashMap<String, String>) {
        let selected_name = self.selected_name();
        self.source
            .replace(StatusEmojiPickerModel::new(custom_emojis, &selected_name));
        let selection = self.source.borrow().selected_entry(&selected_name);
        update_status_emoji_selected_preview(&self.selected_preview, &self.row, selection.as_ref());
        if self.popover.is_visible() {
            self.page.populate(
                &self.source.borrow(),
                self.search.text().as_str(),
                &self.active_category.borrow(),
                self.offset.get(),
                &selected_name,
            );
        }
    }
}

pub(crate) fn status_expiration_options(
    existing_expiration: i64,
    now: i64,
) -> (Vec<String>, Vec<StatusExpirationChoice>, u32) {
    let mut labels = vec![
        gettext("Don't clear"),
        gettext("30 minutes"),
        gettext("1 hour"),
        gettext("4 hours"),
        gettext("End of today"),
        gettext("End of this week"),
    ];
    let mut choices = vec![
        StatusExpirationChoice::Never,
        StatusExpirationChoice::Minutes30,
        StatusExpirationChoice::Hour1,
        StatusExpirationChoice::Hours4,
        StatusExpirationChoice::Today,
        StatusExpirationChoice::ThisWeek,
    ];
    let selected = if existing_expiration > now {
        let formatted = glib::DateTime::from_unix_local(existing_expiration)
            .ok()
            .and_then(|date_time| date_time.format("%a %H:%M").ok())
            .map(|date_time| date_time.to_string())
            .unwrap_or_else(|| existing_expiration.to_string());
        labels.push(
            gettext("Keep current clear time ({time})").replace("{time}", formatted.as_str()),
        );
        choices.push(StatusExpirationChoice::Existing(existing_expiration));
        choices.len() - 1
    } else {
        0
    };
    (labels, choices, selected as u32)
}

pub(crate) fn update_status_dialog_save_response(
    dialog: &adw::AlertDialog,
    status_entry: &adw::EntryRow,
    selected_emoji: &str,
) {
    dialog.set_response_enabled(
        "save",
        !status_entry.text().trim().is_empty() || !selected_emoji.is_empty(),
    );
}

pub(crate) fn status_dialog_clear_available(
    status: &SlackUserStatus,
    now: i64,
    clearing_retry: bool,
) -> bool {
    clearing_retry || status.active_at(now)
}

pub(crate) fn enforce_status_text_limit(status_entry: &adw::EntryRow) {
    let text = status_entry.text();
    if text.chars().count() <= 100 {
        return;
    }
    let limited = text.chars().take(100).collect::<String>();
    status_entry.set_text(&limited);
    status_entry.set_position(-1);
}

pub(crate) fn nearest_status_expiration(
    statuses: &HashMap<String, SlackUserStatus>,
    now: i64,
) -> Option<i64> {
    statuses
        .values()
        .map(|status| status.expiration)
        .filter(|expiration| *expiration > now)
        .min()
}

pub(crate) fn status_expiration_for_choice(
    choice: StatusExpirationChoice,
    now: i64,
    end_today: i64,
    end_week: i64,
) -> i64 {
    match choice {
        StatusExpirationChoice::Never => 0,
        StatusExpirationChoice::Minutes30 => now.saturating_add(30 * 60),
        StatusExpirationChoice::Hour1 => now.saturating_add(60 * 60),
        StatusExpirationChoice::Hours4 => now.saturating_add(4 * 60 * 60),
        StatusExpirationChoice::Today => end_today,
        StatusExpirationChoice::ThisWeek => end_week,
        StatusExpirationChoice::Existing(expiration) => expiration,
    }
}

pub(crate) fn status_from_dialog_input(
    text: &str,
    emoji: &str,
    expiration_choice: StatusExpirationChoice,
    now: i64,
    end_today: i64,
    end_week: i64,
) -> SlackUserStatus {
    SlackUserStatus {
        text: text.trim().chars().take(100).collect(),
        emoji: emoji.trim().trim_matches(':').to_string(),
        expiration: status_expiration_for_choice(expiration_choice, now, end_today, end_week),
    }
}

pub(crate) fn status_expiration_boundaries(now: i64) -> (i64, i64) {
    let fallback = (
        now.saturating_add(24 * 60 * 60),
        now.saturating_add(7 * 24 * 60 * 60),
    );
    let Ok(local) = glib::DateTime::now_local() else {
        return fallback;
    };
    let Ok(end_today) = glib::DateTime::from_local(
        local.year(),
        local.month(),
        local.day_of_month(),
        23,
        59,
        59.0,
    ) else {
        return fallback;
    };
    let Ok(end_week_date) = local.add_days(7_i32.saturating_sub(local.day_of_week())) else {
        return (end_today.to_unix(), fallback.1);
    };
    let Ok(end_week) = glib::DateTime::from_local(
        end_week_date.year(),
        end_week_date.month(),
        end_week_date.day_of_month(),
        23,
        59,
        59.0,
    ) else {
        return (end_today.to_unix(), fallback.1);
    };
    (end_today.to_unix(), end_week.to_unix())
}

pub(crate) fn user_status_presentation(
    status: &SlackUserStatus,
    custom_emojis: &HashMap<String, String>,
    now: i64,
) -> Option<UserStatusPresentation> {
    if !status.active_at(now) {
        return None;
    }
    let text = status.text.trim();
    let emoji = (!status.emoji_name().is_empty()).then(|| {
        EmojiCatalog::new(custom_emojis)
            .resolve(status.emoji_name())
            .and_then(|value| match value {
                EmojiValue::Unicode(glyph) => Some(glyph.to_string()),
                EmojiValue::CustomImage(_) => None,
            })
            .unwrap_or_else(|| "●".to_string())
    });
    let subtitle = match (emoji.as_deref(), text.is_empty()) {
        (Some(emoji), false) => format!("{emoji} {text}"),
        (Some(emoji), true) => emoji.to_string(),
        (None, false) => text.to_string(),
        (None, true) => return None,
    };
    Some(UserStatusPresentation {
        subtitle,
        accessible_text: status.accessible_text(),
    })
}

pub(crate) fn status_emoji_result_label(entry: &EmojiPickerResultEntry) -> String {
    match entry.value_kind {
        EmojiPickerResultValueKind::Unicode => {
            format!("{} :{}: - {}", entry.value, entry.name, entry.label)
        }
        EmojiPickerResultValueKind::CustomImage => {
            format!(":{}: - {}", entry.name, entry.label)
        }
    }
}

pub(crate) fn update_status_emoji_selected_preview(
    preview: &gtk::Box,
    row: &adw::ActionRow,
    selection: Option<&EmojiPickerResultEntry>,
) {
    while let Some(child) = preview.first_child() {
        preview.remove(&child);
    }

    let Some(selection) = selection else {
        preview.set_visible(false);
        row.set_subtitle(&gettext("No emoji"));
        return;
    };
    let visual: gtk::Widget = match selection.value_kind {
        EmojiPickerResultValueKind::Unicode => {
            let label = gtk::Label::new(Some(&selection.value));
            label.add_css_class("title-3");
            label.update_property(&[gtk::accessible::Property::Label(
                &selection.accessible_label,
            )]);
            label.upcast()
        }
        EmojiPickerResultValueKind::CustomImage
            if selection.value.starts_with("https://")
                || selection.value.starts_with("http://") =>
        {
            status_emoji_custom_picture(&selection.value, &selection.accessible_label).upcast()
        }
        EmojiPickerResultValueKind::CustomImage => {
            preview.set_visible(false);
            row.set_subtitle(&status_emoji_result_label(selection));
            return;
        }
    };
    preview.append(&visual);
    preview.set_visible(true);
    row.set_subtitle(&format!("- {}", selection.label));
}

fn record_test_status_emoji_animation_frame() {
    let Some(path) = std::env::var_os("CONDUIT_TEST_STATUS_ANIMATION_FILE") else {
        return;
    };
    let frame_updates = std::fs::read_to_string(&path)
        .ok()
        .and_then(|state| serde_json::from_str::<serde_json::Value>(&state).ok())
        .and_then(|state| state.get("frame_updates")?.as_u64())
        .unwrap_or_default()
        + 1;
    let _ = std::fs::write(
        path,
        serde_json::json!({ "frame_updates": frame_updates }).to_string(),
    );
}

fn record_test_status_emoji_animation_error(stage: &str) {
    let Some(path) = std::env::var_os("CONDUIT_TEST_STATUS_ANIMATION_FILE") else {
        return;
    };
    let _ = std::fs::write(
        path,
        serde_json::json!({ "error": stage, "frame_updates": 0 }).to_string(),
    );
}

fn set_status_emoji_animation_frame(
    picture: &gtk::Picture,
    animation: &gdk_pixbuf::PixbufAnimationIter,
) {
    picture.set_paintable(Some(&gtk::gdk::Texture::for_pixbuf(&animation.pixbuf())));
    record_test_status_emoji_animation_frame();
}

fn schedule_status_emoji_animation_frame(
    weak_picture: glib::WeakRef<gtk::Picture>,
    animation: Rc<gdk_pixbuf::PixbufAnimationIter>,
) {
    let delay = animation
        .delay_time()
        .filter(|delay| !delay.is_zero())
        .unwrap_or(Duration::from_millis(100))
        .max(Duration::from_millis(16));
    glib::timeout_add_local_once(delay, move || {
        let Some(picture) = weak_picture.upgrade() else {
            return;
        };
        if animation.advance(SystemTime::now()) {
            set_status_emoji_animation_frame(&picture, &animation);
        }
        schedule_status_emoji_animation_frame(weak_picture, animation);
    });
}

pub(crate) fn status_emoji_custom_picture(url: &str, label: &str) -> gtk::Picture {
    let picture = gtk::Picture::new();
    picture.set_alternative_text(Some(label));
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_size_request(30, 30);

    let weak_picture = picture.downgrade();
    let file = std::env::var_os("CONDUIT_TEST_STATUS_EMOJI_FILE")
        .map(gio::File::for_path)
        .unwrap_or_else(|| gio::File::for_uri(url));
    file.read_async(
        glib::Priority::DEFAULT,
        gio::Cancellable::NONE,
        move |stream| {
            let Ok(stream) = stream else {
                record_test_status_emoji_animation_error("open");
                return;
            };
            let weak_picture = weak_picture.clone();
            gdk_pixbuf::PixbufAnimation::from_stream_async(
                &stream,
                gio::Cancellable::NONE,
                move |animation| {
                    let Some(picture) = weak_picture.upgrade() else {
                        return;
                    };
                    let Ok(animation) = animation else {
                        record_test_status_emoji_animation_error("decode");
                        return;
                    };
                    let frame = Rc::new(animation.iter(Some(SystemTime::now())));
                    set_status_emoji_animation_frame(&picture, &frame);
                    if !animation.is_static_image() {
                        schedule_status_emoji_animation_frame(weak_picture, frame);
                    }
                },
            );
        },
    );
    picture
}

pub(crate) fn status_emoji_picker_choice(entry: &EmojiPickerResultEntry) -> gtk::FlowBoxChild {
    let child = gtk::FlowBoxChild::new();
    child.set_tooltip_text(Some(&format!(":{}:", entry.name)));
    child.update_property(&[gtk::accessible::Property::Label(&entry.accessible_label)]);
    let content: gtk::Widget = match entry.value_kind {
        EmojiPickerResultValueKind::Unicode => {
            let label = gtk::Label::new(Some(&entry.value));
            label.add_css_class("title-3");
            label.upcast()
        }
        EmojiPickerResultValueKind::CustomImage
            if entry.value.starts_with("https://") || entry.value.starts_with("http://") =>
        {
            status_emoji_custom_picture(&entry.value, &entry.label).upcast()
        }
        EmojiPickerResultValueKind::CustomImage => {
            let label = gtk::Label::new(Some(&format!(":{}:", entry.name)));
            label.upcast()
        }
    };
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(6);
    content.set_margin_end(6);
    child.set_child(Some(&content));
    child
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_expiration_choices_resolve_to_absolute_slack_timestamps() {
        let now = 1_000;

        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::Never, now, 2_000, 7_000),
            0
        );
        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::Minutes30, now, 2_000, 7_000),
            2_800
        );
        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::Hour1, now, 2_000, 7_000),
            4_600
        );
        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::Hours4, now, 2_000, 7_000),
            15_400
        );
        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::Today, now, 2_000, 7_000),
            2_000
        );
        assert_eq!(
            status_expiration_for_choice(StatusExpirationChoice::ThisWeek, now, 2_000, 7_000),
            7_000
        );
        assert_eq!(
            status_expiration_for_choice(
                StatusExpirationChoice::Existing(3_500),
                now,
                2_000,
                7_000,
            ),
            3_500
        );
    }

    #[test]
    fn status_dialog_builds_text_only_and_emoji_only_statuses() {
        assert_eq!(
            status_from_dialog_input(
                " Focus time ",
                "",
                StatusExpirationChoice::Hour1,
                1_000,
                2_000,
                7_000,
            ),
            SlackUserStatus {
                text: "Focus time".to_string(),
                emoji: String::new(),
                expiration: 4_600,
            }
        );
        assert_eq!(
            status_from_dialog_input(
                "",
                ":headphones:",
                StatusExpirationChoice::Never,
                1_000,
                2_000,
                7_000,
            ),
            SlackUserStatus {
                text: String::new(),
                emoji: "headphones".to_string(),
                expiration: 0,
            }
        );
        assert_eq!(
            status_from_dialog_input(
                &"a".repeat(101),
                "",
                StatusExpirationChoice::Never,
                1_000,
                2_000,
                7_000,
            )
            .text
            .chars()
            .count(),
            100
        );
    }

    #[test]
    fn status_emoji_picker_pages_the_entire_compatible_source_by_shared_category() {
        let custom = HashMap::from([(
            "party_parrot".to_string(),
            "https://emoji.example/party-parrot.gif".to_string(),
        )]);
        let model = StatusEmojiPickerModel::new(&custom, "");
        let smileys = model.page("", Some("Smileys"), 0);

        assert_eq!(smileys.entries.len(), EMOJI_PICKER_RESULT_LIMIT);
        assert!(smileys.has_more);
        assert_eq!(smileys.offset, 0);
        let next_smileys = model.page("", Some("Smileys"), EMOJI_PICKER_RESULT_LIMIT);
        assert!(next_smileys.has_previous);
        assert_eq!(next_smileys.offset, EMOJI_PICKER_RESULT_LIMIT);
        assert_eq!(next_smileys.total, smileys.total);
        assert!(next_smileys.entries.len() <= EMOJI_PICKER_RESULT_LIMIT);
        let workspace = model.page("", Some("Workspace"), 0);
        assert!(workspace
            .entries
            .iter()
            .any(|choice| choice.name == "party_parrot"));
        assert!(model
            .page(&"x".repeat(EMOJI_PICKER_MAX_QUERY_CHARS + 1), None, 0,)
            .entries
            .is_empty());
        assert_eq!(
            model
                .page("PARTY parr", None, 0)
                .entries
                .first()
                .map(|choice| choice.name.as_str()),
            Some("party_parrot")
        );
    }

    #[test]
    fn status_emoji_picker_preserves_selection_and_prefers_workspace_collisions() {
        let selected = StatusEmojiPickerModel::new(&HashMap::new(), ":still_loading:");
        assert!(selected.contains("still_loading"));
        assert_eq!(
            selected
                .selected_entry("still_loading")
                .as_ref()
                .map(status_emoji_result_label),
            Some(":still_loading: - still loading".to_string())
        );

        let toned = StatusEmojiPickerModel::new(&HashMap::new(), ":+1::skin-tone-3:");
        assert_eq!(
            toned
                .selected_entry("+1::skin-tone-3")
                .map(|entry| (entry.value_kind, entry.value)),
            Some((EmojiPickerResultValueKind::Unicode, "👍🏼".to_string(),))
        );

        let custom = HashMap::from([(
            "rocket".to_string(),
            "https://emoji.example/custom-rocket.gif".to_string(),
        )]);
        let refreshed = StatusEmojiPickerModel::new(&custom, "still_loading");
        assert!(refreshed.contains("still_loading"));
        assert_eq!(
            refreshed
                .page("rocket", None, 0)
                .entries
                .first()
                .map(|choice| (choice.name.as_str(), choice.value_kind)),
            Some(("rocket", EmojiPickerResultValueKind::CustomImage))
        );
    }

    #[test]
    fn status_dialog_keeps_clear_available_for_a_failed_clear_retry() {
        assert!(!status_dialog_clear_available(
            &SlackUserStatus::default(),
            100,
            false
        ));
        assert!(status_dialog_clear_available(
            &SlackUserStatus::default(),
            100,
            true
        ));
        assert!(status_dialog_clear_available(
            &SlackUserStatus {
                text: "Focus".to_string(),
                ..Default::default()
            },
            100,
            false
        ));
    }

    #[test]
    fn user_status_presentation_handles_text_unicode_custom_and_expiry() {
        let custom = HashMap::from([(
            "working_remotely".to_string(),
            "https://emoji.example/remote.png".to_string(),
        )]);

        assert_eq!(
            user_status_presentation(
                &SlackUserStatus {
                    text: "Focus time".to_string(),
                    ..Default::default()
                },
                &custom,
                100,
            ),
            Some(UserStatusPresentation {
                subtitle: "Focus time".to_string(),
                accessible_text: "Focus time".to_string(),
            })
        );
        assert_eq!(
            user_status_presentation(
                &SlackUserStatus {
                    text: "Approved".to_string(),
                    emoji: ":+1::skin-tone-3:".to_string(),
                    ..Default::default()
                },
                &custom,
                100,
            ),
            Some(UserStatusPresentation {
                subtitle: "👍🏼 Approved".to_string(),
                accessible_text: "Approved".to_string(),
            })
        );
        assert_eq!(
            user_status_presentation(
                &SlackUserStatus {
                    text: "Focus time".to_string(),
                    emoji: ":headphones:".to_string(),
                    ..Default::default()
                },
                &custom,
                100,
            ),
            Some(UserStatusPresentation {
                subtitle: "🎧 Focus time".to_string(),
                accessible_text: "Focus time".to_string(),
            })
        );
        assert_eq!(
            user_status_presentation(
                &SlackUserStatus {
                    text: "Remote".to_string(),
                    emoji: ":working_remotely:".to_string(),
                    ..Default::default()
                },
                &custom,
                100,
            ),
            Some(UserStatusPresentation {
                subtitle: "● Remote".to_string(),
                accessible_text: "Remote".to_string(),
            })
        );
        assert_eq!(
            user_status_presentation(
                &SlackUserStatus {
                    text: "Expired".to_string(),
                    expiration: 100,
                    ..Default::default()
                },
                &custom,
                100,
            ),
            None
        );
    }
}
