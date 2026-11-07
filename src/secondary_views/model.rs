//! Pure view models for the Threads, Search, Files and Later views.
//!
//! Everything here is GTK-free so ordering, grouping, labels and empty
//! states can be unit tested without a display.

use gettextrs::gettext;

use std::collections::HashSet;

use crate::activity::{sort_key, ActivityEntry, ActivityKey, ActivityKind};
use crate::message_markup::MessageRenderContext;
use crate::models::{SavedItem, SearchMessageLocation, SlackMessage};

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
    /// Slack user id of the sender, for the avatar; `None` for bots and apps.
    pub(crate) author_user_id: Option<String>,
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

/// A person or channel found by name in the local directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirectoryRow {
    pub(crate) title: String,
    pub(crate) detail: String,
    pub(crate) target: DirectoryTarget,
    pub(crate) icon_name: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DirectoryTarget {
    Person(String),
    Conversation(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SecondaryRow {
    Directory(DirectoryRow),
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
    /// The Threads list scrolled to its end; fetch the next page.
    LoadMoreThreads,
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
    /// Whether both rows render the same. Messages compare by what renders
    /// (see [`SlackMessage::renders_same`]): a cached and a fresh copy match.
    pub(crate) fn renders_same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Thread(row), Self::Thread(other)) => {
                row.channel_id == other.channel_id
                    && row.channel_title == other.channel_title
                    && row.reply_count == other.reply_count
                    && row.participants == other.participants
                    && row.unread == other.unread
                    && row.unread_count == other.unread_count
                    && row.root.renders_same(&other.root)
            }
            (Self::Saved(row), Self::Saved(other)) => {
                row.channel_id == other.channel_id
                    && row.channel_title == other.channel_title
                    && row.message.renders_same(&other.message)
            }
            _ => self == other,
        }
    }

    /// The message the row renders as a timeline message, if any.
    pub(crate) fn message(&self) -> Option<&SlackMessage> {
        match self {
            Self::Thread(row) => Some(&row.root),
            Self::Saved(row) => Some(&row.message),
            Self::Directory(_) | Self::Search(_) | Self::File(_) => None,
        }
    }

    /// What activating the row (click, Enter) does.
    pub(crate) fn primary_action(&self) -> Option<SecondaryAction> {
        match self {
            Self::Directory(row) => Some(SecondaryAction::Author(match &row.target {
                DirectoryTarget::Person(user_id) => {
                    crate::timeline_message_widget::TimelineAction::MessageUser(user_id.clone())
                }
                DirectoryTarget::Conversation(channel_id) => {
                    crate::timeline_message_widget::TimelineAction::OpenChannel(channel_id.clone())
                }
            })),
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
    pub(crate) fn accessible_label(&self, context: &MessageRenderContext) -> String {
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
            Self::Directory(row) => format!("{}, {}", row.title, row.detail),
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

pub(crate) fn message_author(message: &SlackMessage, context: &MessageRenderContext) -> String {
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

fn message_summary(message: &SlackMessage, context: &MessageRenderContext) -> String {
    plain_text(&message.visible_text(), context)
}

pub(crate) fn plain_text(text: &str, context: &MessageRenderContext) -> String {
    let plain = crate::channel_details::slack_text_to_plain(text, &context.custom_emojis, &|id| {
        context.user_names.get(id).cloned()
    });
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Thread inbox rows, in the order given (Slack's followed-threads order).
pub(crate) fn thread_rows(
    roots: Vec<(String, SlackMessage)>,
    state: &dyn Fn(&str, &str) -> ThreadState,
    channel_title: &dyn Fn(&str) -> String,
    context: &MessageRenderContext,
) -> Vec<ThreadRow> {
    roots
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
        .collect()
}

/// "Alice, Bob, Carol +2": known names only; unresolved ids are counted.
pub(crate) fn participants_label(ids: &[String], context: &MessageRenderContext) -> String {
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

/// Where an Activity row's conversation shows in its context line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChannelLabel {
    /// `#general`; unused for direct messages.
    pub(crate) name: String,
    pub(crate) direct: bool,
    pub(crate) private: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActivityTarget {
    Message(SearchMessageLocation),
    Thread {
        channel_id: String,
        thread_ts: String,
    },
}

/// One Activity row; rows equal to the previous render keep their widgets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActivityRow {
    pub(crate) key: ActivityKey,
    pub(crate) author_id: Option<String>,
    pub(crate) author: String,
    /// Other participants, shown as "+N".
    pub(crate) others: usize,
    pub(crate) time: String,
    pub(crate) snippet: String,
    pub(crate) context: String,
    pub(crate) private: bool,
    /// Reaction glyph (or shortcode) and how many people used it.
    pub(crate) reaction: Option<(String, usize)>,
    /// Unread count; 0 when read.
    pub(crate) unread: u64,
    pub(crate) target: ActivityTarget,
}

impl ActivityRow {
    pub(crate) fn action(&self) -> SecondaryAction {
        match &self.target {
            ActivityTarget::Message(location) => SecondaryAction::OpenMessage(location.clone()),
            ActivityTarget::Thread {
                channel_id,
                thread_ts,
            } => SecondaryAction::OpenThread {
                channel_id: channel_id.clone(),
                thread_ts: thread_ts.clone(),
            },
        }
    }

    pub(crate) fn accessible_label(&self) -> String {
        let mut parts = vec![self.author.clone(), self.context.clone()];
        if let Some((emoji, _)) = &self.reaction {
            parts.push(gettext("Reacted {emoji}").replace("{emoji}", emoji));
        }
        parts.push(self.snippet.clone());
        parts.push(self.time.clone());
        if self.unread > 0 {
            parts.push(gettext("unread"));
        }
        parts.retain(|part| !part.is_empty());
        parts.join(", ")
    }
}

/// "now", "5m", "3h", "2d" or "4w" since `ts`.
pub(crate) fn relative_time(ts: &str, now_secs: u64) -> String {
    let age = now_secs.saturating_sub(sort_key(ts).0);
    let (count, unit) = if age < 60 {
        return gettext("now");
    } else if age < 3_600 {
        (age / 60, gettext("{n}m"))
    } else if age < 86_400 {
        (age / 3_600, gettext("{n}h"))
    } else if age < 7 * 86_400 {
        (age / 86_400, gettext("{n}d"))
    } else {
        (age / (7 * 86_400), gettext("{n}w"))
    };
    unit.replace("{n}", &count.to_string())
}

fn activity_context_line(entry: &ActivityEntry, channel: &ChannelLabel) -> String {
    let in_thread = entry.item.kind == ActivityKind::ThreadReply || entry.item.thread_ts.is_some();
    match (in_thread, channel.direct) {
        (true, true) => gettext("Thread in Direct message"),
        (true, false) => gettext("Thread in {channel}").replace("{channel}", &channel.name),
        (false, true) => gettext("Direct message"),
        (false, false) => channel.name.clone(),
    }
}

/// Activity rows for `entries`, in order; entries that cannot be opened are
/// skipped. A reaction shows its latest reactor and counts the others.
pub(crate) fn activity_rows(
    entries: &[ActivityEntry],
    context: &MessageRenderContext,
    channel_label: &dyn Fn(&str) -> ChannelLabel,
    now_secs: u64,
) -> Vec<ActivityRow> {
    entries
        .iter()
        .filter_map(|entry| {
            let item = &entry.item;
            let (author_id, others, reaction) = match &item.kind {
                ActivityKind::Reaction { name, user_ids } => (
                    user_ids.last().cloned(),
                    user_ids.len().saturating_sub(1),
                    Some((plain_text(&format!(":{name}:"), context), user_ids.len())),
                ),
                ActivityKind::Mention { .. } => (item.author_id.clone(), 0, None),
                ActivityKind::ThreadReply => {
                    (item.author_id.clone(), item.participants.len(), None)
                }
            };
            let target = match (&item.kind, &item.thread_ts) {
                (ActivityKind::ThreadReply, Some(thread_ts)) => ActivityTarget::Thread {
                    channel_id: item.channel_id.clone(),
                    thread_ts: thread_ts.clone(),
                },
                (ActivityKind::ThreadReply, None) => return None,
                _ => ActivityTarget::Message(SearchMessageLocation::new(
                    &item.channel_id,
                    &item.ts,
                    item.thread_ts.as_deref(),
                )?),
            };
            let label = channel_label(&item.channel_id);
            let snippet = plain_text(&item.text, context);
            Some(ActivityRow {
                key: item.key(),
                author: author_id
                    .as_ref()
                    .and_then(|id| context.user_names.get(id).cloned())
                    .unwrap_or_else(|| gettext("Unknown")),
                author_id,
                others,
                time: relative_time(&item.timestamp, now_secs),
                snippet: match &reaction {
                    Some((emoji, _)) => gettext("Reacted {emoji} to: {text}")
                        .replace("{emoji}", emoji)
                        .replace("{text}", &snippet),
                    None => snippet,
                },
                context: activity_context_line(entry, &label),
                private: label.private && !label.direct,
                reaction,
                unread: entry.unread,
                target,
            })
        })
        .collect()
}

/// User ids `entries` show, so the window can resolve missing names.
pub(crate) fn activity_user_ids(entries: &[ActivityEntry]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for item in entries.iter().map(|entry| &entry.item) {
        ids.extend(item.author_id.iter().cloned());
        ids.extend(item.participants.iter().cloned());
        if let ActivityKind::Reaction { user_ids, .. } = &item.kind {
            ids.extend(user_ids.iter().cloned());
        }
    }
    ids
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

    pub(crate) fn context() -> MessageRenderContext {
        MessageRenderContext {
            user_names: Arc::new(HashMap::from([
                ("U1".to_string(), "Alice".to_string()),
                ("U2".to_string(), "Bob".to_string()),
                ("U3".to_string(), "Carol".to_string()),
                ("U4".to_string(), "Dave".to_string()),
            ])),
            ..MessageRenderContext::default()
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
    fn thread_rows_keep_server_order_and_carry_unread_state() {
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
        assert_eq!(order, ["C1", "C2", "C3"]);
        assert!(rows[2].unread);
        assert_eq!(rows[2].unread_count, 2);
        assert_eq!(rows[2].reply_count, 5);
        assert_eq!(rows[2].channel_title, "#C3");
        assert!(!rows[0].unread);
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
            author_user_id: None,
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
    fn activity_entry(
        ts: &str,
        kind: ActivityKind,
        thread_ts: Option<&str>,
        unread: u64,
    ) -> ActivityEntry {
        let mut item =
            crate::activity::ActivityItem::new("C1", ts, kind, ts).with_text("see <@U2>");
        item.author_id = Some("U1".to_string());
        item.thread_ts = thread_ts.map(ToString::to_string);
        ActivityEntry { item, unread }
    }

    fn channel_label(channel_id: &str) -> ChannelLabel {
        ChannelLabel {
            name: "#general".to_string(),
            direct: channel_id == "D1",
            private: true,
        }
    }

    #[test]
    fn activity_rows_describe_mentions_reactions_and_threads() {
        let mention = ActivityKind::Mention {
            broadcast: false,
            group: None,
        };
        let reaction = ActivityKind::Reaction {
            name: "not-an-emoji".to_string(),
            user_ids: vec!["U1".to_string(), "U3".to_string(), "U4".to_string()],
        };
        let mut thread = activity_entry("50.0", ActivityKind::ThreadReply, Some("3.0"), 2);
        thread.item.participants = vec!["U3".to_string()];
        let entries = [
            activity_entry("1000.0", mention.clone(), None, 1),
            activity_entry("900.0", mention, Some("800.0"), 0),
            activity_entry("700.0", reaction, None, 0),
            thread,
        ];
        let rows = activity_rows(&entries, &context(), &channel_label, 1_000 + 3 * 3_600);

        assert_eq!(rows[0].author, "Alice");
        assert_eq!(rows[0].context, "#general");
        assert_eq!(rows[0].snippet, "see @Bob");
        assert_eq!(rows[0].time, "3h");
        assert!(rows[0].private && rows[0].unread == 1);
        assert!(matches!(
            rows[0].action(),
            SecondaryAction::OpenMessage(location) if location.message_ts() == "1000.0"
        ));

        assert_eq!(rows[1].context, "Thread in #general");
        assert!(matches!(
            rows[1].action(),
            SecondaryAction::OpenMessage(location) if location.thread_ts() == Some("800.0")
        ));

        assert_eq!((rows[2].author.as_str(), rows[2].others), ("Dave", 2));
        assert_eq!(rows[2].reaction.as_ref().map(|chip| chip.1), Some(3));
        assert!(rows[2].snippet.starts_with("Reacted "));
        assert!(rows[2].snippet.ends_with(" to: see @Bob"));

        assert_eq!(rows[3].others, 1);
        assert!(matches!(
            rows[3].action(),
            SecondaryAction::OpenThread { thread_ts, .. } if thread_ts == "3.0"
        ));
        assert_eq!(
            rows[3].accessible_label(),
            "Alice, Thread in #general, see @Bob, 3h, unread"
        );
    }

    #[test]
    fn direct_messages_use_their_own_context_lines_and_no_lock() {
        let mention = ActivityKind::Mention {
            broadcast: false,
            group: None,
        };
        let mut direct = activity_entry("10.0", mention.clone(), None, 0);
        direct.item.channel_id = "D1".to_string();
        let mut direct_thread = activity_entry("11.0", mention, Some("9.0"), 0);
        direct_thread.item.channel_id = "D1".to_string();
        let rows = activity_rows(&[direct, direct_thread], &context(), &channel_label, 20);
        assert_eq!(rows[0].context, "Direct message");
        assert_eq!(rows[1].context, "Thread in Direct message");
        assert!(!rows[0].private);
    }

    #[test]
    fn relative_time_steps_through_units() {
        assert_eq!(relative_time("100.0", 130), "now");
        assert_eq!(relative_time("100.0", 100 + 5 * 60), "5m");
        assert_eq!(relative_time("100.0", 100 + 2 * 3_600), "2h");
        assert_eq!(relative_time("100.0", 100 + 3 * 86_400), "3d");
        assert_eq!(relative_time("100.0", 100 + 15 * 86_400), "2w");
    }

    #[test]
    fn activity_user_ids_cover_authors_reactors_and_participants() {
        let reaction = ActivityKind::Reaction {
            name: "tada".to_string(),
            user_ids: vec!["U3".to_string()],
        };
        let mut thread = activity_entry("2.0", ActivityKind::ThreadReply, Some("1.0"), 0);
        thread.item.participants = vec!["U4".to_string()];
        let ids = activity_user_ids(&[activity_entry("1.0", reaction, None, 0), thread]);
        let mut ids = ids.into_iter().collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, ["U1", "U3", "U4"]);
    }
}
