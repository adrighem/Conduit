use super::*;
use crate::models::{SearchMatch, SlackMessage, SlackSearchChannel};
use crate::socket_mode::{
    SocketModeEvent, SocketModeMessageEvent, SocketModeMessageKind, SocketModeReactionEvent,
};
use crate::thread_catalog::ThreadKey;
use crate::unread_ledger::ConversationReadState;

fn mention(channel: &str, ts: &str) -> ActivityItem {
    let kind = ActivityKind::Mention {
        broadcast: false,
        group: None,
    };
    ActivityItem::new(channel, ts, kind, ts).with_text("hi <@UME>")
}

fn reaction(user: &str, added: bool) -> ReactionChange {
    ReactionChange {
        channel_id: "C1".to_string(),
        ts: "5.000100".to_string(),
        name: "tada".to_string(),
        user_id: user.to_string(),
        added,
    }
}

fn ledger(channel: &str, last_read: &str) -> UnreadLedger {
    UnreadLedger::from_entries([(
        channel.to_string(),
        ConversationReadState {
            last_read: Some(last_read.to_string()),
            ..ConversationReadState::default()
        },
    )])
}

fn message(user: &str, ts: &str, text: &str) -> SlackMessage {
    SlackMessage {
        ts: ts.to_string(),
        user: Some(user.to_string()),
        text: Some(text.to_string()),
        ..SlackMessage::default()
    }
}

#[test]
fn feed_dedups_by_key_and_keeps_newest_first() {
    let mut feed = ActivityFeed::default();
    assert!(feed.merge([mention("C1", "10.0"), mention("C1", "30.0")]));
    assert!(feed.merge([mention("C2", "20.0")]));
    assert!(!feed.merge([mention("C1", "10.0")]));
    let order = feed.items().iter().map(|item| item.ts.as_str());
    assert_eq!(order.collect::<Vec<_>>(), ["30.0", "20.0", "10.0"]);
}

#[test]
fn feed_is_capped_and_drops_the_oldest() {
    let mut feed = ActivityFeed::default();
    feed.merge((1..=MAX_STORED_ITEMS + 5).map(|n| mention("C1", &format!("{n}.0"))));
    assert_eq!(feed.items().len(), MAX_STORED_ITEMS);
    assert_eq!(feed.items().last().unwrap().ts, "6.0");
    assert!(!feed.merge([mention("C1", "1.0")]), "older than the cap");
}

#[test]
fn reactions_aggregate_per_message_and_emoji_and_drop_when_empty() {
    let mut feed = ActivityFeed::default();
    assert!(feed.reaction_changed(&reaction("U1", true), "text", "100.0"));
    assert!(feed.reaction_changed(&reaction("U2", true), "text", "101.0"));
    assert!(!feed.reaction_changed(&reaction("U2", true), "text", "102.0"));
    assert_eq!(feed.items().len(), 1);
    let ActivityKind::Reaction { user_ids, .. } = &feed.items()[0].kind else {
        panic!("reaction item");
    };
    assert_eq!(user_ids, &["U1", "U2"]);
    assert_eq!(feed.items()[0].timestamp, "101.0");

    assert!(feed.reaction_changed(&reaction("U1", false), "", "103.0"));
    assert!(!feed.reaction_changed(&reaction("U1", false), "", "103.0"));
    assert!(feed.reaction_changed(&reaction("U2", false), "", "104.0"));
    assert!(feed.items().is_empty());
}

#[test]
fn stored_items_round_trip_through_serde_and_resort() {
    let mut feed = ActivityFeed::default();
    feed.merge([mention("C1", "10.0"), mention("C1", "20.0")]);
    feed.reaction_changed(&reaction("U1", true), "t", "15.0");
    let json = serde_json::to_string(feed.items()).unwrap();
    let loaded: Vec<ActivityItem> = serde_json::from_str(&json).unwrap();
    let mut restored = ActivityFeed::default();
    restored.merge(loaded.into_iter().rev());
    assert_eq!(restored, feed);
}

#[test]
fn mentions_are_direct_group_or_broadcast() {
    let in_group = |id: &str| id == "S1";
    let item = |text: &str| mention_item("C1", &message("U9", "1.0", text), "UME", &in_group);
    let kind = |text| item(text).map(|item| item.kind);
    let mention = |broadcast, group: Option<&str>| ActivityKind::Mention {
        broadcast,
        group: group.map(ToString::to_string),
    };
    assert_eq!(kind("hey <@UME|me>"), Some(mention(false, None)));
    assert_eq!(
        kind("<!subteam^S1|@ops> look"),
        Some(mention(false, Some("S1")))
    );
    assert_eq!(kind("<!subteam^S2> look"), None);
    assert_eq!(kind("<!here> look"), Some(mention(true, None)));
    assert_eq!(kind("hey <@UOTHER>"), None);
    assert_eq!(item("<@UME>").unwrap().author_id.as_deref(), Some("U9"));
}

#[test]
fn realtime_prefilter_keeps_only_mentions_and_reactions_to_my_messages() {
    let posted = |user: &str, text: &str, kind| {
        SocketModeEvent::Message(Box::new(SocketModeMessageEvent {
            channel_id: "C1".to_string(),
            message: message(user, "1.0", text),
            kind,
        }))
    };
    let seen = |event: &SocketModeEvent| observe_socket_event(event, Some("UME")).is_some();
    let posted_kind = SocketModeMessageKind::Posted;
    assert!(seen(&posted("U1", "hi <@UME>", posted_kind)));
    assert!(seen(&posted("U1", "<!channel> hi", posted_kind)));
    assert!(!seen(&posted("UME", "<@UME> note to self", posted_kind)));
    assert!(!seen(&posted("U1", "plain", posted_kind)));
    assert!(!seen(&posted(
        "U1",
        "<@UME>",
        SocketModeMessageKind::Changed
    )));
    let react = |item_user: Option<&str>, user: &str| {
        SocketModeEvent::Reaction(SocketModeReactionEvent {
            channel_id: "C1".to_string(),
            ts: "1.0".to_string(),
            name: "tada".to_string(),
            user_id: user.to_string(),
            item_user: item_user.map(Box::from),
            added: true,
        })
    };
    assert!(seen(&react(Some("UME"), "U1")));
    assert!(!seen(&react(Some("UME"), "UME")));
    assert!(!seen(&react(Some("U2"), "U1")));
    assert!(!seen(&react(None, "U1")));
}

#[test]
fn search_backfill_skips_own_and_unlocatable_matches() {
    let found = |user: &str, ts: Option<&str>, permalink: Option<&str>| SearchMatch {
        channel: Some(SlackSearchChannel {
            id: Some("C1".to_string()),
            ..SlackSearchChannel::default()
        }),
        user: Some(user.to_string()),
        text: Some("<@UME> ping".to_string()),
        ts: ts.map(ToString::to_string),
        permalink: permalink.map(ToString::to_string),
        ..SearchMatch::default()
    };
    let reply = "https://a.slack.com/archives/C1/p3000000?thread_ts=1.0";
    let items = items_from_search(
        &[
            found("U1", Some("2.0"), None),
            found("UME", Some("3.0"), None),
            found("U2", None, None),
            found("U3", Some("4.0"), Some(reply)),
        ],
        "UME",
    );
    let found = items.iter().map(|item| item.ts.as_str());
    assert_eq!(found.collect::<Vec<_>>(), ["2.0", "4.0"]);
    assert_eq!(items[1].thread_ts.as_deref(), Some("1.0"));
}

#[test]
fn backfill_runs_when_missing_or_older_than_fifteen_minutes() {
    let now = 10 * BACKFILL_MAX_AGE_MS;
    assert!(backfill_is_due(None, now));
    assert!(backfill_is_due(Some(now - BACKFILL_MAX_AGE_MS), now));
    assert!(!backfill_is_due(Some(now - BACKFILL_MAX_AGE_MS + 1), now));
}

#[test]
fn unread_is_derived_from_the_read_marker_and_never_stored() {
    let mut feed = ActivityFeed::default();
    feed.merge([mention("C1", "10.0"), mention("C1", "30.0")]);
    feed.merge([mention("C2", "40.0")]);
    let ledger = ledger("C1", "20.0");
    assert_eq!(badge_count(&feed, &ledger), 1, "C2 has no read state");
    let entries = entries(&feed, &ledger, &ThreadCatalog::default(), Some("UME"));
    let unread = entries.iter().map(|entry| entry.unread).collect::<Vec<_>>();
    assert_eq!(unread, [0, 1, 0]);
    assert!(!serde_json::to_string(&entries[1].item)
        .unwrap()
        .contains("unread"));
}

fn thread_record(root_ts: &str, latest: &str, read: Option<&str>) -> ThreadRecord {
    let mut record = ThreadRecord::placeholder(ThreadKey::new("C1", root_ts).unwrap());
    record.root = Some(message("UME", root_ts, "my thread"));
    record.latest_reply = Some(latest.to_string());
    record.reply_count = 3;
    record.last_read = read.map(ToString::to_string);
    record.participant_user_ids = ["UME", "U1", "U2", "U3"].map(String::from).into();
    record
}

#[test]
fn threads_are_derived_from_the_catalog_with_participants() {
    let mut catalog = ThreadCatalog::default();
    let mut unrelated = thread_record("7.0", "8.0", None);
    unrelated.root = Some(message("U9", "7.0", "not mine"));
    unrelated.participant_user_ids = ["U9".to_string()].into();
    catalog.upsert_records([
        thread_record("1.0", "50.0", Some("10.0")),
        thread_record("2.0", "60.0", Some("60.0")),
        unrelated,
    ]);
    let all = entries(
        &ActivityFeed::default(),
        &UnreadLedger::default(),
        &catalog,
        Some("UME"),
    );
    let roots = all.iter().map(|entry| entry.item.ts.as_str());
    assert_eq!(roots.collect::<Vec<_>>(), ["2.0", "1.0"]);
    assert_eq!((all[0].unread, all[1].unread), (0, 3));
    assert_eq!(all[1].item.author_id.as_deref(), Some("U1"));
    assert_eq!(all[1].item.participants, ["U2", "U3"]);
    assert_eq!(all[1].item.timestamp, "50.0");
    assert_eq!(header_counts(&all), (0, 1));
}

#[test]
fn filters_pick_mentions_or_threads_and_unread_only() {
    let mut feed = ActivityFeed::default();
    feed.merge([mention("C1", "10.0"), mention("C1", "30.0")]);
    feed.reaction_changed(&reaction("U1", true), "t", "35.0");
    let mut catalog = ThreadCatalog::default();
    catalog.upsert_records([thread_record("1.0", "50.0", None)]);
    let ledger = ledger("C1", "20.0");
    let all = || entries(&feed, &ledger, &catalog, Some("UME"));
    let count = |filter, unread_only| filter_entries(all(), filter, unread_only).len();
    assert_eq!(count(ActivityFilter::All, false), 4);
    assert_eq!(count(ActivityFilter::Mentions, false), 2);
    assert_eq!(count(ActivityFilter::Threads, false), 1);
    assert_eq!(count(ActivityFilter::Mentions, true), 1);
    assert_eq!(count(ActivityFilter::All, true), 3);
    assert_eq!(header_counts(&all()), (1, 1));
}
