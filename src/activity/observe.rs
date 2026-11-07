//! Realtime and `search.messages` ingest: what may enter the Activity feed.

use super::{ActivityItem, ActivityKind, BACKFILL_MAX_AGE_MS};
use crate::attention;
use crate::models::SearchMatch;
use crate::socket_mode::{SocketModeEvent, SocketModeMessageKind};

/// One reaction added to or removed from the user's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionChange {
    pub(crate) channel_id: String,
    pub(crate) ts: String,
    pub(crate) name: String,
    pub(crate) user_id: String,
    pub(crate) added: bool,
}

/// What the runtime saw on the realtime stream that may belong in the feed.
#[derive(Debug, Clone)]
pub enum ActivityObservation {
    /// A message from someone else that may mention the user; the window
    /// decides, because user-group membership lives there.
    Message {
        channel_id: String,
        message: Box<crate::models::SlackMessage>,
    },
    Reaction(ReactionChange),
}

/// Cheap prefilter run on the realtime path: only text with a user, broadcast
/// or user-group mention is forwarded.
pub(crate) fn observe_socket_event(
    event: &SocketModeEvent,
    me: Option<&str>,
) -> Option<ActivityObservation> {
    let me = me?;
    match event {
        SocketModeEvent::Message(event) => {
            let message = &event.message;
            let text = message.text.as_deref()?;
            (event.kind == SocketModeMessageKind::Posted
                && message.user.as_deref() != Some(me)
                && (attention::contains_direct_mention(text, Some(me))
                    || attention::contains_broadcast_mention(text)
                    || text.contains("<!subteam^")))
            .then(|| ActivityObservation::Message {
                channel_id: event.channel_id.clone(),
                message: Box::new(message.clone()),
            })
        }
        SocketModeEvent::Reaction(reaction) => {
            (reaction.item_user.as_deref() == Some(me) && reaction.user_id != me).then(|| {
                ActivityObservation::Reaction(ReactionChange {
                    channel_id: reaction.channel_id.clone(),
                    ts: reaction.ts.clone(),
                    name: reaction.name.clone(),
                    user_id: reaction.user_id.clone(),
                    added: reaction.added,
                })
            })
        }
        _ => None,
    }
}

/// The mention item for `message`, if it reaches the user: directly, through
/// a user group they belong to (`in_group`), or by `@channel`/`@here`.
pub(crate) fn mention_item(
    channel_id: &str,
    message: &crate::models::SlackMessage,
    me: &str,
    in_group: &dyn Fn(&str) -> bool,
) -> Option<ActivityItem> {
    let text = message.text.as_deref()?;
    let (broadcast, group) = if attention::contains_direct_mention(text, Some(me)) {
        (false, None)
    } else if let Some(group) = subteam_ids(text).into_iter().find(|id| in_group(id)) {
        (false, Some(group.to_string()))
    } else if attention::contains_broadcast_mention(text) {
        (true, None)
    } else {
        return None;
    };
    let mut item = ActivityItem::new(
        channel_id,
        &message.ts,
        ActivityKind::Mention { broadcast, group },
        &message.ts,
    )
    .with_text(text);
    item.author_id = message.user.clone();
    item.thread_ts = message.thread_ts.clone().filter(|ts| *ts != message.ts);
    Some(item)
}

/// User group ids in Slack's `<!subteam^S123>` and `<!subteam^S123|@name>`.
fn subteam_ids(text: &str) -> Vec<&str> {
    let mut ids = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<!subteam^") {
        rest = &rest[start + "<!subteam^".len()..];
        let Some(end) = rest.find('>') else { break };
        ids.push(rest[..end].split('|').next().unwrap_or_default());
        rest = &rest[end..];
    }
    ids
}

/// Mention items from a `search.messages` backfill; the user's own messages
/// and matches without a channel or timestamp are skipped.
pub(crate) fn items_from_search(matches: &[SearchMatch], me: &str) -> Vec<ActivityItem> {
    matches
        .iter()
        .filter(|found| found.user.as_deref() != Some(me))
        .filter_map(|found| {
            let location = found.message_location()?;
            let text = found.text.as_deref().unwrap_or_default();
            let kind = ActivityKind::Mention {
                broadcast: false,
                group: None,
            };
            let mut item = ActivityItem::new(
                location.channel_id(),
                location.message_ts(),
                kind,
                location.message_ts(),
            )
            .with_text(text);
            item.author_id = found.user.clone();
            item.thread_ts = location.thread_ts().map(ToString::to_string);
            Some(item)
        })
        .collect()
}

/// Whether the persisted backfill stamp is missing or older than
/// [`BACKFILL_MAX_AGE_MS`].
pub(crate) fn backfill_is_due(refreshed_at_ms: Option<i64>, now_ms: i64) -> bool {
    refreshed_at_ms.is_none_or(|refreshed| now_ms.saturating_sub(refreshed) >= BACKFILL_MAX_AGE_MS)
}
