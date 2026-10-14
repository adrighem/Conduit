use std::time::{SystemTime, UNIX_EPOCH};

use super::*;
use crate::workspace_pipeline::{
    MessageMutationKind, MutationOrigin, StoreBatch, StoreChange, WorkspaceBootstrapData,
    WorkspaceCoordinator, WorkspaceMutation, WorkspaceRevision,
};

#[test]
fn store_errors_classify_recovery_relevant_failures() {
    let rejected = StoreError::rejected_update("empty membership snapshot");
    let schema = StoreError::incompatible_schema(2, 1);
    let corrupt = StoreError::from(serde_json::from_str::<serde_json::Value>("{").unwrap_err());
    let local_io = StoreError::from(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "cache is read-only",
    ));

    assert_eq!(rejected.category(), StoreErrorCategory::RejectedUpdate);
    assert_eq!(schema.category(), StoreErrorCategory::IncompatibleSchema);
    assert_eq!(corrupt.category(), StoreErrorCategory::CorruptData);
    assert_eq!(local_io.category(), StoreErrorCategory::LocalIo);
}

#[test]
fn store_errors_preserve_database_sources() {
    let error = StoreError::from(rusqlite::Error::InvalidQuery);

    assert_eq!(error.category(), StoreErrorCategory::Unexpected);
    assert!(matches!(
        error,
        StoreError::Database(rusqlite::Error::InvalidQuery)
    ));
}

fn temp_cache_dir(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("conduit-{name}-{}-{unique}", std::process::id()))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime")
}

#[test]
fn coordinator_store_batches_commit_in_revision_order_and_suppress_replays() {
    let directory = temp_cache_dir("coordinator-store-order");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let first_revision = WorkspaceRevision::INITIAL.successor();
        let first = StoreBatch::new(
            first_revision,
            vec![
                StoreChange::ConversationUpsert(SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                }),
                StoreChange::ConversationUpsert(SlackConversation {
                    id: "C2".into(),
                    name: Some("random".into()),
                    ..Default::default()
                }),
            ],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(first.clone()).await.unwrap(),
            StoreBatchExecution::Committed
        );
        let after_first = store.hub().await.unwrap().metrics();

        assert_eq!(
            store.execute_store_batch(first).await.unwrap(),
            StoreBatchExecution::SkippedStale
        );
        let after_replay = store.hub().await.unwrap().metrics();
        assert_eq!(after_replay.transactions, after_first.transactions);

        let second_revision = first_revision.successor();
        let unchanged = StoreBatch::new(
            second_revision,
            vec![StoreChange::ConversationUpsert(SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                ..Default::default()
            })],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(unchanged).await.unwrap(),
            StoreBatchExecution::Unchanged
        );
        let after_unchanged = store.hub().await.unwrap().metrics();
        assert_eq!(after_unchanged.transactions, after_replay.transactions);

        let stale = StoreBatch::new(
            second_revision,
            vec![StoreChange::ConversationRemoved {
                channel_id: "C1".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(stale).await.unwrap(),
            StoreBatchExecution::SkippedStale
        );
        let delayed_revision = second_revision.successor();
        let forward_revision = delayed_revision.successor();
        let forward = StoreBatch::new(
            forward_revision,
            vec![StoreChange::ConversationUpsert(SlackConversation {
                id: "C4".into(),
                name: Some("forward".into()),
                ..Default::default()
            })],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(forward).await.unwrap(),
            StoreBatchExecution::Committed
        );
        let delayed = StoreBatch::new(
            delayed_revision,
            vec![StoreChange::ConversationRemoved {
                channel_id: "C2".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(delayed).await.unwrap(),
            StoreBatchExecution::SkippedStale
        );
        let conversations = store.load_conversations().await.unwrap().unwrap();
        assert_eq!(
            conversations
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["C1", "C2", "C4"])
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn coordinator_store_repair_replaces_a_partial_current_revision() {
    let directory = temp_cache_dir("coordinator-store-repair");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let revision = WorkspaceRevision::INITIAL.successor();
        let partial = StoreBatch::new(
            revision,
            vec![StoreChange::ConversationUpsert(SlackConversation {
                id: "C2".into(),
                ..Default::default()
            })],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(partial).await.unwrap(),
            StoreBatchExecution::Committed
        );

        let repair = StoreBatch::new(
            revision,
            vec![StoreChange::ConversationsRepaired(vec![
                SlackConversation {
                    id: "C1".into(),
                    ..Default::default()
                },
                SlackConversation {
                    id: "C2".into(),
                    ..Default::default()
                },
            ])],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_repair_batch(repair).await.unwrap(),
            StoreBatchExecution::Committed
        );
        assert_eq!(
            store
                .load_conversations()
                .await
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|conversation| conversation.id)
                .collect::<HashSet<_>>(),
            HashSet::from(["C1".to_string(), "C2".to_string()])
        );

        let stale = StoreBatch::new(
            revision,
            vec![StoreChange::ConversationRemoved {
                channel_id: "C1".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(stale).await.unwrap(),
            StoreBatchExecution::SkippedStale
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn coordinator_store_repair_preserves_a_row_mutation_queued_ahead_of_its_transaction() {
    let directory = temp_cache_dir("coordinator-store-repair-row-race");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let revision = WorkspaceRevision::INITIAL.successor();
        let stale = SlackConversation {
            id: "C1".into(),
            name: Some("general".into()),
            is_starred: Some(false),
            ..Default::default()
        };
        store
            .execute_store_batch(
                StoreBatch::new(
                    revision,
                    vec![StoreChange::ConversationsReplaced(vec![stale.clone()])],
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let repair = StoreBatch::new(
            revision,
            vec![StoreChange::ConversationsRepaired(vec![stale])],
        )
        .unwrap();

        let (started, writer_started) = tokio::sync::oneshot::channel();
        let (release_writer, release) = std::sync::mpsc::channel();
        let blocking_store = store.clone();
        let blocker = tokio::spawn(async move {
            blocking_store
                .occupy_writer_until(started, release)
                .await
                .unwrap();
        });
        writer_started.await.unwrap();

        let queued_topic = store.update_conversation("C1", |conversation| {
            conversation
                .extra
                .insert("topic".into(), serde_json::json!("Keep me"));
        });
        tokio::pin!(queued_topic);
        assert!(matches!(
            futures_util::poll!(&mut queued_topic),
            std::task::Poll::Pending
        ));
        let repair_write = store.execute_store_repair_batch(repair);
        tokio::pin!(repair_write);
        assert!(matches!(
            futures_util::poll!(&mut repair_write),
            std::task::Poll::Pending
        ));

        release_writer.send(()).unwrap();
        assert!(queued_topic.await.unwrap());
        repair_write.await.unwrap();
        blocker.await.unwrap();

        let stored = store.load_conversations().await.unwrap().unwrap();
        assert_eq!(
            stored[0].extra.get("topic"),
            Some(&serde_json::json!("Keep me"))
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn coordinator_store_batch_failure_rolls_back_data_and_revision() {
    let directory = temp_cache_dir("coordinator-store-rollback");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let baseline = store.hub().await.unwrap().metrics();
        let revision = WorkspaceRevision::INITIAL.successor();
        let invalid = StoreBatch::new(
            revision,
            vec![
                StoreChange::ConversationUpsert(SlackConversation {
                    id: "C1".into(),
                    name: Some("must-roll-back".into()),
                    ..Default::default()
                }),
                StoreChange::ConversationRemoved {
                    channel_id: String::new(),
                },
            ],
        )
        .unwrap();
        assert!(store.execute_store_batch(invalid).await.is_err());
        let after_failure = store.hub().await.unwrap().metrics();
        assert_eq!(after_failure.transactions, baseline.transactions);
        assert_eq!(
            after_failure.rolled_back_batches,
            baseline.rolled_back_batches + 1
        );
        assert!(store.load_conversations().await.unwrap().is_none());

        let retry = StoreBatch::new(
            revision,
            vec![StoreChange::ConversationUpsert(SlackConversation {
                id: "C1".into(),
                name: Some("retry-commits".into()),
                ..Default::default()
            })],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(retry).await.unwrap(),
            StoreBatchExecution::Committed
        );
        let conversations = store.load_conversations().await.unwrap().unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].name.as_deref(), Some("retry-commits"));
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn conversation_refresh_store_batch_rolls_back_and_recovers_as_one_unit() {
    let directory = temp_cache_dir("conversation-refresh-store-rollback");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let first_revision = WorkspaceRevision::INITIAL.successor();
        store
            .execute_store_batch(
                StoreBatch::new(
                    first_revision,
                    vec![StoreChange::ConversationUpsert(SlackConversation {
                        id: "C1".into(),
                        name: Some("old".into()),
                        is_starred: Some(true),
                        is_archived: Some(false),
                        ..Default::default()
                    })],
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let revision = first_revision.successor();
        let refresh_changes = vec![
            StoreChange::ConversationMetadataUpsert(SlackConversation {
                id: "C1".into(),
                name: Some("renamed".into()),
                is_archived: Some(true),
                ..Default::default()
            }),
            StoreChange::ConversationStarChanged {
                channel_id: "C1".into(),
                starred: false,
            },
        ];
        let mut failing_changes = refresh_changes.clone();
        failing_changes.push(StoreChange::ConversationRemoved {
            channel_id: String::new(),
        });
        assert!(store
            .execute_store_batch(StoreBatch::new(revision, failing_changes).unwrap())
            .await
            .is_err());

        let rolled_back = store.load_conversations().await.unwrap().unwrap();
        assert_eq!(rolled_back[0].name.as_deref(), Some("old"));
        assert_eq!(rolled_back[0].is_starred, Some(true));
        assert_eq!(rolled_back[0].is_archived, Some(false));

        assert_eq!(
            store
                .execute_store_batch(StoreBatch::new(revision, refresh_changes).unwrap())
                .await
                .unwrap(),
            StoreBatchExecution::Committed
        );
        let recovered = store.load_conversations().await.unwrap().unwrap();
        assert_eq!(recovered[0].name.as_deref(), Some("renamed"));
        assert_eq!(recovered[0].is_starred, Some(false));
        assert_eq!(recovered[0].is_archived, Some(true));
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn ephemeral_messages_are_never_persisted() {
    let directory = temp_cache_dir("ephemeral-not-persisted");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let revision = WorkspaceRevision::INITIAL.successor();
        let kept = SlackMessage {
            ts: "1.0".into(),
            text: Some("kept".into()),
            ..Default::default()
        };
        let preview = SlackMessage {
            ts: "2.0".into(),
            text: Some("giphy preview".into()),
            is_ephemeral: Some(true),
            ..Default::default()
        };
        store
            .execute_store_batch(
                StoreBatch::new(
                    revision,
                    vec![
                        StoreChange::HistoryReplaced {
                            channel_id: "C1".into(),
                            messages: vec![kept, preview.clone()],
                        },
                        StoreChange::MessageDelta {
                            channel_id: "C1".into(),
                            message: SlackMessage {
                                ts: "3.0".into(),
                                ..preview
                            },
                            kind: MessageMutationKind::Posted,
                        },
                    ],
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let history = store.load_history("C1").await.unwrap().unwrap();
        assert_eq!(
            history
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["1.0"]
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn coordinator_message_deltas_merge_with_unhydrated_sqlite_timelines() {
    let directory = temp_cache_dir("coordinator-message-delta-merge");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let first_revision = WorkspaceRevision::INITIAL.successor();
        let mut optimistic = SlackMessage {
            ts: "3.0".into(),
            text: Some("optimistic".into()),
            client_msg_id: Some("client-1".into()),
            ..Default::default()
        };
        optimistic.refresh_canonical_content();
        let root = SlackMessage {
            ts: "1.0".into(),
            text: Some("root".into()),
            ..Default::default()
        };
        let older = SlackMessage {
            ts: "2.0".into(),
            text: Some("older".into()),
            ..Default::default()
        };
        let older_reply = SlackMessage {
            ts: "1.1".into(),
            thread_ts: Some("1.0".into()),
            text: Some("older reply".into()),
            ..Default::default()
        };
        let cached_broadcast = SlackMessage {
            ts: "1.4".into(),
            thread_ts: Some("1.0".into()),
            subtype: Some("thread_broadcast".into()),
            client_msg_id: Some("projection-1".into()),
            text: Some("cached broadcast".into()),
            ..Default::default()
        };
        store
            .execute_store_batch(
                StoreBatch::new(
                    first_revision,
                    vec![
                        StoreChange::HistoryReplaced {
                            channel_id: "C1".into(),
                            messages: vec![
                                root.clone(),
                                older.clone(),
                                optimistic,
                                cached_broadcast,
                            ],
                        },
                        StoreChange::ThreadReplaced {
                            channel_id: "C1".into(),
                            thread_ts: "1.0".into(),
                            messages: vec![root.clone(), older_reply.clone()],
                        },
                    ],
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let mut authoritative = SlackMessage {
            ts: "4.0".into(),
            text: Some("authoritative".into()),
            client_msg_id: Some("client-1".into()),
            ..Default::default()
        };
        authoritative.refresh_canonical_content();
        let new_reply = SlackMessage {
            ts: "1.2".into(),
            thread_ts: Some("1.0".into()),
            text: Some("new reply".into()),
            ..Default::default()
        };
        let mut normal_reply_in_channel = new_reply.clone();
        normal_reply_in_channel.text = Some("must be filtered".into());
        normal_reply_in_channel.client_msg_id = Some("projection-1".into());
        let mut broadcast = new_reply.clone();
        broadcast.ts = "1.3".into();
        broadcast.subtype = Some("thread_broadcast".into());
        broadcast.text = Some("broadcast".into());
        let mutations = [
            (authoritative, MessageMutationKind::Changed),
            (normal_reply_in_channel, MessageMutationKind::Changed),
            (broadcast, MessageMutationKind::Posted),
            (
                SlackMessage {
                    ts: "2.0".into(),
                    text: Some("edited older".into()),
                    ..Default::default()
                },
                MessageMutationKind::Changed,
            ),
            (older_reply.clone(), MessageMutationKind::Deleted),
        ];
        let mut revision = first_revision;
        for (message, kind) in mutations {
            revision = revision.successor();
            store
                .execute_store_batch(
                    StoreBatch::new(
                        revision,
                        vec![StoreChange::MessageDelta {
                            channel_id: "C1".into(),
                            message,
                            kind,
                        }],
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }

        let history = store.load_history("C1").await.unwrap().unwrap();
        assert_eq!(
            history
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["4.0", "2.0", "1.3", "1.0"]
        );
        assert_eq!(
            history
                .iter()
                .find(|message| message.ts == "2.0")
                .and_then(|message| message.text.as_deref()),
            Some("edited older")
        );
        assert_eq!(
            history
                .iter()
                .filter(|message| message.client_msg_id.as_deref() == Some("client-1"))
                .count(),
            1,
            "the authoritative response must replace its optimistic identity"
        );
        assert!(!history.iter().any(|message| message.ts == "1.2"));

        let thread = store.load_thread("C1", "1.0").await.unwrap().unwrap();
        assert_eq!(
            thread
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["1.3", "1.2", "1.0"]
        );
        assert!(!thread.iter().any(|message| message.ts == older_reply.ts));
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn coordinator_message_reductions_merge_into_unhydrated_store_timelines() {
    let directory = temp_cache_dir("coordinator-message-reduction-merge");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        store
            .store_history(
                "C1",
                &[SlackMessage {
                    ts: "1.0".into(),
                    text: Some("persisted history".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        store
            .store_thread(
                "C1",
                "10.0",
                &[SlackMessage {
                    ts: "10.1".into(),
                    thread_ts: Some("10.0".into()),
                    text: Some("persisted reply".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        let mut coordinator = WorkspaceCoordinator::default();
        let history_reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: SlackMessage {
                    ts: "2.0".into(),
                    text: Some("new history".into()),
                    ..Default::default()
                },
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        store
            .execute_store_batch(history_reduction.store_batch().unwrap().clone())
            .await
            .unwrap();

        let thread_reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: SlackMessage {
                    ts: "10.2".into(),
                    thread_ts: Some("10.0".into()),
                    text: Some("new reply".into()),
                    ..Default::default()
                },
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        store
            .execute_store_batch(thread_reduction.store_batch().unwrap().clone())
            .await
            .unwrap();

        assert_eq!(
            store
                .load_history("C1")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["2.0", "1.0"]
        );
        assert_eq!(
            store
                .load_thread("C1", "10.0")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["10.2", "10.1"]
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn coordinator_same_identity_delete_removes_authoritative_unhydrated_timestamp() {
    let directory = temp_cache_dir("coordinator-message-identity-delete");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let mut coordinator = WorkspaceCoordinator::default();
        let mut optimistic = SlackMessage {
            ts: "10.0".into(),
            text: Some("optimistic".into()),
            client_msg_id: Some("client-1".into()),
            ..Default::default()
        };
        optimistic.refresh_canonical_content();
        coordinator.apply(WorkspaceMutation::MessageChanged {
            channel_id: "C1".into(),
            message: optimistic,
            kind: MessageMutationKind::Posted,
            origin: MutationOrigin::Local,
        });

        let mut authoritative = SlackMessage {
            ts: "11.0".into(),
            text: Some("authoritative".into()),
            client_msg_id: Some("client-1".into()),
            ..Default::default()
        };
        authoritative.refresh_canonical_content();
        store
            .store_history("C1", std::slice::from_ref(&authoritative))
            .await
            .unwrap();

        let deletion = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: authoritative,
                kind: MessageMutationKind::Deleted,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        store
            .execute_store_batch(deletion.store_batch().unwrap().clone())
            .await
            .unwrap();

        assert!(store.load_history("C1").await.unwrap().is_none());
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn coordinator_projection_edit_removes_authoritative_unhydrated_identity() {
    let directory = temp_cache_dir("coordinator-message-projection-edit");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let mut authoritative = SlackMessage {
            ts: "11.0".into(),
            thread_ts: Some("1.0".into()),
            subtype: Some("thread_broadcast".into()),
            text: Some("authoritative broadcast".into()),
            client_msg_id: Some("client-1".into()),
            ..Default::default()
        };
        authoritative.refresh_canonical_content();
        store
            .store_history("C1", std::slice::from_ref(&authoritative))
            .await
            .unwrap();
        store
            .store_thread("C1", "1.0", std::slice::from_ref(&authoritative))
            .await
            .unwrap();

        let mut coordinator = WorkspaceCoordinator::default();
        authoritative.subtype = None;
        authoritative.text = Some("normal reply".into());
        let edit = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: authoritative,
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        store
            .execute_store_batch(edit.store_batch().unwrap().clone())
            .await
            .unwrap();

        assert!(store.load_history("C1").await.unwrap().is_none());
        let thread = store.load_thread("C1", "1.0").await.unwrap().unwrap();
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].ts, "11.0");
        assert_eq!(thread[0].text.as_deref(), Some("normal reply"));
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn sqlite_only_reply_deletes_reconcile_both_root_copies_without_drift() {
    let directory = temp_cache_dir("coordinator-message-root-delete");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let root = |ts: &str, latest_reply: &str| SlackMessage {
            ts: ts.into(),
            text: Some("root".into()),
            reply_count: Some(2),
            latest_reply: Some(latest_reply.into()),
            reply_users: Some(vec!["U1".into(), "U2".into()]),
            ..Default::default()
        };
        let reply = |ts: &str, root_ts: &str, user: &str| SlackMessage {
            ts: ts.into(),
            thread_ts: Some(root_ts.into()),
            user: Some(user.into()),
            text: Some("reply".into()),
            ..Default::default()
        };
        let root_one = root("10.0", "12.0");
        let root_two = root("20.0", "22.0");
        let reply_11 = reply("11.0", "10.0", "U1");
        let reply_12 = reply("12.0", "10.0", "U2");
        let reply_21 = reply("21.0", "20.0", "U1");
        let reply_22 = reply("22.0", "20.0", "U2");
        store
            .store_history("C1", &[root_one.clone(), root_two.clone()])
            .await
            .unwrap();
        store
            .store_thread(
                "C1",
                "10.0",
                &[root_one.clone(), reply_11.clone(), reply_12],
            )
            .await
            .unwrap();
        store
            .store_thread(
                "C1",
                "20.0",
                &[root_two.clone(), reply_21.clone(), reply_22.clone()],
            )
            .await
            .unwrap();

        let mut coordinator = WorkspaceCoordinator::default();
        for deleted in [reply_11.clone(), reply_22.clone()] {
            let reduction = coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: deleted,
                    kind: MessageMutationKind::Deleted,
                    origin: MutationOrigin::Realtime,
                })
                .unwrap();
            store
                .execute_store_batch(reduction.store_batch().unwrap().clone())
                .await
                .unwrap();
        }
        store
            .execute_store_batch(
                StoreBatch::new(
                    coordinator.revision().successor(),
                    vec![StoreChange::MessageDelta {
                        channel_id: "C1".into(),
                        message: reply_22,
                        kind: MessageMutationKind::Deleted,
                    }],
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let history = store.load_history("C1").await.unwrap().unwrap();
        let thread_one = store.load_thread("C1", "10.0").await.unwrap().unwrap();
        let thread_two = store.load_thread("C1", "20.0").await.unwrap().unwrap();
        for root in [
            history.iter().find(|message| message.ts == "10.0").unwrap(),
            thread_one
                .iter()
                .find(|message| message.ts == "10.0")
                .unwrap(),
        ] {
            assert_eq!(root.reply_count, Some(1));
            assert_eq!(root.latest_reply.as_deref(), Some("12.0"));
            assert_eq!(root.reply_users.as_deref(), Some(&["U2".to_string()][..]));
        }
        for root in [
            history.iter().find(|message| message.ts == "20.0").unwrap(),
            thread_two
                .iter()
                .find(|message| message.ts == "20.0")
                .unwrap(),
        ] {
            assert_eq!(root.reply_count, Some(1));
            assert_eq!(root.latest_reply.as_deref(), Some("21.0"));
            assert_eq!(root.reply_users.as_deref(), Some(&["U1".to_string()][..]));
        }
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn partial_cached_reply_delete_preserves_users_and_unknown_delete_is_count_neutral() {
    let directory = temp_cache_dir("coordinator-message-partial-root-delete");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let root = SlackMessage {
            ts: "10.0".into(),
            reply_count: Some(3),
            latest_reply: Some("12.0".into()),
            reply_users: Some(vec!["U1".into(), "U2".into(), "U3".into()]),
            ..Default::default()
        };
        let reply = |ts: &str, user: &str| SlackMessage {
            ts: ts.into(),
            thread_ts: Some("10.0".into()),
            user: Some(user.into()),
            ..Default::default()
        };
        let persisted = reply("11.0", "U1");
        store
            .store_history("C1", std::slice::from_ref(&root))
            .await
            .unwrap();
        store
            .store_thread(
                "C1",
                "10.0",
                &[root.clone(), persisted.clone(), reply("12.0", "U2")],
            )
            .await
            .unwrap();

        let first_revision = WorkspaceRevision::INITIAL.successor();
        store
            .execute_store_batch(
                StoreBatch::new(
                    first_revision,
                    vec![StoreChange::MessageDelta {
                        channel_id: "C1".into(),
                        message: persisted,
                        kind: MessageMutationKind::Deleted,
                    }],
                )
                .unwrap(),
            )
            .await
            .unwrap();
        store
            .execute_store_batch(
                StoreBatch::new(
                    first_revision.successor(),
                    vec![StoreChange::MessageDelta {
                        channel_id: "C1".into(),
                        message: reply("13.0", "U4"),
                        kind: MessageMutationKind::Deleted,
                    }],
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let history = store.load_history("C1").await.unwrap().unwrap();
        let thread = store.load_thread("C1", "10.0").await.unwrap().unwrap();
        let history_root = history.iter().find(|message| message.ts == "10.0").unwrap();
        let thread_root = thread.iter().find(|message| message.ts == "10.0").unwrap();
        assert_eq!(history_root, thread_root);
        assert_eq!(history_root.reply_count, Some(2));
        assert_eq!(history_root.latest_reply.as_deref(), Some("12.0"));
        assert_eq!(
            history_root.reply_users.as_deref(),
            Some(&["U1".to_string(), "U2".to_string(), "U3".to_string()][..])
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn sqlite_only_post_edit_and_move_reconcile_both_root_copies() {
    let directory = temp_cache_dir("coordinator-message-root-transitions");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let first_root = SlackMessage {
            ts: "10.0".into(),
            reply_count: Some(1),
            latest_reply: Some("11.0".into()),
            reply_users: Some(vec!["U1".into()]),
            ..Default::default()
        };
        let second_root = SlackMessage {
            ts: "20.0".into(),
            reply_count: Some(0),
            reply_users: Some(Vec::new()),
            ..Default::default()
        };
        let existing = SlackMessage {
            ts: "11.0".into(),
            thread_ts: Some("10.0".into()),
            user: Some("U1".into()),
            ..Default::default()
        };
        store
            .store_history("C1", &[first_root.clone(), second_root.clone()])
            .await
            .unwrap();
        store
            .store_thread("C1", "10.0", &[first_root.clone(), existing.clone()])
            .await
            .unwrap();
        store
            .store_thread("C1", "20.0", std::slice::from_ref(&second_root))
            .await
            .unwrap();

        let mut coordinator = WorkspaceCoordinator::default();
        let mut reply = SlackMessage {
            ts: "12.0".into(),
            thread_ts: Some("10.0".into()),
            client_msg_id: Some("reply-2".into()),
            user: Some("U2".into()),
            text: Some("posted".into()),
            ..Default::default()
        };
        for kind in [
            MessageMutationKind::Posted,
            MessageMutationKind::Changed,
            MessageMutationKind::Changed,
        ] {
            if kind == MessageMutationKind::Changed && reply.user.as_deref() == Some("U2") {
                reply.user = Some("U3".into());
                reply.text = Some("edited".into());
            } else if kind == MessageMutationKind::Changed {
                reply.thread_ts = Some("20.0".into());
                reply.text = Some("moved".into());
            }
            let reduction = coordinator
                .apply(WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: reply.clone(),
                    kind,
                    origin: MutationOrigin::Realtime,
                })
                .unwrap();
            store
                .execute_store_batch(reduction.store_batch().unwrap().clone())
                .await
                .unwrap();
        }

        let history = store.load_history("C1").await.unwrap().unwrap();
        let first_thread = store.load_thread("C1", "10.0").await.unwrap().unwrap();
        let second_thread = store.load_thread("C1", "20.0").await.unwrap().unwrap();
        let root_copy = |messages: &[SlackMessage], root_ts: &str| {
            messages
                .iter()
                .find(|message| message.ts == root_ts)
                .cloned()
                .unwrap()
        };
        assert_eq!(
            root_copy(&history, "10.0"),
            root_copy(&first_thread, "10.0")
        );
        assert_eq!(root_copy(&history, "10.0").reply_count, Some(1));
        assert_eq!(
            root_copy(&history, "10.0").latest_reply.as_deref(),
            Some("11.0")
        );
        assert_eq!(
            root_copy(&history, "10.0").reply_users.as_deref(),
            Some(&["U1".to_string()][..])
        );
        assert_eq!(
            root_copy(&history, "20.0"),
            root_copy(&second_thread, "20.0")
        );
        assert_eq!(root_copy(&history, "20.0").reply_count, Some(1));
        assert_eq!(
            root_copy(&history, "20.0").latest_reply.as_deref(),
            Some("12.0")
        );
        assert_eq!(
            root_copy(&history, "20.0").reply_users.as_deref(),
            Some(&["U3".to_string()][..])
        );
        assert!(!first_thread.iter().any(|message| message.ts == "12.0"));
        assert_eq!(
            second_thread
                .iter()
                .find(|message| message.ts == "12.0")
                .and_then(|message| message.text.as_deref()),
            Some("moved")
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn changed_thread_root_updates_its_cached_copy_without_cross_channel_scan() {
    let directory = temp_cache_dir("coordinator-message-root-edit-scope");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let root = SlackMessage {
            ts: "10.0".into(),
            thread_ts: Some("10.0".into()),
            text: Some("old root".into()),
            reply_count: Some(2),
            latest_reply: Some("12.0".into()),
            reply_users: Some(vec!["U1".into(), "U2".into()]),
            ..Default::default()
        };
        let other_root = SlackMessage {
            ts: "10.0".into(),
            thread_ts: Some("10.0".into()),
            text: Some("other channel".into()),
            ..Default::default()
        };
        store
            .store_history("C1", std::slice::from_ref(&root))
            .await
            .unwrap();
        store
            .store_thread("C1", "10.0", std::slice::from_ref(&root))
            .await
            .unwrap();
        store
            .store_thread("C10", "10.0", std::slice::from_ref(&other_root))
            .await
            .unwrap();

        let current = SlackMessage {
            ts: "10.0".into(),
            text: Some("current root".into()),
            ..Default::default()
        };
        let mut coordinator = WorkspaceCoordinator::default();
        let reduction = coordinator
            .apply(WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: current,
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            })
            .unwrap();
        store
            .execute_store_batch(reduction.store_batch().unwrap().clone())
            .await
            .unwrap();

        let history = store.load_history("C1").await.unwrap().unwrap();
        let thread = store.load_thread("C1", "10.0").await.unwrap().unwrap();
        assert_eq!(history[0], thread[0]);
        assert_eq!(history[0].text.as_deref(), Some("current root"));
        assert_eq!(history[0].reply_count, Some(2));
        assert_eq!(history[0].latest_reply.as_deref(), Some("12.0"));
        assert_eq!(
            history[0].reply_users.as_deref(),
            Some(&["U1".to_string(), "U2".to_string()][..])
        );
        assert_eq!(
            store.load_thread("C10", "10.0").await.unwrap().unwrap()[0]
                .text
                .as_deref(),
            Some("other channel")
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn reply_aggregate_delta_preserves_projection_specific_root_content() {
    let directory = temp_cache_dir("coordinator-message-root-content");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let history_root = SlackMessage {
            ts: "10.0".into(),
            text: Some("channel snapshot".into()),
            reply_count: Some(0),
            ..Default::default()
        };
        let thread_root = SlackMessage {
            text: Some("newer thread snapshot".into()),
            ..history_root.clone()
        };
        store
            .store_history("C1", std::slice::from_ref(&history_root))
            .await
            .unwrap();
        store
            .store_thread("C1", "10.0", std::slice::from_ref(&thread_root))
            .await
            .unwrap();

        store
            .execute_store_batch(
                StoreBatch::new(
                    WorkspaceRevision::INITIAL.successor(),
                    vec![StoreChange::MessageDelta {
                        channel_id: "C1".into(),
                        message: SlackMessage {
                            ts: "11.0".into(),
                            thread_ts: Some("10.0".into()),
                            user: Some("U1".into()),
                            ..Default::default()
                        },
                        kind: MessageMutationKind::Posted,
                    }],
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let history = store.load_history("C1").await.unwrap().unwrap();
        let thread = store.load_thread("C1", "10.0").await.unwrap().unwrap();
        let history_root = history.iter().find(|message| message.ts == "10.0").unwrap();
        let thread_root = thread.iter().find(|message| message.ts == "10.0").unwrap();
        assert_eq!(history_root.text.as_deref(), Some("channel snapshot"));
        assert_eq!(thread_root.text.as_deref(), Some("newer thread snapshot"));
        assert_eq!(history_root.reply_count, Some(1));
        assert_eq!(thread_root.reply_count, Some(1));
        assert_eq!(history_root.latest_reply, thread_root.latest_reply);
        assert_eq!(history_root.reply_users, thread_root.reply_users);
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn ordinary_post_skips_unrelated_same_channel_thread_payloads() {
    let directory = temp_cache_dir("coordinator-message-post-thread-scope");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        store
            .store_thread(
                "C1",
                "10.0",
                &[SlackMessage {
                    ts: "10.0".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
    });
    Connection::open(store.database_path())
        .unwrap()
        .execute(
            "UPDATE workspace_items SET payload_json = '{broken'
             WHERE workspace_key = ?1
               AND kind = 'thread_replies'
               AND item_key = 'C1:10.0'",
            [&store.workspace_key],
        )
        .unwrap();

    runtime().block_on(async {
        assert_eq!(
            store
                .execute_store_batch(
                    StoreBatch::new(
                        WorkspaceRevision::INITIAL.successor(),
                        vec![StoreChange::MessageDelta {
                            channel_id: "C1".into(),
                            message: SlackMessage {
                                ts: "20.0".into(),
                                text: Some("ordinary post".into()),
                                ..Default::default()
                            },
                            kind: MessageMutationKind::Posted,
                        }],
                    )
                    .unwrap(),
                )
                .await
                .unwrap(),
            StoreBatchExecution::Committed
        );
        assert_eq!(
            store.load_history("C1").await.unwrap().unwrap()[0]
                .text
                .as_deref(),
            Some("ordinary post")
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn coordinator_message_delta_batch_rolls_back_wholly_and_can_recover() {
    let directory = temp_cache_dir("coordinator-message-delta-rollback");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let first_revision = WorkspaceRevision::INITIAL.successor();
        let root = SlackMessage {
            ts: "1.0".into(),
            text: Some("root".into()),
            ..Default::default()
        };
        store
            .execute_store_batch(
                StoreBatch::new(
                    first_revision,
                    vec![
                        StoreChange::HistoryReplaced {
                            channel_id: "C1".into(),
                            messages: vec![root.clone()],
                        },
                        StoreChange::ThreadReplaced {
                            channel_id: "C1".into(),
                            thread_ts: "1.0".into(),
                            messages: vec![root.clone()],
                        },
                    ],
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let reply = SlackMessage {
            ts: "2.0".into(),
            thread_ts: Some("1.0".into()),
            text: Some("reply".into()),
            ..Default::default()
        };
        let revision = first_revision.successor();
        let deltas = vec![
            StoreChange::MessageDelta {
                channel_id: "C1".into(),
                message: SlackMessage {
                    ts: "3.0".into(),
                    text: Some("new history".into()),
                    ..Default::default()
                },
                kind: MessageMutationKind::Posted,
            },
            StoreChange::MessageDelta {
                channel_id: "C1".into(),
                message: reply.clone(),
                kind: MessageMutationKind::Posted,
            },
        ];
        let mut failing_changes = deltas.clone();
        failing_changes.push(StoreChange::ConversationRemoved {
            channel_id: String::new(),
        });
        assert!(store
            .execute_store_batch(StoreBatch::new(revision, failing_changes).unwrap())
            .await
            .is_err());
        assert_eq!(
            store
                .load_history("C1")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["1.0"]
        );
        assert_eq!(
            store
                .load_thread("C1", "1.0")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["1.0"]
        );

        assert_eq!(
            store
                .execute_store_batch(StoreBatch::new(revision, deltas).unwrap())
                .await
                .unwrap(),
            StoreBatchExecution::Committed
        );
        assert_eq!(
            store
                .load_history("C1")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["3.0", "1.0"]
        );
        assert_eq!(
            store
                .load_thread("C1", "1.0")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["2.0", "1.0"]
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn coordinator_user_store_changes_persist_only_safe_projections() {
    let directory = temp_cache_dir("coordinator-user-projection");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let revision = WorkspaceRevision::INITIAL.successor();
        let batch = StoreBatch::new(
            revision,
            vec![StoreChange::UsersReplaced(vec![SlackUser {
                id: Some("U1".into()),
                name: Some("ada".into()),
                profile: Some(crate::models::SlackUserProfile {
                    display_name: Some("Ada".into()),
                    real_name: Some("Ada Lovelace".into()),
                    phone: Some("PrivatePhoneCanary".into()),
                    email: Some("PrivateEmailCanary".into()),
                    huddle_state_call_id: Some("PrivateHuddleCanary".into()),
                    fields: HashMap::from([(
                        "private".into(),
                        crate::models::SlackProfileField {
                            value: Some("PrivateCustomFieldCanary".into()),
                            ..Default::default()
                        },
                    )]),
                    ..Default::default()
                }),
                ..Default::default()
            }])],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(batch).await.unwrap(),
            StoreBatchExecution::Committed
        );

        assert_eq!(
            store.load_user_names().await.unwrap().get("U1"),
            Some(&"Ada".to_string())
        );
        let payloads = store
            .hub()
            .await
            .unwrap()
            .query(|connection| {
                let mut statement = connection
                    .prepare("SELECT payload_json FROM workspace_items ORDER BY kind, item_key")?;
                let payloads = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(payloads.join("\n"))
            })
            .await
            .unwrap();
        for private_value in [
            "PrivatePhoneCanary",
            "PrivateEmailCanary",
            "PrivateHuddleCanary",
            "PrivateCustomFieldCanary",
        ] {
            assert!(!payloads.contains(private_value));
        }
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn coordinator_user_upsert_preserves_sparse_status_and_deletes_explicit_clear() {
    let directory = temp_cache_dir("coordinator-user-status-clear");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let first_revision = WorkspaceRevision::INITIAL.successor();
        let active = StoreBatch::new(
            first_revision,
            vec![StoreChange::UserUpsert(SlackUser {
                id: Some("U1".into()),
                profile: Some(crate::models::SlackUserProfile {
                    status_text: Some("Heads down".into()),
                    status_emoji: Some(":construction:".into()),
                    ..Default::default()
                }),
                ..Default::default()
            })],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(active).await.unwrap(),
            StoreBatchExecution::Committed
        );

        let sparse = StoreBatch::new(
            first_revision.successor(),
            vec![StoreChange::UserUpsert(SlackUser {
                id: Some("U1".into()),
                profile: Some(crate::models::SlackUserProfile {
                    huddle_state_call_id: Some("R1".into()),
                    ..Default::default()
                }),
                ..Default::default()
            })],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(sparse).await.unwrap(),
            StoreBatchExecution::Unchanged
        );
        assert_eq!(
            store.load_user_statuses().await.unwrap()["U1"].text,
            "Heads down"
        );

        let cleared = StoreBatch::new(
            first_revision.successor().successor(),
            vec![StoreChange::UserUpsert(SlackUser {
                id: Some("U1".into()),
                profile: Some(crate::models::SlackUserProfile {
                    status_text: Some(String::new()),
                    status_emoji: Some(String::new()),
                    status_expiration: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            })],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(cleared).await.unwrap(),
            StoreBatchExecution::Committed
        );
        assert!(!store.load_user_statuses().await.unwrap().contains_key("U1"));
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn coordinator_store_executor_handles_every_change_variant() {
    let directory = temp_cache_dir("coordinator-all-store-changes");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let root = SlackMessage {
            ts: "1.000".into(),
            reply_count: Some(1),
            ..Default::default()
        };
        let mut thread_catalog = ThreadCatalog::default();
        thread_catalog.observe_history("C1", std::slice::from_ref(&root));
        let first_revision = WorkspaceRevision::INITIAL.successor();
        let bootstrap = StoreBatch::new(
            first_revision,
            vec![StoreChange::BootstrapReplaced(WorkspaceBootstrapData {
                conversations: vec![SlackConversation {
                    id: "C1".into(),
                    name: Some("bootstrap".into()),
                    ..Default::default()
                }],
                users: vec![SlackUser {
                    id: Some("U1".into()),
                    name: Some("bootstrap-user".into()),
                    ..Default::default()
                }],
                histories: HashMap::from([(
                    "C1".into(),
                    vec![SlackMessage {
                        ts: "1.000".into(),
                        text: Some("bootstrap-history".into()),
                        ..Default::default()
                    }],
                )]),
                threads: thread_catalog.into_records(),
                ..Default::default()
            })],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(bootstrap).await.unwrap(),
            StoreBatchExecution::Committed
        );

        let second_revision = first_revision.successor();
        let replacement = StoreBatch::new(
            second_revision,
            vec![
                StoreChange::ConversationsReplaced(vec![SlackConversation {
                    id: "C2".into(),
                    name: Some("replace".into()),
                    ..Default::default()
                }]),
                StoreChange::ConversationsRepaired(vec![SlackConversation {
                    id: "C2".into(),
                    name: Some("replace".into()),
                    ..Default::default()
                }]),
                StoreChange::ConversationUpsert(SlackConversation {
                    id: "C3".into(),
                    name: Some("upsert".into()),
                    ..Default::default()
                }),
                StoreChange::ConversationMetadataUpsert(SlackConversation {
                    id: "C3".into(),
                    name: Some("metadata".into()),
                    ..Default::default()
                }),
                StoreChange::ConversationStarChanged {
                    channel_id: "C3".into(),
                    starred: false,
                },
                StoreChange::ConversationMembershipUpsert(SlackConversation {
                    id: "C3".into(),
                    is_starred: Some(true),
                    ..Default::default()
                }),
                StoreChange::ConversationRemoved {
                    channel_id: "C2".into(),
                },
                StoreChange::UsersReplaced(vec![SlackUser {
                    id: Some("U2".into()),
                    name: Some("replace-user".into()),
                    ..Default::default()
                }]),
                StoreChange::UserUpsert(SlackUser {
                    id: Some("U3".into()),
                    name: Some("upsert-user".into()),
                    ..Default::default()
                }),
                StoreChange::HistoryReplaced {
                    channel_id: "C3".into(),
                    messages: vec![SlackMessage {
                        ts: "2.000".into(),
                        text: Some("replacement-history".into()),
                        ..Default::default()
                    }],
                },
                StoreChange::MessageDelta {
                    channel_id: "C3".into(),
                    message: SlackMessage {
                        ts: "2.100".into(),
                        text: Some("delta-history".into()),
                        ..Default::default()
                    },
                    kind: MessageMutationKind::Posted,
                },
                StoreChange::HistoryRemoved {
                    channel_id: "C1".into(),
                },
                StoreChange::ThreadReplaced {
                    channel_id: "C3".into(),
                    thread_ts: "2.000".into(),
                    messages: vec![SlackMessage {
                        ts: "2.001".into(),
                        thread_ts: Some("2.000".into()),
                        text: Some("replacement-thread".into()),
                        ..Default::default()
                    }],
                },
                StoreChange::ThreadCatalogReplaced(Vec::new()),
            ],
        )
        .unwrap();
        assert_eq!(
            store.execute_store_batch(replacement).await.unwrap(),
            StoreBatchExecution::Committed
        );

        let conversations = store.load_conversations().await.unwrap().unwrap();
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].id, "C3");
        assert_eq!(conversations[0].name.as_deref(), Some("metadata"));
        assert_eq!(conversations[0].is_starred, Some(true));
        let names = store.load_user_names().await.unwrap();
        assert_eq!(names.get("U2"), Some(&"replace-user".to_string()));
        assert_eq!(names.get("U3"), Some(&"upsert-user".to_string()));
        assert!(!names.contains_key("U1"));
        assert!(store.load_history("C1").await.unwrap().is_none());
        assert_eq!(
            store.load_history("C3").await.unwrap().unwrap()[0].body_text(),
            "delta-history"
        );
        assert_eq!(
            store.load_thread("C3", "2.000").await.unwrap().unwrap()[0].body_text(),
            "replacement-thread"
        );
        assert!(store.load_thread_catalog().await.unwrap().is_empty());
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_round_trips_cached_snapshots() {
    let directory = temp_cache_dir("workspace-store");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        assert!(store
            .load_conversations()
            .await
            .expect("conversation load failed")
            .is_none());

        let conversations = vec![SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            ..Default::default()
        }];
        store
            .store_conversations(&conversations)
            .await
            .expect("conversation store failed");
        assert_eq!(
            store
                .load_bootstrap()
                .await
                .expect("workspace bootstrap load failed")
                .expect("missing cached workspace bootstrap")
                .workspace_id,
            "T123:U123"
        );
        assert_eq!(
            store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations")[0]
                .id,
            "C123"
        );

        let messages = vec![SlackMessage {
            ts: "1710000000.000100".to_string(),
            text: Some("cached".to_string()),
            ..Default::default()
        }];
        store
            .store_history("C123", &messages)
            .await
            .expect("history store failed");
        assert_eq!(
            store
                .load_history("C123")
                .await
                .expect("history load failed")
                .expect("missing cached history")[0]
                .body_text(),
            "cached"
        );

        store
            .store_thread("C123", "1710000000.000100", &messages)
            .await
            .expect("thread store failed");
        assert_eq!(
            store
                .load_thread("C123", "1710000000.000100")
                .await
                .expect("thread load failed")
                .expect("missing cached thread")[0]
                .ts,
            "1710000000.000100"
        );

        let emojis = HashMap::from([
            (
                "party_parrot".to_string(),
                "https://emoji.example/parrot.gif".to_string(),
            ),
            ("ship_it".to_string(), "alias:rocket".to_string()),
        ]);
        store
            .store_custom_emojis(&emojis)
            .await
            .expect("emoji store failed");
        assert_eq!(
            store.load_custom_emojis().await.expect("emoji load failed"),
            emojis
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn schema_v1_migrates_to_v2_without_losing_keyed_payloads() {
    let directory = temp_cache_dir("workspace-schema-v2-migration");
    std::fs::create_dir_all(&directory).unwrap();
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let connection = Connection::open(store.database_path()).unwrap();
    connection
        .execute_batch(
            "PRAGMA user_version = 1;
             CREATE TABLE workspaces (
                 workspace_key TEXT PRIMARY KEY,
                 workspace_id TEXT NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE app_state (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 active_workspace_key TEXT REFERENCES workspaces(workspace_key)
             );
             INSERT INTO app_state(singleton, active_workspace_key) VALUES (1, NULL);
             CREATE TABLE workspace_items (
                 workspace_key TEXT NOT NULL REFERENCES workspaces(workspace_key) ON DELETE CASCADE,
                 kind TEXT NOT NULL,
                 item_key TEXT NOT NULL,
                 payload_json TEXT NOT NULL,
                 PRIMARY KEY (workspace_key, kind, item_key)
             ) WITHOUT ROWID;",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO workspaces(workspace_key, workspace_id) VALUES (?1, 'T123:U123')",
            [&store.workspace_key],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
             VALUES (?1, 'conversation', 'C1', ?2)",
            params![
                &store.workspace_key,
                serde_json::to_string(&SlackConversation {
                    id: "C1".into(),
                    name: Some("general".into()),
                    ..Default::default()
                })
                .unwrap()
            ],
        )
        .unwrap();
    drop(connection);

    let conversations = runtime()
        .block_on(store.load_conversations())
        .unwrap()
        .unwrap();
    assert_eq!(conversations[0].id, "C1");

    let connection = Connection::open(store.database_path()).unwrap();
    let version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, DATABASE_SCHEMA_VERSION);
    let metadata_columns: Vec<String> = connection
        .prepare("PRAGMA table_info(sync_metadata)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(
        metadata_columns,
        [
            "workspace_key",
            "operation",
            "target",
            "refreshed_at_ms",
            "retry_count",
            "retry_after_ms"
        ]
    );
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn corrupt_database_is_recreated_as_an_empty_v2_cache() {
    let directory = temp_cache_dir("workspace-corrupt-database-reset");
    std::fs::create_dir_all(&directory).unwrap();
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    std::fs::write(store.database_path(), b"not a sqlite database").unwrap();

    assert!(runtime()
        .block_on(store.load_conversations())
        .unwrap()
        .is_none());
    let connection = Connection::open(store.database_path()).unwrap();
    let version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, DATABASE_SCHEMA_VERSION);
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn failed_v1_metadata_migration_recreates_only_the_derived_cache() {
    let directory = temp_cache_dir("workspace-failed-v1-migration-reset");
    std::fs::create_dir_all(&directory).unwrap();
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let connection = Connection::open(store.database_path()).unwrap();
    connection
        .execute_batch(
            "PRAGMA user_version = 1;
             CREATE TABLE sync_metadata (broken TEXT);",
        )
        .unwrap();
    drop(connection);
    let credentials_sentinel = directory.join("credentials-are-external");
    let drafts_sentinel = directory.join("drafts-are-external");
    std::fs::write(&credentials_sentinel, "preserve").unwrap();
    std::fs::write(&drafts_sentinel, "preserve").unwrap();

    assert!(runtime()
        .block_on(store.load_conversations())
        .unwrap()
        .is_none());
    assert_eq!(
        std::fs::read_to_string(credentials_sentinel).unwrap(),
        "preserve"
    );
    assert_eq!(
        std::fs::read_to_string(drafts_sentinel).unwrap(),
        "preserve"
    );
    let connection = Connection::open(store.database_path()).unwrap();
    let version: u32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, DATABASE_SCHEMA_VERSION);
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn malformed_keyed_payload_resets_the_workspace_cache() {
    let directory = temp_cache_dir("workspace-malformed-payload-reset");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        store
            .store_conversations(&[SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                ..Default::default()
            }])
            .await
            .unwrap();
    });
    let connection = Connection::open(store.database_path()).unwrap();
    connection
        .execute(
            "UPDATE workspace_items SET payload_json = '{broken'
             WHERE workspace_key = ?1 AND kind = 'conversation'",
            [&store.workspace_key],
        )
        .unwrap();
    drop(connection);

    assert!(runtime()
        .block_on(store.load_conversations())
        .unwrap()
        .is_none());
    let remaining: u32 = Connection::open(store.database_path())
        .unwrap()
        .query_row("SELECT count(*) FROM workspace_items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 0);
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn store_hub_reuses_one_writer_and_two_query_only_readers() {
    let directory = temp_cache_dir("store-hub-connections");
    runtime().block_on(async {
        let hub = StoreHub::open(directory.clone()).await.unwrap();

        let first_writer = hub
            .write(|connection| Ok(connection as *const Connection as usize))
            .await
            .unwrap();
        let second_writer = hub
            .write(|connection| Ok(connection as *const Connection as usize))
            .await
            .unwrap();
        assert_eq!(first_writer, second_writer);

        let mut readers = Vec::new();
        for _ in 0..4 {
            readers.push(
                hub.query(|connection| {
                    let query_only: bool =
                        connection.query_row("PRAGMA query_only", [], |row| row.get(0))?;
                    Ok((connection as *const Connection as usize, query_only))
                })
                .await
                .unwrap(),
            );
        }
        assert!(readers.iter().all(|(_, query_only)| *query_only));
        assert_eq!(readers[0].0, readers[2].0);
        assert_eq!(readers[1].0, readers[3].0);
        assert_ne!(readers[0].0, readers[1].0);
        assert_ne!(first_writer, readers[0].0);
        assert_ne!(first_writer, readers[1].0);

        hub.shutdown().await.unwrap();
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn store_hub_commit_barrier_makes_writer_changes_visible_to_readers() {
    let directory = temp_cache_dir("store-hub-barrier");
    runtime().block_on(async {
        let hub = StoreHub::open(directory.clone()).await.unwrap();
        hub.write(|connection| {
            connection.execute_batch(
                "CREATE TABLE barrier_probe (value INTEGER NOT NULL);
                 INSERT INTO barrier_probe(value) VALUES (42);",
            )?;
            Ok(())
        })
        .await
        .unwrap();
        hub.barrier().await.unwrap();

        for _ in 0..2 {
            let value: i64 = hub
                .query(|connection| {
                    Ok(connection
                        .query_row("SELECT value FROM barrier_probe", [], |row| row.get(0))?)
                })
                .await
                .unwrap();
            assert_eq!(value, 42);
        }
        hub.shutdown().await.unwrap();
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn store_hub_query_does_not_contend_on_admission_lock() {
    let directory = temp_cache_dir("store-hub-query-uncontended");
    runtime().block_on(async {
        let hub = StoreHub::open(directory.clone()).await.unwrap();
        hub.write(|connection| {
            connection.execute_batch(
                "CREATE TABLE uncontended_probe (value INTEGER NOT NULL);
                 INSERT INTO uncontended_probe(value) VALUES (99);",
            )?;
            Ok(())
        })
        .await
        .unwrap();
        hub.barrier().await.unwrap();

        let admission_guard = hub.inner.admission.lock().await;

        let value: i64 = hub
            .query(|connection| {
                Ok(connection
                    .query_row("SELECT value FROM uncontended_probe", [], |row| row.get(0))?)
            })
            .await
            .unwrap();
        assert_eq!(value, 99);

        drop(admission_guard);
        hub.shutdown().await.unwrap();
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn store_hub_shutdown_drains_queued_writes_and_rejects_new_work() {
    let directory = temp_cache_dir("store-hub-shutdown");
    runtime().block_on(async {
        let hub = StoreHub::open(directory.clone()).await.unwrap();
        let (started, wait_for_start) = std::sync::mpsc::channel();
        let (release, wait_for_release) = std::sync::mpsc::channel();
        let active = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.write(move |connection| {
                    started.send(()).unwrap();
                    wait_for_release.recv().unwrap();
                    connection.execute_batch(
                        "CREATE TABLE shutdown_probe (value INTEGER NOT NULL);
                         INSERT INTO shutdown_probe(value) VALUES (7);",
                    )?;
                    Ok(())
                })
                .await
            })
        };
        tokio::task::spawn_blocking(move || wait_for_start.recv().unwrap())
            .await
            .unwrap();
        let queued = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.write(|connection| {
                    connection.execute("INSERT INTO shutdown_probe(value) VALUES (8)", [])?;
                    Ok(())
                })
                .await
            })
        };
        while hub.inner.writer.capacity() == hub.inner.writer.max_capacity() {
            tokio::task::yield_now().await;
        }
        let shutdown = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.shutdown().await })
        };
        assert!(!shutdown.is_finished());
        release.send(()).unwrap();
        active.await.unwrap().unwrap();
        queued.await.unwrap().unwrap();
        shutdown.await.unwrap().unwrap();

        let connection = Connection::open(directory.join(DATABASE_FILENAME)).unwrap();
        let values: i64 = connection
            .query_row("SELECT sum(value) FROM shutdown_probe", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(values, 15);
        assert!(matches!(
            hub.write(|_| Ok(())).await,
            Err(StoreError::HubClosed)
        ));
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_bootstrap_loads_all_startup_domains_in_one_projection() {
    let directory = temp_cache_dir("workspace-bootstrap");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        store
            .store_conversations(&[SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                ..Default::default()
            }])
            .await
            .unwrap();
        store
            .store_user_names(&HashMap::from([("U1".into(), "Ada".into())]))
            .await
            .unwrap();
        store
            .store_user_full_names(&HashMap::from([("U1".into(), "Ada Lovelace".into())]))
            .await
            .unwrap();
        store
            .store_user_avatar_urls(&HashMap::from([(
                "U1".into(),
                "https://avatars.slack-edge.com/u1.png".into(),
            )]))
            .await
            .unwrap();
        store
            .store_user_search_aliases(&HashMap::from([(
                "U1".into(),
                vec!["ada".into(), "lovelace".into()],
            )]))
            .await
            .unwrap();
        store
            .store_custom_emojis(&HashMap::from([(
                "party".into(),
                "https://emoji.slack-edge.com/party.png".into(),
            )]))
            .await
            .unwrap();

        let bootstrap = store.load_bootstrap().await.unwrap().unwrap();
        assert_eq!(bootstrap.workspace_id, "T123:U123");
        assert_eq!(bootstrap.conversations[0].id, "C1");
        assert_eq!(bootstrap.user_names["U1"], "Ada");
        assert_eq!(bootstrap.user_full_names["U1"], "Ada Lovelace");
        assert!(bootstrap.user_avatar_urls["U1"].ends_with("u1.png"));
        assert_eq!(bootstrap.user_search_aliases["U1"][0], "ada");
        assert!(bootstrap.custom_emojis.contains_key("party"));
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_bootstrap_does_not_read_unrelated_timeline_rows() {
    let directory = temp_cache_dir("workspace-bootstrap-focused");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        store
            .store_conversations(&[SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                ..Default::default()
            }])
            .await
            .unwrap();
        let workspace_key = store.workspace_key.clone();
        store
            .hub()
            .await
            .unwrap()
            .write(move |connection| {
                connection.execute(
                    "INSERT OR REPLACE INTO workspace_items(
                        workspace_key, kind, item_key, payload_json
                     ) VALUES (?1, 'channel_history', 'C_BAD', 'not-json')",
                    [workspace_key],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let bootstrap = store.load_bootstrap().await.unwrap().unwrap();
        assert_eq!(bootstrap.conversations[0].id, "C1");
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_round_trips_rich_bot_message_fields() {
    let directory = temp_cache_dir("rich-bot-history");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let mut message = SlackMessage {
        ts: "1710000000.000200".to_string(),
        bot_id: Some("B123".to_string()),
        app_id: Some("A123".to_string()),
        bot_profile: Some(crate::models::SlackBotProfile {
            name: Some("People assistant".to_string()),
            icons: Some(crate::models::SlackIcons {
                image_72: Some("https://cdn.example/bot.png".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        attachments: Some(vec![crate::models::SlackAttachment {
            fallback: Some("Review this request in Slack".to_string()),
            title: Some("Review request".to_string()),
            actions: Some(vec![crate::models::SlackAttachmentAction {
                name: Some("decision".to_string()),
                text: Some("Approve".to_string()),
                kind: Some("button".to_string()),
                value: Some("test-action-value".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }]),
        ..Default::default()
    };
    message.refresh_canonical_content();

    runtime().block_on(async {
        store
            .store_history("C123", std::slice::from_ref(&message))
            .await
            .expect("rich history store failed");
        let restored = store
            .load_history("C123")
            .await
            .expect("rich history load failed")
            .expect("missing rich history");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].author_label(), "People assistant");
        assert_eq!(restored[0].visible_text(), "Review request");
        assert_eq!(restored[0].accessible_text(), "Review request\nApprove");
        assert!(restored[0].blocks.is_none());
        assert!(restored[0].attachments.is_none());
    });

    let connection = Connection::open(store.database_path()).unwrap();
    let payload: String = connection
        .query_row(
            "SELECT payload_json FROM workspace_items
             WHERE workspace_key = ?1 AND kind = 'channel_history' AND item_key = 'C123'",
            [&store.workspace_key],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!payload.contains("test-action-value"));

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn legacy_cached_history_is_upgraded_to_canonical_content() {
    let directory = temp_cache_dir("legacy-rich-history");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        store
            .store_history(
                "C123",
                &[SlackMessage {
                    ts: "1710000000.000300".to_string(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
    });
    let connection = Connection::open(store.database_path()).unwrap();
    let legacy_payload = serde_json::json!([{
        "ts": "1710000000.000300",
        "bot_profile": {"name": "People assistant"},
        "attachments": [{"title": "Review request"}]
    }])
    .to_string();
    connection
        .execute(
            "UPDATE workspace_items SET payload_json = ?1
             WHERE workspace_key = ?2 AND kind = 'channel_history' AND item_key = 'C123'",
            params![legacy_payload, store.workspace_key],
        )
        .unwrap();
    drop(connection);

    runtime().block_on(async {
        let restored = store.load_history("C123").await.unwrap().unwrap();
        assert_eq!(
            restored[0].content_version,
            crate::rich_message::MESSAGE_CONTENT_VERSION
        );
        assert_eq!(restored[0].author_label(), "People assistant");
        assert_eq!(restored[0].visible_text(), "Review request");
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn richer_fresh_message_replaces_legacy_cached_message_with_same_timestamp() {
    let directory = temp_cache_dir("fresh-replaces-legacy");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        store
            .store_history(
                "C123",
                &[SlackMessage {
                    ts: "1710000000.000400".to_string(),
                    text: Some("Legacy fallback".to_string()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        store
            .store_merged_history(
                "C123",
                &[SlackMessage {
                    ts: "1710000000.000400".to_string(),
                    attachments: Some(vec![crate::models::SlackAttachment {
                        title: Some("Fresh request".to_string()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        let restored = store.load_history("C123").await.unwrap().unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].visible_text(), "Fresh request");
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn focused_repository_reads_ignore_malformed_unrelated_domains() {
    let directory = temp_cache_dir("workspace-focused-reads");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        store
            .store_history(
                "C1",
                &[SlackMessage {
                    ts: "1.0".into(),
                    text: Some("history".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        store
            .store_user_names(&HashMap::from([("U1".into(), "Ada".into())]))
            .await
            .unwrap();
    });
    let connection = Connection::open(store.database_path()).unwrap();
    connection
        .execute(
            "INSERT INTO workspace_items(workspace_key, kind, item_key, payload_json)
             VALUES (?1, 'conversation', 'BROKEN', '{broken')",
            [&store.workspace_key],
        )
        .unwrap();
    drop(connection);

    runtime().block_on(async {
        assert_eq!(
            store.load_history("C1").await.unwrap().unwrap()[0].body_text(),
            "history"
        );
        assert_eq!(store.load_user_names().await.unwrap()["U1"], "Ada");
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn sync_freshness_round_trips_success_and_retry_metadata() {
    let directory = temp_cache_dir("workspace-sync-freshness");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        assert_eq!(
            store
                .load_sync_freshness("membership", "workspace")
                .await
                .unwrap(),
            None
        );
        let freshness = SyncFreshness {
            refreshed_at_ms: Some(1_721_500_000_000),
            retry_count: 3,
            retry_after_ms: Some(1_721_500_030_000),
        };
        store
            .store_sync_freshness("membership", "workspace", freshness.clone())
            .await
            .unwrap();
        assert_eq!(
            store
                .load_sync_freshness("membership", "workspace")
                .await
                .unwrap(),
            Some(freshness)
        );
        assert_eq!(
            store.load_sync_freshness("history", "C1").await.unwrap(),
            None
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn store_hub_batches_fifty_maintenance_mutations_in_one_transaction() {
    let directory = temp_cache_dir("store-hub-maintenance-batch");
    runtime().block_on(async {
        let hub = StoreHub::open(directory.clone()).await.unwrap();
        hub.write(|connection| {
            connection.execute(
                "CREATE TABLE maintenance_probe (value INTEGER PRIMARY KEY)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let baseline = hub.metrics();

        let mut writes = tokio::task::JoinSet::new();
        for value in 0..50_i64 {
            let hub = hub.clone();
            writes.spawn(async move {
                hub.write_maintenance(move |transaction| {
                    transaction
                        .execute("INSERT INTO maintenance_probe(value) VALUES (?1)", [value])?;
                    Ok(())
                })
                .await
            });
        }
        while let Some(result) = writes.join_next().await {
            result.unwrap().unwrap();
        }

        let metrics = hub.metrics();
        assert_eq!(metrics.transactions - baseline.transactions, 1);
        assert_eq!(metrics.changed_rows - baseline.changed_rows, 50);
        hub.shutdown().await.unwrap();
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn store_hub_rolls_back_failed_batches_and_suppresses_unchanged_commits() {
    let directory = temp_cache_dir("store-hub-maintenance-rollback");
    runtime().block_on(async {
        let hub = StoreHub::open(directory.clone()).await.unwrap();
        hub.write(|connection| {
            connection.execute(
                "CREATE TABLE maintenance_probe (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        hub.write_maintenance(|transaction| {
            transaction.execute(
                "INSERT INTO maintenance_probe(key, value) VALUES ('stable', 'same')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let after_insert = hub.metrics();
        hub.write_maintenance(|transaction| {
            transaction.execute(
                "INSERT INTO maintenance_probe(key, value) VALUES ('stable', 'same')
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value
                 WHERE maintenance_probe.value IS NOT excluded.value",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        let after_unchanged = hub.metrics();
        assert_eq!(after_unchanged.transactions, after_insert.transactions);
        assert_eq!(after_unchanged.skipped_rows, after_insert.skipped_rows + 1);

        let first = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.write_maintenance(|transaction| {
                    transaction.execute(
                        "INSERT INTO maintenance_probe(key, value) VALUES ('rollback', 'yes')",
                        [],
                    )?;
                    Ok(())
                })
                .await
            })
        };
        let second = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.write_maintenance(|transaction| {
                    transaction.execute("INSERT INTO missing_table(value) VALUES (1)", [])?;
                    Ok(())
                })
                .await
            })
        };
        assert!(first.await.unwrap().is_err());
        assert!(second.await.unwrap().is_err());
        hub.barrier().await.unwrap();
        let exists: bool = hub
            .query(|connection| {
                Ok(connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM maintenance_probe WHERE key = 'rollback')",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert!(!exists);
        hub.shutdown().await.unwrap();
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn focused_repository_suppresses_identical_write_commits() {
    let directory = temp_cache_dir("workspace-identical-write");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        let names = HashMap::from([("U1".to_string(), "Ada".to_string())]);
        store.store_user_names(&names).await.unwrap();
        let after_insert = store.hub().await.unwrap().metrics();
        store.store_user_names(&names).await.unwrap();
        let after_identical = store.hub().await.unwrap().metrics();
        assert_eq!(after_identical.transactions, after_insert.transactions);
        assert!(after_identical.skipped_rows > after_insert.skipped_rows);
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_does_not_persist_ephemeral_huddle_metadata() {
    let directory = temp_cache_dir("workspace-huddle-privacy");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");

    runtime().block_on(async {
        let conversation: SlackConversation = serde_json::from_value(serde_json::json!({
            "id": "C123",
            "name": "general",
            "properties": {
                "huddles": {
                    "id": "R_PRIVATE",
                    "participants": ["U_PRIVATE"]
                },
                "canvas": { "enabled": true }
            }
        }))
        .unwrap();
        assert!(conversation.has_huddle_metadata());

        store
            .store_conversations(std::slice::from_ref(&conversation))
            .await
            .expect("conversation snapshot store failed");
        store
            .store_conversation(&conversation)
            .await
            .expect("conversation row store failed");

        let cached = store
            .load_conversations()
            .await
            .expect("conversation load failed")
            .expect("missing cached conversation");
        assert!(!cached[0].has_huddle_metadata());
        assert_eq!(
            cached[0]
                .extra
                .get("properties")
                .and_then(|value| value.get("canvas"))
                .and_then(|value| value.get("enabled"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );

        let connection = Connection::open(store.database_path()).unwrap();
        let payload: String = connection
            .query_row(
                "SELECT payload_json FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C123'",
                [&store.workspace_key],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!payload.contains("R_PRIVATE"));
        assert!(!payload.contains("U_PRIVATE"));
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn ensuring_workspace_identity_upgrades_an_existing_cache() {
    let directory = temp_cache_dir("workspace-store-identity-upgrade");
    std::fs::create_dir_all(&directory).unwrap();
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    std::fs::write(
        store.path(),
        r#"{"version":1,"conversations":[{"id":"D1","is_im":true}]}"#,
    )
    .unwrap();

    runtime()
        .block_on(store.ensure_workspace_identity())
        .expect("workspace identity upgrade failed");

    let state = runtime()
        .block_on(store.load_bootstrap())
        .unwrap()
        .expect("missing upgraded bootstrap");
    assert_eq!(state.workspace_id, "T123:U123");
    assert_eq!(state.conversations[0].id, "D1");
    assert!(!store.path().exists());
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_exposes_a_lightweight_active_search_snapshot() {
    let directory = temp_cache_dir("workspace-search-index");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        store
            .store_conversations(&[SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                is_channel: Some(true),
                ..Default::default()
            }])
            .await
            .unwrap();
        store
            .store_history(
                "C1",
                &[SlackMessage {
                    ts: "1.0".into(),
                    text: Some("private message body".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        store.ensure_workspace_identity().await.unwrap();
    });

    let search_state = load_active_search_state(&directory).unwrap().unwrap();
    assert_eq!(search_state.workspace_id, "T123:U123");
    assert_eq!(search_state.conversations[0].id, "C1");
    assert!(store.database_path().exists());

    let connection = Connection::open(store.database_path()).unwrap();
    let stored_private_body: bool = connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM workspace_items
                WHERE workspace_key = ?1 AND kind = 'channel_history'
                  AND payload_json LIKE '%private message body%'
            )",
            [&store.workspace_key],
            |row| row.get(0),
        )
        .unwrap();
    assert!(stored_private_body);

    connection
        .execute(
            "UPDATE workspace_items SET payload_json = 'not valid JSON'
             WHERE workspace_key = ?1 AND kind = 'channel_history'",
            [&store.workspace_key],
        )
        .unwrap();
    let search_state = load_active_search_state(&directory).unwrap().unwrap();
    assert_eq!(search_state.conversations[0].id, "C1");
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn clearing_the_active_workspace_preserves_its_cached_state() {
    let directory = temp_cache_dir("workspace-clear-active");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    runtime().block_on(async {
        store
            .store_conversations(&[SlackConversation {
                id: "C1".into(),
                name: Some("general".into()),
                ..Default::default()
            }])
            .await
            .unwrap();
        store.ensure_workspace_identity().await.unwrap();
    });

    clear_active_workspace(&directory).unwrap();
    runtime().block_on(async {
        store
            .store_history(
                "C1",
                &[SlackMessage {
                    ts: "1.0".into(),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
    });

    assert!(load_active_search_state(&directory).unwrap().is_none());
    let conversations = runtime()
        .block_on(store.load_conversations())
        .unwrap()
        .unwrap();
    assert_eq!(conversations[0].id, "C1");
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_updates_one_conversation_without_replacing_others() {
    let directory = temp_cache_dir("workspace-store-conversation-update");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        store
            .store_conversations(&[
                SlackConversation {
                    id: "C1".to_string(),
                    name: Some("general".to_string()),
                    ..Default::default()
                },
                SlackConversation {
                    id: "C2".to_string(),
                    name: Some("random".to_string()),
                    ..Default::default()
                },
            ])
            .await
            .expect("conversation store failed");

        store
            .store_conversation(&SlackConversation {
                id: "C1".to_string(),
                name: Some("renamed".to_string()),
                ..Default::default()
            })
            .await
            .expect("conversation update failed");
        store
            .store_conversation(&SlackConversation {
                id: "C3".to_string(),
                name: Some("new".to_string()),
                ..Default::default()
            })
            .await
            .expect("conversation insert failed");

        let conversations = store
            .load_conversations()
            .await
            .expect("conversation load failed")
            .expect("missing cached conversations");
        assert_eq!(conversations.len(), 3);
        assert_eq!(
            conversations
                .iter()
                .find(|conversation| conversation.id == "C1")
                .and_then(|conversation| conversation.name.as_deref()),
            Some("renamed")
        );
        assert!(conversations
            .iter()
            .any(|conversation| conversation.id == "C2"));
        assert!(conversations
            .iter()
            .any(|conversation| conversation.id == "C3"));
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_persists_sparse_conversation_star_updates() {
    let directory = temp_cache_dir("workspace-store-conversation-star-update");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");

    runtime().block_on(async {
        store
            .store_conversation(&SlackConversation {
                id: "C1".to_string(),
                name: Some("general".to_string()),
                is_channel: Some(true),
                is_starred: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .store_conversation(&SlackConversation {
                id: "C1".to_string(),
                is_starred: Some(false),
                ..Default::default()
            })
            .await
            .unwrap();

        let conversations = store
            .load_conversations()
            .await
            .unwrap()
            .expect("missing cached conversations");
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].name.as_deref(), Some("general"));
        assert_eq!(conversations[0].is_starred, Some(false));
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn conversation_row_mutations_ignore_unrelated_corrupt_rows() {
    let directory = temp_cache_dir("workspace-store-conversation-row-update");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        store
            .store_conversations(&[
                SlackConversation {
                    id: "C1".to_string(),
                    name: Some("old".to_string()),
                    is_private: Some(false),
                    ..Default::default()
                },
                SlackConversation {
                    id: "C2".to_string(),
                    name: Some("unrelated".to_string()),
                    ..Default::default()
                },
            ])
            .await
            .expect("conversation store failed");

        let connection = Connection::open(store.database_path()).unwrap();
        connection
            .execute(
                "UPDATE workspace_items SET payload_json = '{broken'
                 WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C2'",
                [&store.workspace_key],
            )
            .unwrap();
        drop(connection);

        store
            .merge_conversation(&SlackConversation {
                id: "C1".to_string(),
                name: Some("renamed".to_string()),
                ..Default::default()
            })
            .await
            .expect("metadata update read an unrelated row");
        store
            .merge_conversation(&SlackConversation {
                id: "C1".to_string(),
                is_archived: Some(true),
                ..Default::default()
            })
            .await
            .expect("sparse update read an unrelated row");
    });

    let connection = Connection::open(store.database_path()).unwrap();
    let updated_payload: String = connection
        .query_row(
            "SELECT payload_json FROM workspace_items
             WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C1'",
            [&store.workspace_key],
            |row| row.get(0),
        )
        .unwrap();
    let updated: SlackConversation = serde_json::from_str(&updated_payload).unwrap();
    assert_eq!(updated.name.as_deref(), Some("renamed"));
    assert_eq!(updated.is_private, Some(false));
    assert_eq!(updated.is_archived, Some(true));
    let unrelated_payload: String = connection
        .query_row(
            "SELECT payload_json FROM workspace_items
             WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C2'",
            [&store.workspace_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unrelated_payload, "{broken");

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn corrupt_row_recovery_deletes_only_corrupt_row_preserving_workspace_and_other_items() {
    let directory = temp_cache_dir("workspace-store-corrupt-row-recovery");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        store
            .store_history(
                "C1",
                &[SlackMessage {
                    ts: "100.0".to_string(),
                    text: Some("hello".to_string()),
                    ..Default::default()
                }],
            )
            .await
            .expect("store history C1 failed");

        store
            .store_history(
                "C2",
                &[SlackMessage {
                    ts: "200.0".to_string(),
                    text: Some("world".to_string()),
                    ..Default::default()
                }],
            )
            .await
            .expect("store history C2 failed");

        // Corrupt C2 row in workspace_items
        let connection = Connection::open(store.database_path()).unwrap();
        connection
            .execute(
                "UPDATE workspace_items SET payload_json = '{invalid_json'
                 WHERE workspace_key = ?1 AND kind = 'channel_history' AND item_key = 'C2'",
                [&store.workspace_key],
            )
            .unwrap();
        drop(connection);

        // Load C2: this should recover from the corrupt row by deleting only C2
        let c2_history = store.load_history("C2").await.unwrap();
        assert!(c2_history.is_none(), "C2 history should be discarded");

        // C1 history must still exist! (Under old code, C1 was wiped by reset_sqlite_workspace)
        let c1_history = store.load_history("C1").await.unwrap();
        assert!(
            c1_history.is_some(),
            "C1 history must still exist and not be wiped by C2's corruption"
        );
        assert_eq!(c1_history.unwrap()[0].text.as_deref(), Some("hello"));

        // Verify the C2 row was deleted from sqlite
        let connection = Connection::open(store.database_path()).unwrap();
        let c2_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM workspace_items
                 WHERE workspace_key = ?1 AND kind = 'channel_history' AND item_key = 'C2'",
                [&store.workspace_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(c2_count, 0, "corrupt C2 row should be deleted");

        // Verify the workspace itself was not reset (still exists)
        let ws_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM workspaces WHERE workspace_key = ?1",
                [&store.workspace_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ws_count, 1, "workspace must not be deleted");
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn corrupt_conversation_row_recovery_preserves_other_conversations() {
    let directory = temp_cache_dir("workspace-store-corrupt-conv-recovery");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        store
            .store_conversations(&[
                SlackConversation {
                    id: "C1".to_string(),
                    name: Some("general".to_string()),
                    ..Default::default()
                },
                SlackConversation {
                    id: "C2".to_string(),
                    name: Some("random".to_string()),
                    ..Default::default()
                },
            ])
            .await
            .expect("store conversations failed");

        // Corrupt C2 row in workspace_items
        let connection = Connection::open(store.database_path()).unwrap();
        connection
            .execute(
                "UPDATE workspace_items SET payload_json = '{invalid_json'
                 WHERE workspace_key = ?1 AND kind = 'conversation' AND item_key = 'C2'",
                [&store.workspace_key],
            )
            .unwrap();
        drop(connection);

        // First load: C2 error triggers deleting C2
        let conversations = store.load_conversations().await.unwrap();
        assert!(conversations.is_none());

        // Second load: C2 was deleted, so C1 is still present and loads cleanly
        let conversations = store.load_conversations().await.unwrap();
        assert!(conversations.is_some(), "C1 must still be cached");
        let list = conversations.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "C1");

        // Verify the workspace itself was not wiped
        let connection = Connection::open(store.database_path()).unwrap();
        let ws_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM workspaces WHERE workspace_key = ?1",
                [&store.workspace_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ws_count, 1, "workspace must not be deleted");
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn conversation_row_mutations_do_not_follow_mismatched_payload_ids() {
    let directory = temp_cache_dir("workspace-store-conversation-row-id-mismatch");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        store
            .store_conversations(&[
                SlackConversation {
                    id: "C0".to_string(),
                    name: Some("untouched".to_string()),
                    is_private: Some(true),
                    ..Default::default()
                },
                SlackConversation {
                    id: "C1".to_string(),
                    name: Some("original".to_string()),
                    ..Default::default()
                },
            ])
            .await
            .expect("conversation store failed");

        let mismatched = serde_json::to_string(&SlackConversation {
            id: "C0".to_string(),
            name: Some("mismatched".to_string()),
            is_private: Some(false),
            ..Default::default()
        })
        .unwrap();
        let replace_c1_payload = |payload: &str| {
            let connection = Connection::open(store.database_path()).unwrap();
            connection
                .execute(
                    "UPDATE workspace_items SET payload_json = ?1
                     WHERE workspace_key = ?2 AND kind = 'conversation' AND item_key = 'C1'",
                    params![payload, &store.workspace_key],
                )
                .unwrap();
        };
        replace_c1_payload(&mismatched);

        store
            .store_conversation(&SlackConversation {
                id: "C1".to_string(),
                name: Some("metadata repaired".to_string()),
                ..Default::default()
            })
            .await
            .expect("metadata repair failed");
        let repaired = store.load_conversations().await.unwrap().unwrap();
        assert_eq!(
            repaired
                .iter()
                .find(|conversation| conversation.id == "C0")
                .and_then(|conversation| conversation.name.as_deref()),
            Some("untouched")
        );
        assert_eq!(
            repaired
                .iter()
                .find(|conversation| conversation.id == "C0")
                .and_then(|conversation| conversation.is_private),
            Some(true)
        );
        assert_eq!(
            repaired
                .iter()
                .find(|conversation| conversation.id == "C1")
                .and_then(|conversation| conversation.name.as_deref()),
            Some("metadata repaired")
        );

        replace_c1_payload(&mismatched);
        store
            .merge_conversation(&SlackConversation {
                id: "C1".to_string(),
                is_archived: Some(true),
                ..Default::default()
            })
            .await
            .expect("sparse repair failed");
        let repaired = store.load_conversations().await.unwrap().unwrap();
        assert_eq!(
            repaired
                .iter()
                .find(|conversation| conversation.id == "C0")
                .and_then(|conversation| conversation.name.as_deref()),
            Some("untouched")
        );
        assert_eq!(
            repaired
                .iter()
                .find(|conversation| conversation.id == "C0")
                .and_then(|conversation| conversation.is_private),
            Some(true)
        );
        assert_eq!(
            repaired
                .iter()
                .find(|conversation| conversation.id == "C1")
                .and_then(|conversation| conversation.is_archived),
            Some(true)
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

async fn claimed(store: &WorkspaceStore, channel_id: &str, message_ts: &str) -> bool {
    store
        .accept_attention_delivery(channel_id, message_ts, true)
        .await
        .unwrap()
        .notification_claimed
}

#[test]
fn attention_delivery_claim_is_atomic_and_survives_reopen() {
    let directory = temp_cache_dir("workspace-store-attention-delivery");
    let runtime = runtime();

    runtime.block_on(async {
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        assert!(claimed(&store, "D1", "1710000001.000001").await);
        assert!(!claimed(&store, "D1", "1710000001.000001").await);
        assert!(claimed(&store, "D1", "1710000002.000001").await);

        drop(store);
        let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
        assert!(!claimed(&reopened, "D1", "1710000001.000001").await);
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn realtime_attention_delivery_claims_each_message_once_across_restarts() {
    let directory = temp_cache_dir("workspace-store-attention-acceptance");
    let runtime = runtime();

    runtime.block_on(async {
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        assert_eq!(
            store
                .accept_attention_delivery("", "1710000001.000001", true)
                .await
                .unwrap(),
            AttentionDeliveryOutcome {
                observation: AttentionObservationStatus::InvalidIdentity,
                notification_claimed: false,
            }
        );
        assert_eq!(
            store
                .accept_attention_delivery("D1", "1710000001.000001", true)
                .await
                .unwrap(),
            AttentionDeliveryOutcome {
                observation: AttentionObservationStatus::Accepted,
                notification_claimed: true,
            }
        );
        assert_eq!(
            store
                .accept_attention_delivery("D1", "1710000001.000001", true)
                .await
                .unwrap(),
            AttentionDeliveryOutcome {
                observation: AttentionObservationStatus::AlreadyObserved,
                notification_claimed: false,
            }
        );
        // A delivery that does not want a notification must not consume the
        // identity, so a later notifying delivery can still claim it.
        assert_eq!(
            store
                .accept_attention_delivery("D1", "1710000002.000001", false)
                .await
                .unwrap(),
            AttentionDeliveryOutcome {
                observation: AttentionObservationStatus::Accepted,
                notification_claimed: false,
            }
        );
        assert_eq!(
            store
                .accept_attention_delivery("D1", "1710000002.000001", true)
                .await
                .unwrap(),
            AttentionDeliveryOutcome {
                observation: AttentionObservationStatus::Accepted,
                notification_claimed: true,
            }
        );

        drop(store);
        let reopened = WorkspaceStore::new(directory.clone(), "T123:U123");
        assert_eq!(
            reopened
                .accept_attention_delivery("D1", "1710000001.000001", true)
                .await
                .unwrap(),
            AttentionDeliveryOutcome {
                observation: AttentionObservationStatus::AlreadyObserved,
                notification_claimed: false,
            }
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_serializes_individual_conversation_updates_across_clones() {
    let directory = temp_cache_dir("workspace-store-conversation-concurrent");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let cloned_store = store.clone();
    let runtime = runtime();

    runtime.block_on(async {
        store
            .store_conversations(&[SlackConversation {
                id: "C1".to_string(),
                ..Default::default()
            }])
            .await
            .expect("conversation store failed");

        let (update_result, insert_result) = futures_util::future::join(
            store.update_conversation("C1", |conversation| {
                conversation.is_starred = Some(true);
            }),
            cloned_store.store_conversation(&SlackConversation {
                id: "C2".to_string(),
                ..Default::default()
            }),
        )
        .await;
        assert!(update_result.expect("conversation update failed"));
        insert_result.expect("conversation insert failed");

        let conversations = store
            .load_conversations()
            .await
            .expect("conversation load failed")
            .expect("missing cached conversations");
        assert_eq!(conversations.len(), 2);
        assert_eq!(
            conversations
                .iter()
                .find(|conversation| conversation.id == "C1")
                .and_then(|conversation| conversation.is_starred),
            Some(true)
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_removes_one_conversation() {
    let directory = temp_cache_dir("workspace-store-conversation-remove");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        store
            .store_conversations(&[
                SlackConversation {
                    id: "C1".to_string(),
                    ..Default::default()
                },
                SlackConversation {
                    id: "C2".to_string(),
                    ..Default::default()
                },
            ])
            .await
            .expect("conversation store failed");

        assert!(store
            .remove_conversation("C1")
            .await
            .expect("conversation removal failed"));
        assert!(!store
            .remove_conversation("C1")
            .await
            .expect("duplicate conversation removal failed"));
        let conversations = store
            .load_conversations()
            .await
            .expect("conversation load failed")
            .expect("missing cached conversations");
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].id, "C2");
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_round_trips_user_names() {
    let directory = temp_cache_dir("workspace-store-user-names");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        assert!(store
            .load_user_names()
            .await
            .expect("user name load failed")
            .is_empty());

        store
            .store_user_name("U123", "Ada Lovelace")
            .await
            .expect("user name store failed");

        assert_eq!(
            store
                .load_user_names()
                .await
                .expect("user name load failed")
                .get("U123")
                .map(String::as_str),
            Some("Ada Lovelace")
        );

        store
            .store_user_full_names(&HashMap::from([(
                "U123".to_string(),
                "Augusta Ada King".to_string(),
            )]))
            .await
            .expect("user full name store failed");
        assert_eq!(
            store
                .load_user_full_names()
                .await
                .expect("user full name load failed")
                .get("U123")
                .map(String::as_str),
            Some("Augusta Ada King")
        );

        let avatar_urls = HashMap::from([(
            "U123".to_string(),
            "https://avatars.slack-edge.com/ada.png".to_string(),
        )]);
        store
            .store_user_avatar_urls(&avatar_urls)
            .await
            .expect("user avatar URL store failed");
        assert_eq!(
            store
                .load_user_avatar_urls()
                .await
                .expect("user avatar URL load failed"),
            avatar_urls
        );

        let aliases = HashMap::from([(
            "U123".to_string(),
            vec!["Ada".to_string(), "Ada Lovelace".to_string()],
        )]);
        store
            .store_user_search_aliases(&aliases)
            .await
            .expect("user search alias store failed");
        assert_eq!(
            store
                .load_user_search_aliases()
                .await
                .expect("user search alias load failed"),
            aliases
        );

        let status = SlackUserStatus {
            text: "In a meeting".to_string(),
            emoji: ":calendar:".to_string(),
            expiration: 200,
        };
        store
            .store_user_status("U123", Some(status.clone()))
            .await
            .expect("user status store failed");
        assert_eq!(
            store
                .load_user_statuses()
                .await
                .expect("user status load failed")
                .get("U123"),
            Some(&status)
        );
        store
            .store_user_status("U123", None)
            .await
            .expect("user status removal failed");
        assert!(store
            .load_user_statuses()
            .await
            .expect("user status load failed")
            .is_empty());
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_serializes_concurrent_updates_from_clones() {
    let directory = temp_cache_dir("workspace-store-concurrent-updates");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let cloned_store = store.clone();
    let runtime = runtime();

    runtime.block_on(async {
        let conversations = vec![SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            ..Default::default()
        }];
        let messages = vec![SlackMessage {
            ts: "1710000000.000100".to_string(),
            text: Some("cached".to_string()),
            ..Default::default()
        }];

        let (conversations_result, history_result) = futures_util::future::join(
            store.store_conversations(&conversations),
            cloned_store.store_history("C123", &messages),
        )
        .await;
        conversations_result.expect("conversation store failed");
        history_result.expect("history store failed");

        assert_eq!(
            store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("concurrent conversation update was lost")[0]
                .id,
            "C123"
        );
        assert_eq!(
            store
                .load_history("C123")
                .await
                .expect("history load failed")
                .expect("concurrent history update was lost")[0]
                .body_text(),
            "cached"
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_cache_key_does_not_expose_workspace_identity() {
    let key = cache_key("T123:U123");

    assert_eq!(key.len(), 64);
    assert!(!key.contains("T123"));
    assert!(!key.contains("U123"));
}

#[test]
fn workspace_store_replaces_invalid_cache_on_next_write() {
    let directory = temp_cache_dir("workspace-store-invalid");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    std::fs::create_dir_all(&directory).expect("failed to create cache dir");
    std::fs::write(store.path(), "not json").expect("failed to write invalid cache");

    runtime.block_on(async {
        store
            .store_conversations(&[SlackConversation {
                id: "C123".to_string(),
                ..Default::default()
            }])
            .await
            .expect("conversation store failed");

        assert_eq!(
            store
                .load_conversations()
                .await
                .expect("conversation load failed")
                .expect("missing cached conversations")[0]
                .id,
            "C123"
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_merges_paged_history_newest_first() {
    let directory = temp_cache_dir("workspace-store-merged-history");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        store
            .store_history(
                "C123",
                &[
                    SlackMessage {
                        ts: "1710000300.000000".to_string(),
                        text: Some("new".to_string()),
                        ..Default::default()
                    },
                    SlackMessage {
                        ts: "1710000200.000000".to_string(),
                        text: Some("middle".to_string()),
                        ..Default::default()
                    },
                ],
            )
            .await
            .expect("history store failed");

        store
            .store_merged_history(
                "C123",
                &[
                    SlackMessage {
                        ts: "1710000200.000000".to_string(),
                        text: Some("duplicate".to_string()),
                        ..Default::default()
                    },
                    SlackMessage {
                        ts: "1710000100.000000".to_string(),
                        text: Some("old".to_string()),
                        ..Default::default()
                    },
                ],
            )
            .await
            .expect("merged history store failed");

        let messages = store
            .load_history("C123")
            .await
            .expect("history load failed")
            .expect("missing cached history");
        let timestamps = messages
            .iter()
            .map(|message| message.ts.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            timestamps,
            vec![
                "1710000300.000000",
                "1710000200.000000",
                "1710000100.000000"
            ]
        );
        assert_eq!(
            messages
                .iter()
                .find(|message| message.ts == "1710000200.000000")
                .and_then(|message| message.text.as_deref()),
            Some("duplicate")
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn stale_history_page_does_not_remove_newer_realtime_message() {
    let directory = temp_cache_dir("workspace-store-realtime-history-race");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        store
            .store_merged_history(
                "D1",
                &[SlackMessage {
                    ts: "5.0".to_string(),
                    text: Some("realtime".to_string()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        store
            .store_history(
                "D1",
                &[SlackMessage {
                    ts: "4.0".to_string(),
                    text: Some("stale page".to_string()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        let messages = store.load_history("D1").await.unwrap().unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["5.0", "4.0"]
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn channel_history_filters_thread_replies_but_keeps_broadcasts() {
    let directory = temp_cache_dir("workspace-store-thread-routing");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        let root = SlackMessage {
            ts: "1.0".into(),
            thread_ts: Some("1.0".into()),
            ..Default::default()
        };
        let reply = SlackMessage {
            ts: "2.0".into(),
            thread_ts: Some("1.0".into()),
            ..Default::default()
        };
        let mut broadcast = reply.clone();
        broadcast.ts = "3.0".into();
        broadcast.subtype = Some("thread_broadcast".into());

        store
            .store_merged_history("C1", &[root.clone(), reply.clone(), broadcast.clone()])
            .await
            .unwrap();
        assert_eq!(
            store
                .load_history("C1")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["3.0", "1.0"]
        );

        // Loading also sanitizes caches written by older Conduit versions.
        store
            .store_kind_map(
                "channel_history",
                HashMap::from([("C2".into(), vec![root, reply, broadcast])]),
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .load_history("C2")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["3.0", "1.0"]
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn stale_thread_snapshot_keeps_newer_realtime_reply() {
    let directory = temp_cache_dir("workspace-store-realtime-thread-race");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        store
            .store_merged_thread(
                "C1",
                "1.0",
                &[SlackMessage {
                    ts: "2.0".into(),
                    thread_ts: Some("1.0".into()),
                    text: Some("realtime reply".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();
        store
            .store_thread(
                "C1",
                "1.0",
                &[SlackMessage {
                    ts: "1.0".into(),
                    text: Some("stale parent".into()),
                    ..Default::default()
                }],
            )
            .await
            .unwrap();

        assert_eq!(
            store
                .load_thread("C1", "1.0")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["2.0", "1.0"]
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_prunes_cached_history_to_recent_bound() {
    let directory = temp_cache_dir("workspace-store-pruned-history");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        let messages = (0..=MAX_CACHED_CHANNEL_MESSAGES)
            .map(|index| SlackMessage {
                ts: format!("1710000{:03}.000000", MAX_CACHED_CHANNEL_MESSAGES - index),
                text: Some(format!("message {index}")),
                ..Default::default()
            })
            .collect::<Vec<_>>();

        store
            .store_history("C123", &messages)
            .await
            .expect("history store failed");

        let cached = store
            .load_history("C123")
            .await
            .expect("history load failed")
            .expect("missing cached history");

        assert_eq!(cached.len(), MAX_CACHED_CHANNEL_MESSAGES);
        assert_eq!(cached[0].ts, "1710000200.000000");
        assert_eq!(
            cached.last().map(|message| message.ts.as_str()),
            Some("1710000001.000000")
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_round_trips_thread_catalog() {
    use crate::thread_catalog::ThreadCatalog;

    let directory = temp_cache_dir("workspace-store-thread-catalog");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        let mut catalog = ThreadCatalog::default();
        let root = SlackMessage {
            ts: "1710000000.000100".into(),
            reply_count: Some(3),
            subscribed: Some(true),
            latest_reply: Some("1710000300.000100".into()),
            ..Default::default()
        };
        catalog.observe_thread("C123", &root.ts.clone(), &[root], false);
        let records = catalog.into_records();
        store
            .store_thread_catalog(&records)
            .await
            .expect("thread catalog store failed");

        assert_eq!(
            store
                .load_thread_catalog()
                .await
                .expect("thread catalog load failed"),
            records
        );
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn history_and_thread_deltas_merge_successively_without_full_replaces() {
    let directory = temp_cache_dir("workspace-store-history-thread-deltas");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        let revision = WorkspaceRevision::INITIAL.successor();
        // Page 1: History delta with message 2.0
        store
            .execute_store_batch(
                StoreBatch::new(
                    revision,
                    vec![StoreChange::HistoryDelta {
                        channel_id: "C123".into(),
                        messages: vec![SlackMessage {
                            ts: "2.0".into(),
                            text: Some("page 1 message".into()),
                            ..Default::default()
                        }],
                    }],
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let history = store.load_history("C123").await.unwrap().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].ts, "2.0");

        // Page 2: Older message 1.0 merged in
        let revision2 = revision.successor();
        store
            .execute_store_batch(
                StoreBatch::new(
                    revision2,
                    vec![StoreChange::HistoryDelta {
                        channel_id: "C123".into(),
                        messages: vec![SlackMessage {
                            ts: "1.0".into(),
                            text: Some("page 2 message".into()),
                            ..Default::default()
                        }],
                    }],
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let history = store.load_history("C123").await.unwrap().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].ts, "2.0");
        assert_eq!(history[1].ts, "1.0");

        // Thread delta for replies
        let revision3 = revision2.successor();
        store
            .execute_store_batch(
                StoreBatch::new(
                    revision3,
                    vec![StoreChange::ThreadDelta {
                        channel_id: "C123".into(),
                        thread_ts: "1.0".into(),
                        messages: vec![SlackMessage {
                            ts: "1.1".into(),
                            thread_ts: Some("1.0".into()),
                            text: Some("reply 1".into()),
                            ..Default::default()
                        }],
                    }],
                )
                .unwrap(),
            )
            .await
            .unwrap();

        let thread = store.load_thread("C123", "1.0").await.unwrap().unwrap();
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].ts, "1.1");
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_store_round_trips_read_state_watermarks() {
    use crate::thread_catalog::ThreadCatalog;

    let directory = temp_cache_dir("workspace-store-read-watermarks");
    let store = WorkspaceStore::new(directory.clone(), "T123:U123");
    let runtime = runtime();

    runtime.block_on(async {
        let mut conversation = SlackConversation {
            id: "C123".into(),
            name: Some("general".into()),
            ..Default::default()
        };
        conversation
            .extra
            .insert("last_read".into(), serde_json::json!("1710000200.000100"));
        conversation
            .extra
            .insert("unread_mentions".into(), serde_json::json!(["1.0"]));
        store
            .store_conversations(&[conversation.clone()])
            .await
            .expect("store conversations failed");

        let loaded_convs = store
            .load_conversations()
            .await
            .expect("load conversations failed")
            .expect("conversations found");
        assert_eq!(loaded_convs.len(), 1);
        assert!(!loaded_convs[0].extra.contains_key("last_read"));
        assert!(!loaded_convs[0].extra.contains_key("unread_mentions"));

        let read_state = ConversationReadState {
            last_read: Some("1710000200.000100".into()),
            server_latest: Some("1710000300.000100".into()),
            server_mention_count: 4,
            server_unread_count: 1,
            server_has_unreads: true,
            live_mentions: BTreeSet::from(["1710000400.000100".to_string()]),
            live_latest_unread: Some("1710000500.000100".into()),
        };
        store
            .execute_store_batch(
                StoreBatch::new(
                    WorkspaceRevision::INITIAL.successor(),
                    vec![StoreChange::ReadStatesUpserted(vec![(
                        "C123".into(),
                        read_state.clone(),
                    )])],
                )
                .unwrap(),
            )
            .await
            .expect("store read state failed");
        let bootstrap = store
            .load_bootstrap()
            .await
            .unwrap()
            .expect("bootstrap exists");
        assert_eq!(bootstrap.read_states.get("C123"), Some(&read_state));

        let mut catalog = ThreadCatalog::default();
        let root = SlackMessage {
            ts: "1710000000.000100".into(),
            reply_count: Some(2),
            subscribed: Some(true),
            latest_reply: Some("1710000300.000100".into()),
            ..Default::default()
        };
        catalog.observe_thread("C123", &root.ts.clone(), &[root], false);
        let mut records = catalog.into_records();
        records[0].last_read = Some("1710000250.000100".into());

        store
            .store_thread_catalog(&records)
            .await
            .expect("store thread catalog failed");

        let loaded_records = store
            .load_thread_catalog()
            .await
            .expect("load thread catalog failed");
        assert_eq!(loaded_records.len(), 1);
        assert_eq!(
            loaded_records[0].last_read.as_deref(),
            Some("1710000250.000100")
        );
        assert!(loaded_records[0].has_unread_replies());
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn store_and_load_sidebar_theme() {
    let directory = temp_cache_dir("workspace-store-sidebar-theme");
    let store = WorkspaceStore::new(directory.clone(), "T123");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let theme = SidebarTheme {
            column_bg: "#123456".into(),
            menu_bg_hover: "#234567".into(),
            active_item: "#345678".into(),
            active_item_text: "#ffffff".into(),
            hover_item: "#456789".into(),
            text_color: "#dddddd".into(),
            active_presence: "#00ff00".into(),
            mention_badge: Some("#ff0000".into()),
            top_nav_bg: Some("#112233".into()),
            top_nav_text: Some("#eeeeee".into()),
        };

        store.store_sidebar_theme(&theme).await.unwrap();

        let bootstrap = store
            .load_bootstrap()
            .await
            .unwrap()
            .expect("bootstrap exists");
        assert_eq!(bootstrap.sidebar_theme, Some(theme));
    });

    let _ = std::fs::remove_dir_all(directory);
}
