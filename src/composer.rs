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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiToken {
    pub start: usize,
    pub end: usize,
    pub query: String,
}

fn is_shortcode_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+')
}

fn is_shortcode_boundary(character: char) -> bool {
    !is_shortcode_character(character) && !matches!(character, ':' | '/' | '\\')
}

pub fn emoji_token_at_caret(text: &str, caret: usize) -> Option<EmojiToken> {
    let characters = text.chars().collect::<Vec<_>>();
    if caret > characters.len()
        || caret < 3
        || characters
            .get(caret)
            .is_some_and(|character| is_shortcode_character(*character) || *character == ':')
    {
        return None;
    }

    let mut query_start = caret;
    while query_start > 0 && is_shortcode_character(characters[query_start - 1]) {
        query_start -= 1;
    }
    let colon = query_start.checked_sub(1)?;
    if characters[colon] != ':' || (colon > 0 && !is_shortcode_boundary(characters[colon - 1])) {
        return None;
    }

    let query = characters[query_start..caret].iter().collect::<String>();
    if query
        .chars()
        .filter(|character| character.is_ascii_alphabetic())
        .count()
        < 2
    {
        return None;
    }

    Some(EmojiToken {
        start: colon,
        end: caret,
        query,
    })
}

pub fn replace_emoji_token(text: &str, token: &EmojiToken, shortcode: &str) -> (String, usize) {
    let mut characters = text.chars().collect::<Vec<_>>();
    let replacement = emoji_shortcode(shortcode).chars().collect::<Vec<_>>();
    let end = token.end.min(characters.len());
    let start = token.start.min(end);
    characters.splice(start..end, replacement.iter().copied());
    let caret = start + replacement.len();
    (characters.into_iter().collect(), caret)
}

pub fn emoji_shortcode(name: &str) -> String {
    format!(":{name}:")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmojiCompletionKeyAction {
    Previous,
    Next,
    Accept,
    Dismiss,
    Ignore,
}

pub fn emoji_completion_key_action(
    key: gtk::gdk::Key,
    state: gtk::gdk::ModifierType,
) -> EmojiCompletionKeyAction {
    match key {
        gtk::gdk::Key::Up => EmojiCompletionKeyAction::Previous,
        gtk::gdk::Key::Down => EmojiCompletionKeyAction::Next,
        gtk::gdk::Key::Escape => EmojiCompletionKeyAction::Dismiss,
        gtk::gdk::Key::Tab
            if !state.intersects(
                gtk::gdk::ModifierType::SHIFT_MASK
                    | gtk::gdk::ModifierType::CONTROL_MASK
                    | gtk::gdk::ModifierType::ALT_MASK
                    | gtk::gdk::ModifierType::SUPER_MASK,
            ) =>
        {
            EmojiCompletionKeyAction::Accept
        }
        gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter
            if !state.intersects(
                gtk::gdk::ModifierType::SHIFT_MASK | gtk::gdk::ModifierType::CONTROL_MASK,
            ) =>
        {
            EmojiCompletionKeyAction::Accept
        }
        _ => EmojiCompletionKeyAction::Ignore,
    }
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

    #[test]
    fn detects_shortcode_tokens_at_supported_boundaries() {
        assert_eq!(
            emoji_token_at_caret(":sm", 3),
            Some(EmojiToken {
                start: 0,
                end: 3,
                query: "sm".to_string(),
            })
        );
        assert_eq!(emoji_token_at_caret("hello :par", 10).unwrap().query, "par");
        assert_eq!(emoji_token_at_caret("hello (:sm", 10).unwrap().query, "sm");
        assert_eq!(emoji_token_at_caret("hello.:sm", 9).unwrap().query, "sm");
    }

    #[test]
    fn rejects_incomplete_completed_and_embedded_shortcode_tokens() {
        assert_eq!(emoji_token_at_caret(":s", 2), None);
        assert_eq!(emoji_token_at_caret(":12", 3), None);
        assert_eq!(emoji_token_at_caret(":a1", 3), None);
        assert_eq!(emoji_token_at_caret(":sm:", 4), None);
        assert_eq!(emoji_token_at_caret("https://sm", 10), None);
        assert_eq!(emoji_token_at_caret("12:30", 5), None);
        assert_eq!(emoji_token_at_caret("word:sm", 7), None);
        assert_eq!(emoji_token_at_caret("hello :smile", 9), None);
    }

    #[test]
    fn replaces_only_the_active_token_using_character_offsets() {
        let text = "Živjo :sm there";
        let token = emoji_token_at_caret(text, 9).unwrap();
        let (updated, caret) = replace_emoji_token(text, &token, "smile");

        assert_eq!(updated, "Živjo :smile: there");
        assert_eq!(caret, 13);
    }

    #[test]
    fn completion_keys_do_not_override_modified_enter() {
        assert_eq!(
            emoji_completion_key_action(gtk::gdk::Key::Return, gtk::gdk::ModifierType::empty()),
            EmojiCompletionKeyAction::Accept
        );
        assert_eq!(
            emoji_completion_key_action(gtk::gdk::Key::Return, gtk::gdk::ModifierType::SHIFT_MASK),
            EmojiCompletionKeyAction::Ignore
        );
        assert_eq!(
            emoji_completion_key_action(gtk::gdk::Key::Down, gtk::gdk::ModifierType::empty()),
            EmojiCompletionKeyAction::Next
        );
        assert_eq!(
            emoji_completion_key_action(gtk::gdk::Key::Escape, gtk::gdk::ModifierType::empty()),
            EmojiCompletionKeyAction::Dismiss
        );
    }
}
