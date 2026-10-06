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

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gettextrs::gettext;
use gtk::glib;

use crate::emoji::{EmojiCatalog, EmojiValue};
use crate::models::SlackUserStatus;

#[derive(Debug, Clone)]
pub(crate) struct StatusDialogState {
    pub(crate) dialog: adw::AlertDialog,
    pub(crate) status_entry: adw::EntryRow,
    pub(crate) selected_emoji: Rc<RefCell<String>>,
    pub(crate) expiration_choice_count: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingStatusUpdate {
    pub(crate) requested: SlackUserStatus,
    pub(crate) dialog_draft: SlackUserStatus,
    pub(crate) clearing: bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use crate::emoji::{
        EmojiEntry, EmojiPickerModel, EmojiPickerQuery, EmojiPickerResult,
        EmojiPickerResultEntry, EmojiPickerResultValueKind,
        EMOJI_PICKER_MAX_QUERY_CHARS, EMOJI_PICKER_PROTOCOL_VERSION, EMOJI_PICKER_RESULT_LIMIT,
    };

    #[derive(Debug, Clone)]
    struct StatusEmojiPickerModel {
        emojis: EmojiPickerModel,
    }

    impl StatusEmojiPickerModel {
        fn new(custom_emojis: &HashMap<String, String>, selected_emoji: &str) -> Self {
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

        fn contains(&self, name: &str) -> bool {
            name.is_empty() || self.emojis.entries().iter().any(|entry| entry.name == name)
        }

        fn selected_entry(&self, name: &str) -> Option<EmojiPickerResultEntry> {
            self.emojis
                .entries()
                .iter()
                .find(|entry| entry.name == name)
                .map(EmojiPickerResultEntry::from)
        }

        fn page(
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

    fn status_emoji_result_label(entry: &EmojiPickerResultEntry) -> String {
        match entry.value_kind {
            EmojiPickerResultValueKind::Unicode => {
                format!("{} :{}: - {}", entry.value, entry.name, entry.label)
            }
            EmojiPickerResultValueKind::CustomImage => {
                format!(":{}: - {}", entry.name, entry.label)
            }
        }
    }

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
