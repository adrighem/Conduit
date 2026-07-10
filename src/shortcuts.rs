/* shortcuts.rs
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionShortcut {
    pub action: &'static str,
    pub accelerators: &'static [&'static str],
}

pub const APP_SHORTCUTS: &[ActionShortcut] = &[
    ActionShortcut {
        action: "app.quit",
        accelerators: &["<control>q"],
    },
    ActionShortcut {
        action: "app.preferences",
        accelerators: &["<control>comma"],
    },
    ActionShortcut {
        action: "app.shortcuts",
        accelerators: &["<control>question"],
    },
];

pub const WINDOW_SHORTCUTS: &[ActionShortcut] = &[
    ActionShortcut {
        action: "win.switch-conversation",
        accelerators: &["<control>k"],
    },
    ActionShortcut {
        action: "win.search-workspace",
        accelerators: &["<control>f"],
    },
    ActionShortcut {
        action: "win.show-messages",
        accelerators: &["<control>1"],
    },
    ActionShortcut {
        action: "win.show-unreads",
        accelerators: &["<control>2"],
    },
    ActionShortcut {
        action: "win.show-files",
        accelerators: &["<control>3"],
    },
    ActionShortcut {
        action: "win.show-later",
        accelerators: &["<control>4"],
    },
    ActionShortcut {
        action: "win.refresh-conversations",
        accelerators: &["F5"],
    },
    ActionShortcut {
        action: "win.focus-composer",
        accelerators: &["<control>m"],
    },
    ActionShortcut {
        action: "win.upload-file",
        accelerators: &["<control>o"],
    },
    ActionShortcut {
        action: "win.close-thread",
        accelerators: &["<control><shift>w"],
    },
];

#[cfg(test)]
fn accelerators_for_action(action: &str) -> Option<&'static [&'static str]> {
    APP_SHORTCUTS
        .iter()
        .chain(WINDOW_SHORTCUTS.iter())
        .find(|shortcut| shortcut.action == action)
        .map(|shortcut| shortcut.accelerators)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn includes_workspace_search_and_switcher() {
        assert_eq!(
            accelerators_for_action("app.shortcuts").unwrap(),
            ["<control>question"]
        );
        assert_eq!(
            accelerators_for_action("win.switch-conversation").unwrap(),
            ["<control>k"]
        );
        assert_eq!(
            accelerators_for_action("win.search-workspace").unwrap(),
            ["<control>f"]
        );
        assert_eq!(
            accelerators_for_action("win.show-messages").unwrap(),
            ["<control>1"]
        );
        assert_eq!(
            accelerators_for_action("win.show-unreads").unwrap(),
            ["<control>2"]
        );
    }

    #[test]
    fn actions_and_accelerators_are_unique() {
        let mut actions = HashSet::new();
        let mut accelerators = HashSet::new();

        for shortcut in APP_SHORTCUTS.iter().chain(WINDOW_SHORTCUTS.iter()) {
            assert!(
                actions.insert(shortcut.action),
                "duplicate action {}",
                shortcut.action
            );
            for accelerator in shortcut.accelerators {
                assert!(
                    accelerators.insert(*accelerator),
                    "duplicate accelerator {accelerator}"
                );
            }
        }
    }

    #[test]
    fn shortcuts_dialog_lists_window_shortcuts() {
        let dialog = include_str!("shortcuts-dialog.ui");

        for text in [
            "Jump to a Conversation",
            "Search Workspace Messages",
            "Messages",
            "Unreads",
            "Refresh Conversations",
            "Focus Composer",
            "&lt;Control&gt;k",
            "&lt;Control&gt;f",
            "&lt;Control&gt;m",
            "&lt;Control&gt;o",
        ] {
            assert!(dialog.contains(text), "missing shortcut dialog text {text}");
        }
    }
}
