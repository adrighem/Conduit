/* mention_target.rs
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

//! What an `@` mention points at. A composer mention stores one Slack ID;
//! Slack's ID space tells users (`U…`/`W…`), user groups (`S…`) and the
//! broadcast keywords (`here`, `channel`, `everyone`) apart, so the kind is
//! derived from the ID instead of being persisted next to it.

use serde_json::{json, Value};

use super::{mention_candidates, MentionCandidate};
use crate::models::{SlackUser, SlackUserGroup};

/// Broadcast keywords with the hint shown next to them in completion.
const BROADCAST_MENTIONS: &[(&str, &str)] = &[
    ("here", "Notify active members of this conversation"),
    ("channel", "Notify all members of this conversation"),
    ("everyone", "Notify everyone in the workspace"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionTarget<'a> {
    User(&'a str),
    UserGroup(&'a str),
    Broadcast(&'a str),
}

impl<'a> MentionTarget<'a> {
    pub fn classify(id: &'a str) -> Self {
        if BROADCAST_MENTIONS.iter().any(|(keyword, _)| *keyword == id) {
            Self::Broadcast(id)
        } else if id.starts_with('S') {
            Self::UserGroup(id)
        } else {
            Self::User(id)
        }
    }

    pub fn is_person(self) -> bool {
        matches!(self, Self::User(_))
    }

    /// The `rich_text_section` element Slack expects for this mention.
    pub fn rich_text_element(self) -> Value {
        match self {
            Self::User(id) => json!({"type": "user", "user_id": id}),
            Self::UserGroup(id) => json!({"type": "usergroup", "usergroup_id": id}),
            Self::Broadcast(range) => json!({"type": "broadcast", "range": range}),
        }
    }

    /// The `mrkdwn` fallback text Slack expects for this mention.
    pub fn fallback_markup(self) -> String {
        match self {
            Self::User(id) => format!("<@{id}>"),
            Self::UserGroup(id) => format!("<!subteam^{id}>"),
            Self::Broadcast(range) => format!("<!{range}>"),
        }
    }
}

/// Every `@` completion target: people from all user sources (the
/// `users.list` snapshot misses Slack Connect members that only `users.info`
/// resolves), then user groups, then the broadcast keywords.
pub fn all_mention_candidates<'a>(
    people: impl IntoIterator<Item = &'a SlackUser>,
    groups: &[SlackUserGroup],
) -> Vec<MentionCandidate> {
    let people = people.into_iter().cloned().collect::<Vec<_>>();
    let mut candidates = mention_candidates(&people);
    candidates.extend(user_group_mention_candidates(groups));
    candidates.extend(broadcast_mention_candidates());
    candidates
}

/// Completion candidates for `@here`, `@channel` and `@everyone`.
fn broadcast_mention_candidates() -> Vec<MentionCandidate> {
    BROADCAST_MENTIONS
        .iter()
        .map(|(keyword, hint)| MentionCandidate {
            user_id: (*keyword).to_string(),
            display_name: (*keyword).to_string(),
            full_name: Some((*hint).to_string()),
            username: None,
            search_aliases: Vec::new(),
        })
        .collect()
}

/// Completion candidates for Slack user groups, labelled by their handle.
fn user_group_mention_candidates(groups: &[SlackUserGroup]) -> Vec<MentionCandidate> {
    groups
        .iter()
        .filter(|group| {
            matches!(
                MentionTarget::classify(group.id.trim()),
                MentionTarget::UserGroup(_)
            )
        })
        .map(|group| {
            let display_name = group.mention_label();
            let full_name = group
                .name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case(&display_name))
                .map(str::to_string);
            MentionCandidate {
                user_id: group.id.trim().to_string(),
                display_name,
                full_name,
                username: None,
                search_aliases: Vec::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: &str, handle: Option<&str>, name: Option<&str>) -> SlackUserGroup {
        SlackUserGroup {
            id: id.to_string(),
            handle: handle.map(str::to_string),
            name: name.map(str::to_string),
            users: Vec::new(),
        }
    }

    #[test]
    fn classifies_users_groups_and_broadcasts_by_id() {
        assert_eq!(MentionTarget::classify("U1"), MentionTarget::User("U1"));
        assert_eq!(MentionTarget::classify("W1"), MentionTarget::User("W1"));
        assert_eq!(
            MentionTarget::classify("S1"),
            MentionTarget::UserGroup("S1")
        );
        for keyword in ["here", "channel", "everyone"] {
            assert_eq!(
                MentionTarget::classify(keyword),
                MentionTarget::Broadcast(keyword)
            );
        }
    }

    #[test]
    fn serializes_slack_markup_per_target() {
        assert_eq!(MentionTarget::classify("U1").fallback_markup(), "<@U1>");
        assert_eq!(
            MentionTarget::classify("S1").fallback_markup(),
            "<!subteam^S1>"
        );
        assert_eq!(MentionTarget::classify("here").fallback_markup(), "<!here>");
        assert_eq!(
            MentionTarget::classify("S1").rich_text_element(),
            json!({"type": "usergroup", "usergroup_id": "S1"})
        );
        assert_eq!(
            MentionTarget::classify("channel").rich_text_element(),
            json!({"type": "broadcast", "range": "channel"})
        );
    }

    #[test]
    fn user_group_candidates_use_handle_and_keep_distinct_name() {
        let candidates = user_group_mention_candidates(&[
            group("S1", Some("@devs"), Some("Developers")),
            group("S2", Some("ops"), Some("ops")),
            group("U9", Some("bogus"), None),
        ]);

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].user_id, "S1");
        assert_eq!(candidates[0].display_name, "devs");
        assert_eq!(candidates[0].full_name.as_deref(), Some("Developers"));
        assert_eq!(candidates[1].display_name, "ops");
        assert_eq!(candidates[1].full_name, None);
    }

    #[test]
    fn all_candidates_merge_external_people_with_groups_and_broadcasts() {
        let member = SlackUser {
            id: Some("U1".into()),
            name: Some("ada".into()),
            ..Default::default()
        };
        let external = SlackUser {
            id: Some("U2".into()),
            name: Some("grace".into()),
            ..Default::default()
        };
        let ids = all_mention_candidates(
            [&member, &external, &member],
            &[group("S1", Some("devs"), None)],
        )
        .into_iter()
        .map(|candidate| candidate.user_id)
        .collect::<Vec<_>>();

        assert_eq!(ids, ["U1", "U2", "S1", "here", "channel", "everyone"]);
    }

    #[test]
    fn broadcast_candidates_cover_all_keywords() {
        let ids = broadcast_mention_candidates()
            .into_iter()
            .map(|candidate| candidate.user_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["here", "channel", "everyone"]);
    }
}
