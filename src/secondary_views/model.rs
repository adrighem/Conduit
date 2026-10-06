//! Pure view models for the Threads, Search, Files and Later views.
//!
//! Everything here is GTK-free so ordering, grouping, labels and empty
//! states can be unit tested without a display.

use std::cmp::Ordering;

use gettextrs::gettext;

use crate::message_html::MessageHtmlContext;
use crate::models::{slack_timestamp_is_after, SavedItem, SearchMessageLocation, SlackMessage};

/// Participant names shown on a thread card before collapsing into "+N".
const MAX_PARTICIPANT_NAMES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecondaryKind {
    Threads,
    Search,
    Files,
    Saved,
}

/// Icon, title and description for an `AdwStatusPage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusContent {
    pub(crate) icon_name: &'static str,
    pub(crate) title: String,
    pub(crate) description: String,
}

impl SecondaryKind {
    pub(crate) fn title(self) -> String {
        match self {
            Self::Threads => gettext("Threads"),
            Self::Search => gettext("Search results"),
            Self::Files => gettext("Files"),
            Self::Saved => gettext("Later"),
        }
    }

    pub(crate) fn empty_state(self) -> StatusContent {
        let (icon_name, title, description) = match self {
            Self::Threads => (
                "mail-reply-all-symbolic",
                gettext("No Threads"),
                gettext("Threads you take part in will appear here"),
            ),
            Self::Search => (
                "system-search-symbolic",
                gettext("No Results Found"),
                gettext("Try a different search"),
            ),
            Self::Files => (
                "folder-documents-symbolic",
                gettext("No Files"),
                gettext("Files shared in your workspace will appear here"),
            ),
            Self::Saved => (
                "bookmark-new-symbolic",
                gettext("Nothing Saved for Later"),
                gettext("Messages you save for later will appear here"),
            ),
        };
        StatusContent {
            icon_name,
            title,
            description,
        }
    }
}

/// Thread catalog read state for one thread root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ThreadState {
    pub(crate) has_unread: bool,
    pub(crate) unread_count: u64,
    pub(crate) participant_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThreadRow {
    pub(crate) channel_id: String,
    pub(crate) channel_title: String,
    pub(crate) root: SlackMessage,
    pub(crate) reply_count: u64,
    pub(crate) participants: String,
    pub(crate) unread: bool,
    pub(crate) unread_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchRow {
    pub(crate) location: Option<SearchMessageLocation>,
    pub(crate) channel_title: String,
    pub(crate) author: String,
    pub(crate) ts: Option<String>,
    pub(crate) plain_text: String,
    /// Pango markup: escaped text with query terms in bold.
    pub(crate) snippet_markup: String,
    pub(crate) permalink: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileOpenTarget {
    Media {
        url: String,
        name: String,
        video: bool,
    },
    External(String),
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRow {
    pub(crate) title: String,
    pub(crate) icon_name: &'static str,
    pub(crate) detail: String,
    pub(crate) owner: Option<String>,
    pub(crate) channel_title: Option<String>,
    pub(crate) created_ts: Option<String>,
    pub(crate) thumbnail_url: Option<String>,
    pub(crate) open: FileOpenTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SavedRow {
    pub(crate) channel_id: String,
    pub(crate) channel_title: String,
    pub(crate) message: SlackMessage,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SecondaryRow {
    Thread(ThreadRow),
    Search(SearchRow),
    File(FileRow),
    Saved(SavedRow),
}

/// User intent raised by a secondary view; the window maps it to existing
/// navigation and runtime commands.
#[derive(Debug, Clone)]
pub(crate) enum SecondaryAction {
    OpenThread {
        channel_id: String,
        thread_ts: String,
    },
    OpenMessage(SearchMessageLocation),
    OpenExternal(String),
    OpenMedia {
        url: String,
        name: String,
        video: bool,
    },
    RemoveSaved {
        channel_id: String,
        ts: String,
        thread_ts: Option<String>,
    },
    Author(crate::timeline_message_widget::TimelineAction),
}

impl SecondaryRow {
    /// What activating the row (click, Enter) does.
    pub(crate) fn primary_action(&self) -> Option<SecondaryAction> {
        match self {
            Self::Thread(row) => Some(SecondaryAction::OpenThread {
                channel_id: row.channel_id.clone(),
                thread_ts: row.root.ts.clone(),
            }),
            Self::Search(row) => row
                .location
                .clone()
                .map(SecondaryAction::OpenMessage)
                .or_else(|| row.permalink.clone().map(SecondaryAction::OpenExternal)),
            Self::File(row) => match &row.open {
                FileOpenTarget::Media { url, name, video } => Some(SecondaryAction::OpenMedia {
                    url: url.clone(),
                    name: name.clone(),
                    video: *video,
                }),
                FileOpenTarget::External(url) => Some(SecondaryAction::OpenExternal(url.clone())),
                FileOpenTarget::Unavailable => None,
            },
            Self::Saved(row) => SearchMessageLocation::new(
                &row.channel_id,
                &row.message.ts,
                row.message.thread_ts.as_deref(),
            )
            .map(SecondaryAction::OpenMessage),
        }
    }

    /// Screen reader label for the whole list item.
    pub(crate) fn accessible_label(&self, context: &MessageHtmlContext) -> String {
        match self {
            Self::Thread(row) => {
                let mut label = gettext("Thread in {channel} by {author}: {text}")
                    .replace("{channel}", &row.channel_title)
                    .replace("{author}", &message_author(&row.root, context))
                    .replace("{text}", &message_summary(&row.root, context));
                label.push_str(", ");
                label.push_str(&reply_count_label(row.reply_count));
                if row.unread {
                    label.push_str(", ");
                    label.push_str(&gettext("unread"));
                }
                label
            }
            Self::Search(row) => gettext("{author} in {channel}: {text}")
                .replace("{author}", &row.author)
                .replace("{channel}", &row.channel_title)
                .replace("{text}", &row.plain_text),
            Self::File(row) => {
                let mut parts = vec![row.title.clone()];
                parts.extend(
                    [&row.detail]
                        .into_iter()
                        .filter(|part| !part.is_empty())
                        .cloned(),
                );
                parts.extend(row.owner.clone());
                parts.extend(row.channel_title.clone());
                parts.join(", ")
            }
            Self::Saved(row) => gettext("Saved message in {channel} by {author}: {text}")
                .replace("{channel}", &row.channel_title)
                .replace("{author}", &message_author(&row.message, context))
                .replace("{text}", &message_summary(&row.message, context)),
        }
    }
}

pub(crate) fn reply_count_label(count: u64) -> String {
    if count == 1 {
        gettext("1 reply")
    } else {
        gettext("{count} replies").replace("{count}", &count.to_string())
    }
}

pub(crate) fn message_author(message: &SlackMessage, context: &MessageHtmlContext) -> String {
    context
        .display_user_id(message)
        .and_then(|user_id| context.user_names.get(user_id).cloned())
        .or_else(|| message.username.clone())
        .or_else(|| {
            message
                .bot_profile
                .as_ref()
                .and_then(|profile| profile.name.clone())
        })
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| gettext("Unknown"))
}

fn message_summary(message: &SlackMessage, context: &MessageHtmlContext) -> String {
    plain_text(&message.visible_text(), context)
}

pub(super) fn plain_text(text: &str, context: &MessageHtmlContext) -> String {
    let plain = crate::channel_details::slack_text_to_plain(text, &context.custom_emojis, &|id| {
        context.user_names.get(id).cloned()
    });
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compare_ts_desc(left: &str, right: &str) -> Ordering {
    if slack_timestamp_is_after(left, right) {
        Ordering::Less
    } else if slack_timestamp_is_after(right, left) {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

fn thread_activity_ts(root: &SlackMessage) -> &str {
    root.latest_reply.as_deref().unwrap_or(&root.ts)
}

/// Thread inbox rows, most recent activity first.
pub(crate) fn thread_rows(
    roots: Vec<(String, SlackMessage)>,
    state: &dyn Fn(&str, &str) -> ThreadState,
    channel_title: &dyn Fn(&str) -> String,
    context: &MessageHtmlContext,
) -> Vec<ThreadRow> {
    let mut rows = roots
        .into_iter()
        .map(|(channel_id, root)| {
            let state = state(&channel_id, &root.ts);
            let mut participant_ids = root.reply_users.clone().unwrap_or_default();
            for id in state.participant_ids {
                if !participant_ids.contains(&id) {
                    participant_ids.push(id);
                }
            }
            ThreadRow {
                channel_title: channel_title(&channel_id),
                channel_id,
                reply_count: root.reply_count.unwrap_or_default(),
                participants: participants_label(&participant_ids, context),
                unread: state.has_unread,
                unread_count: state.unread_count,
                root,
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        compare_ts_desc(
            thread_activity_ts(&left.root),
            thread_activity_ts(&right.root),
        )
    });
    rows
}

/// "Alice, Bob, Carol +2": known names only; unresolved ids are counted.
pub(crate) fn participants_label(ids: &[String], context: &MessageHtmlContext) -> String {
    let mut names = Vec::new();
    let mut hidden = 0_usize;
    for id in ids {
        match context.user_names.get(id) {
            Some(name) if names.len() < MAX_PARTICIPANT_NAMES => names.push(name.clone()),
            _ => hidden += 1,
        }
    }
    let mut label = names.join(", ");
    if hidden > 0 {
        if !label.is_empty() {
            label.push(' ');
        }
        label.push_str(&format!("+{hidden}"));
    }
    label
}

pub(super) fn is_http_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Saved messages in Slack's order; items without a message are skipped.
pub(crate) fn saved_rows(
    items: &[SavedItem],
    channel_title: &dyn Fn(&str) -> String,
) -> Vec<SavedRow> {
    items
        .iter()
        .filter_map(|item| {
            let channel_id = item.channel.as_deref().or(item.group.as_deref())?;
            let message = item.message.as_ref()?;
            Some(SavedRow {
                channel_id: channel_id.to_string(),
                channel_title: channel_title(channel_id),
                message: message.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;

    pub(crate) fn context() -> MessageHtmlContext {
        MessageHtmlContext {
            user_names: Arc::new(HashMap::from([
                ("U1".to_string(), "Alice".to_string()),
                ("U2".to_string(), "Bob".to_string()),
                ("U3".to_string(), "Carol".to_string()),
                ("U4".to_string(), "Dave".to_string()),
            ])),
            ..MessageHtmlContext::default()
        }
    }

    fn root(ts: &str, latest_reply: Option<&str>, replies: u64) -> SlackMessage {
        SlackMessage {
            ts: ts.to_string(),
            user: Some("U1".to_string()),
            text: Some(format!("root {ts}")),
            reply_count: Some(replies),
            latest_reply: latest_reply.map(ToString::to_string),
            ..SlackMessage::default()
        }
    }

    pub(crate) fn title(channel_id: &str) -> String {
        format!("#{channel_id}")
    }

    #[test]
    fn thread_rows_sort_by_latest_activity_and_carry_unread_state() {
        let roots = vec![
            ("C1".to_string(), root("100.0", Some("150.0"), 2)),
            ("C2".to_string(), root("120.0", None, 1)),
            ("C3".to_string(), root("90.0", Some("200.0"), 5)),
        ];
        let state = |channel_id: &str, _: &str| ThreadState {
            has_unread: channel_id == "C3",
            unread_count: u64::from(channel_id == "C3") * 2,
            participant_ids: Vec::new(),
        };
        let rows = thread_rows(roots, &state, &title, &context());

        let order = rows
            .iter()
            .map(|row| row.channel_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(order, ["C3", "C1", "C2"]);
        assert!(rows[0].unread);
        assert_eq!(rows[0].unread_count, 2);
        assert_eq!(rows[0].reply_count, 5);
        assert_eq!(rows[0].channel_title, "#C3");
        assert!(!rows[1].unread);
    }

    #[test]
    fn thread_rows_merge_reply_users_with_catalog_participants() {
        let mut message = root("1.0", Some("2.0"), 3);
        message.reply_users = Some(vec!["U2".to_string()]);
        let state = |_: &str, _: &str| ThreadState {
            participant_ids: vec!["U2".to_string(), "U3".to_string()],
            ..ThreadState::default()
        };
        let rows = thread_rows(
            vec![("C1".to_string(), message)],
            &state,
            &title,
            &context(),
        );
        assert_eq!(rows[0].participants, "Bob, Carol");
    }

    #[test]
    fn participants_label_collapses_extra_and_unknown_users() {
        let ids = ["U1", "U2", "U3", "U4", "UX"].map(ToString::to_string);
        assert_eq!(participants_label(&ids, &context()), "Alice, Bob, Carol +2");
        assert_eq!(participants_label(&[], &context()), "");
        assert_eq!(participants_label(&["UX".to_string()], &context()), "+1");
    }

    #[test]
    fn reply_count_label_handles_singular() {
        assert_eq!(reply_count_label(1), "1 reply");
        assert_eq!(reply_count_label(4), "4 replies");
    }

    #[test]
    fn saved_rows_keep_order_and_skip_items_without_messages() {
        let items = vec![
            SavedItem {
                kind: Some("message".to_string()),
                channel: Some("C2".to_string()),
                message: Some(root("2.0", None, 0)),
                ..SavedItem::default()
            },
            SavedItem {
                kind: Some("channel".to_string()),
                channel: Some("C3".to_string()),
                ..SavedItem::default()
            },
            SavedItem {
                kind: Some("message".to_string()),
                group: Some("G1".to_string()),
                message: Some(root("1.0", None, 0)),
                ..SavedItem::default()
            },
        ];
        let rows = saved_rows(&items, &title);
        let ids = rows
            .iter()
            .map(|row| row.channel_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["C2", "G1"]);
        assert_eq!(rows[1].channel_title, "#G1");
    }

    #[test]
    fn empty_states_and_titles_cover_every_view() {
        for kind in [
            SecondaryKind::Threads,
            SecondaryKind::Search,
            SecondaryKind::Files,
            SecondaryKind::Saved,
        ] {
            let state = kind.empty_state();
            assert!(state.icon_name.ends_with("-symbolic"));
            assert!(!state.title.is_empty());
            assert!(!state.description.is_empty());
            assert!(!kind.title().is_empty());
        }
        assert_eq!(SecondaryKind::Saved.title(), "Later");
    }

    #[test]
    fn primary_actions_route_each_row_kind() {
        let context = context();
        let thread = SecondaryRow::Thread(
            thread_rows(
                vec![("C1".to_string(), root("5.0", None, 1))],
                &|_, _| ThreadState::default(),
                &title,
                &context,
            )
            .remove(0),
        );
        assert!(matches!(
            thread.primary_action(),
            Some(SecondaryAction::OpenThread { channel_id, thread_ts })
                if channel_id == "C1" && thread_ts == "5.0"
        ));

        let mut reply = root("7.0", None, 0);
        reply.thread_ts = Some("5.0".to_string());
        let saved = SecondaryRow::Saved(SavedRow {
            channel_id: "C1".to_string(),
            channel_title: "#C1".to_string(),
            message: reply,
        });
        let Some(SecondaryAction::OpenMessage(location)) = saved.primary_action() else {
            panic!("saved rows open their message");
        };
        assert_eq!(location.thread_ts(), Some("5.0"));
        assert_eq!(location.message_ts(), "7.0");

        let orphan = SecondaryRow::Search(SearchRow {
            location: None,
            channel_title: "Slack".to_string(),
            author: "Bob".to_string(),
            ts: None,
            plain_text: String::new(),
            snippet_markup: String::new(),
            permalink: Some("https://example.slack.com/p1".to_string()),
        });
        assert!(matches!(
            orphan.primary_action(),
            Some(SecondaryAction::OpenExternal(url)) if url.ends_with("/p1")
        ));
    }

    #[test]
    fn accessible_labels_describe_rows() {
        let context = context();
        let thread = SecondaryRow::Thread(ThreadRow {
            channel_id: "C1".to_string(),
            channel_title: "#general".to_string(),
            root: root("1.0", None, 2),
            reply_count: 2,
            participants: String::new(),
            unread: true,
            unread_count: 1,
        });
        assert_eq!(
            thread.accessible_label(&context),
            "Thread in #general by Alice: root 1.0, 2 replies, unread"
        );
    }
}
