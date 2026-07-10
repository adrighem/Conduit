/* composer.rs
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

use gtk::prelude::*;

pub fn text_view_text(text_view: &gtk::TextView) -> String {
    let buffer = text_view.buffer();
    let (start, end) = buffer.bounds();
    buffer.text(&start, &end, false).to_string()
}

pub fn set_text_view_text(text_view: &gtk::TextView, text: &str) {
    text_view.buffer().set_text(text);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextViewEnterAction {
    Send,
    InsertNewline,
    Ignore,
}

pub fn text_view_enter_action(
    key: gtk::gdk::Key,
    state: gtk::gdk::ModifierType,
) -> TextViewEnterAction {
    if !matches!(key, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter) {
        return TextViewEnterAction::Ignore;
    }

    if state.intersects(gtk::gdk::ModifierType::SHIFT_MASK | gtk::gdk::ModifierType::CONTROL_MASK) {
        TextViewEnterAction::InsertNewline
    } else {
        TextViewEnterAction::Send
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_action_sends_on_plain_enter() {
        assert_eq!(
            text_view_enter_action(gtk::gdk::Key::Return, gtk::gdk::ModifierType::empty()),
            TextViewEnterAction::Send
        );
        assert_eq!(
            text_view_enter_action(gtk::gdk::Key::KP_Enter, gtk::gdk::ModifierType::empty()),
            TextViewEnterAction::Send
        );
    }

    #[test]
    fn enter_action_inserts_newline_with_shift_or_control() {
        assert_eq!(
            text_view_enter_action(gtk::gdk::Key::Return, gtk::gdk::ModifierType::SHIFT_MASK),
            TextViewEnterAction::InsertNewline
        );
        assert_eq!(
            text_view_enter_action(gtk::gdk::Key::Return, gtk::gdk::ModifierType::CONTROL_MASK),
            TextViewEnterAction::InsertNewline
        );
    }

    #[test]
    fn enter_action_ignores_other_keys() {
        assert_eq!(
            text_view_enter_action(gtk::gdk::Key::space, gtk::gdk::ModifierType::empty()),
            TextViewEnterAction::Ignore
        );
    }
}
