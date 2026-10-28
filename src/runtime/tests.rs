use std::cell::Cell;
use std::future;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::*;
use crate::slack::SlackMessageActionRequest;
use crate::workspace_pipeline::{ConversationRefresh, MessageChange, StoreChange, WorkspaceChange};

fn admission_request(session: u64, request: u64, command: RuntimeCommand) -> RuntimeRequest {
    RuntimeRequest::new(
        RuntimeIdentity {
            session: SessionId(session),
            request: RequestId::new(request),
        },
        command,
    )
}

fn admission_test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build admission test runtime")
}

fn mailbox_event(request: u64, context: OperationContext, kind: RuntimeEventKind) -> RuntimeEvent {
    RuntimeEvent {
        meta: RuntimeEventMeta {
            session: SessionId(1),
            request: Some(RequestId::new(request)),
            context,
        },
        kind,
    }
}

fn mailbox_status_event(request: u64, status: &str) -> RuntimeEvent {
    mailbox_event(
        request,
        OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace),
        RuntimeEventKind::Status(status.to_string()),
    )
}

fn mailbox_status(event: RuntimeEvent) -> String {
    let RuntimeEventKind::Status(status) = event.kind else {
        panic!("expected status event");
    };
    status
}

async fn wait_for_mailbox_block(sender: &RuntimeEventMailboxSender, expected: u64) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while sender.snapshot().blocked < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime event producer did not block");
}

#[test]
fn runtime_event_mailbox_bounds_capacity_and_preserves_fifo_after_backpressure() {
    admission_test_runtime().block_on(async {
        let (sender, mut receiver) = runtime_event_channel_with_capacity(2, 1);
        sender.send(mailbox_status_event(1, "first")).unwrap();
        sender.send(mailbox_status_event(2, "second")).unwrap();

        let blocked_sender = sender.clone();
        let blocked = tokio::spawn(async move {
            blocked_sender
                .send(mailbox_status_event(3, "third"))
                .unwrap();
        });
        wait_for_mailbox_block(&sender, 1).await;
        assert_eq!(
            receiver.snapshot(),
            RuntimeEventMailboxSnapshot {
                admitted: 2,
                blocked: 1,
                depth: 2,
                peak_depth: 2,
                ..RuntimeEventMailboxSnapshot::default()
            }
        );

        assert_eq!(mailbox_status(receiver.recv().await.unwrap()), "first");
        tokio::time::timeout(Duration::from_secs(1), blocked)
            .await
            .expect("blocked producer did not resume")
            .unwrap();
        assert_eq!(mailbox_status(receiver.recv().await.unwrap()), "second");
        assert_eq!(mailbox_status(receiver.recv().await.unwrap()), "third");
        let snapshot = receiver.snapshot();
        assert_eq!(snapshot.admitted, 3);
        assert_eq!(snapshot.dequeued, 3);
        assert_eq!(snapshot.depth, 0);
        assert_eq!(snapshot.peak_depth, 2);
        assert_eq!(RUNTIME_EVENT_QUEUE_CAPACITY, 256);
        assert_eq!(RUNTIME_EVENT_PROGRESS_CAPACITY, 32);
    });
}

#[test]
fn runtime_event_mailbox_tickets_prevent_reliable_and_progress_overtaking() {
    admission_test_runtime().block_on(async {
        let (sender, mut receiver) = runtime_event_channel_with_capacity(1, 1);
        sender.send(mailbox_status_event(1, "head")).unwrap();

        let first_sender = sender.clone();
        let first = tokio::spawn(async move {
            first_sender
                .send(mailbox_status_event(2, "first blocked"))
                .unwrap();
        });
        wait_for_mailbox_block(&sender, 1).await;
        let second_sender = sender.clone();
        let second = tokio::spawn(async move {
            second_sender
                .send(mailbox_status_event(3, "second blocked"))
                .unwrap();
        });
        wait_for_mailbox_block(&sender, 2).await;

        // Free one slot without waking producers. This creates the exact
        // scheduling window where a later producer could otherwise barge.
        let head = {
            let mut state = receiver
                .inner
                .state
                .lock()
                .expect("runtime event mailbox lock poisoned");
            let head = state.queue.pop_front().unwrap();
            state.metrics.dequeued = state.metrics.dequeued.saturating_add(1);
            head
        };
        assert_eq!(mailbox_status(head), "head");
        sender
            .send(mailbox_event(
                4,
                OperationContext::new(
                    RuntimeOperation::AttachmentDownload,
                    RuntimeTarget::Attachment("download".to_string()),
                ),
                RuntimeEventKind::AttachmentDownloadProgress {
                    fraction: 0.5,
                    label: "must not steal".to_string(),
                },
            ))
            .unwrap();
        assert_eq!(sender.snapshot().depth, 0);
        assert_eq!(sender.snapshot().dropped_progress, 1);

        receiver.inner.space.notify_all();
        assert_eq!(
            mailbox_status(
                tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                    .await
                    .expect("first reliable producer stayed blocked")
                    .unwrap()
            ),
            "first blocked"
        );
        assert_eq!(
            mailbox_status(
                tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                    .await
                    .expect("second reliable producer stayed blocked")
                    .unwrap()
            ),
            "second blocked"
        );
        first.await.unwrap();
        second.await.unwrap();
        assert!(receiver.try_recv().is_err());
        let snapshot = receiver.snapshot();
        assert_eq!(snapshot.admitted, 3);
        assert_eq!(snapshot.dequeued, 3);
        assert_eq!(snapshot.blocked, 2);
        assert_eq!(snapshot.dropped_progress, 1);
        assert_eq!(snapshot.depth, 0);
    });
}

#[test]
fn runtime_event_mailbox_abort_while_full_still_admits_lossless_patch_group() {
    admission_test_runtime().block_on(async {
        let (sender, mut receiver) = runtime_event_channel_with_capacity(1, 0);
        sender.send(mailbox_status_event(1, "head")).unwrap();
        let event_sender = RuntimeEventSender::new(
            sender.clone(),
            RuntimeIdentity {
                session: SessionId(1),
                request: RequestId::new(2),
            },
            OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
        );
        let patch = WorkspacePatch::new(
            WorkspaceRevision::INITIAL.successor(),
            vec![WorkspaceChange::ConversationRemoved {
                channel_id: "C1".to_string(),
            }],
        )
        .unwrap();
        let producer = tokio::spawn(async move {
            event_sender.send_workspace_patch(patch);
            event_sender.send_event(RuntimeEventKind::ConversationsSynchronized);
        });
        wait_for_mailbox_block(&sender, 1).await;
        producer.abort();

        assert_eq!(mailbox_status(receiver.recv().await.unwrap()), "head");
        let patch = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("cancelled producer lost workspace patch")
            .unwrap();
        assert!(matches!(patch.kind, RuntimeEventKind::WorkspacePatch(_)));
        let completion = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("cancelled producer lost grouped completion")
            .unwrap();
        assert!(matches!(
            completion.kind,
            RuntimeEventKind::ConversationsSynchronized
        ));
        let _ = producer.await;
    });
}

#[test]
fn runtime_event_mailbox_bounds_progress_and_keeps_latest_before_terminal() {
    admission_test_runtime().block_on(async {
        let (sender, mut receiver) = runtime_event_channel_with_capacity(4, 2);
        let attachment_context = OperationContext::new(
            RuntimeOperation::AttachmentDownload,
            RuntimeTarget::Attachment("download".to_string()),
        );
        let upload_context = OperationContext::new(
            RuntimeOperation::FileUpload,
            RuntimeTarget::Upload {
                channel_id: "C1".to_string(),
                thread_ts: None,
            },
        );
        for step in 0..10_000 {
            let fraction = f64::from(step) / 9_999.0;
            sender
                .send(mailbox_event(
                    1,
                    attachment_context.clone(),
                    RuntimeEventKind::AttachmentDownloadProgress {
                        fraction,
                        label: format!("{fraction}"),
                    },
                ))
                .unwrap();
        }
        let latest = receiver.recv().await.unwrap();
        assert!(matches!(
            latest.kind,
            RuntimeEventKind::AttachmentDownloadProgress { fraction, .. }
                if fraction == 1.0
        ));
        sender
            .send(mailbox_event(
                1,
                attachment_context.clone(),
                RuntimeEventKind::AttachmentDownloadProgress {
                    fraction: 1.0,
                    label: "complete".to_string(),
                },
            ))
            .unwrap();
        sender
            .send(mailbox_event(
                2,
                upload_context,
                RuntimeEventKind::FileUploadProgress {
                    fraction: 0.5,
                    label: "upload".to_string(),
                },
            ))
            .unwrap();
        sender
            .send(mailbox_event(
                3,
                attachment_context.clone(),
                RuntimeEventKind::AttachmentDownloadProgress {
                    fraction: 0.2,
                    label: "dropped".to_string(),
                },
            ))
            .unwrap();
        assert_eq!(receiver.snapshot().depth, 2);
        assert_eq!(receiver.snapshot().coalesced_progress, 9_999);
        assert_eq!(receiver.snapshot().dropped_progress, 1);

        sender
            .send(mailbox_event(
                1,
                attachment_context,
                RuntimeEventKind::AttachmentDownloaded {
                    url: "https://example.test/file".to_string(),
                    name: "file".to_string(),
                    path: PathBuf::from("/tmp/file"),
                },
            ))
            .unwrap();
        let upload = receiver.recv().await.unwrap();
        assert!(matches!(
            upload.kind,
            RuntimeEventKind::FileUploadProgress { .. }
        ));
        let terminal = receiver.recv().await.unwrap();
        assert!(matches!(
            terminal.kind,
            RuntimeEventKind::AttachmentDownloaded { .. }
        ));
        let snapshot = receiver.snapshot();
        assert_eq!(snapshot.depth, 0);
        assert_eq!(snapshot.peak_depth, 2);
        assert_eq!(snapshot.coalesced_progress, 10_000);
        assert_eq!(snapshot.dropped_progress, 1);
    });
}

#[test]
fn runtime_event_mailbox_upload_terminal_and_error_evict_pending_progress() {
    admission_test_runtime().block_on(async {
        let (sender, mut receiver) = runtime_event_channel_with_capacity(2, 1);
        let context = OperationContext::new(
            RuntimeOperation::FileUpload,
            RuntimeTarget::Upload {
                channel_id: "C1".to_string(),
                thread_ts: None,
            },
        );
        sender
            .send(mailbox_event(
                1,
                context.clone(),
                RuntimeEventKind::FileUploadProgress {
                    fraction: 0.4,
                    label: "upload".to_string(),
                },
            ))
            .unwrap();
        sender
            .send(mailbox_event(
                1,
                context.clone(),
                RuntimeEventKind::FileUploaded("file".to_string()),
            ))
            .unwrap();
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::FileUploaded(_)
        ));
        sender
            .send(mailbox_event(
                2,
                context.clone(),
                RuntimeEventKind::FileUploadProgress {
                    fraction: 0.8,
                    label: "retry".to_string(),
                },
            ))
            .unwrap();
        sender
            .send(mailbox_event(
                2,
                context,
                RuntimeEventKind::Error(RuntimeFailure::validation("upload failed")),
            ))
            .unwrap();

        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::Error(_)
        ));
        assert!(receiver.try_recv().is_err());
        assert_eq!(receiver.snapshot().coalesced_progress, 2);
    });
}

#[test]
fn runtime_event_mailbox_receiver_close_wakes_blocked_producer() {
    admission_test_runtime().block_on(async {
        let (sender, receiver) = runtime_event_channel_with_capacity(1, 0);
        sender.send(mailbox_status_event(1, "first")).unwrap();
        let blocked_sender = sender.clone();
        let blocked = tokio::spawn(async move {
            blocked_sender
                .send(mailbox_status_event(2, "second"))
                .is_err()
        });
        wait_for_mailbox_block(&sender, 1).await;
        drop(receiver);

        assert!(tokio::time::timeout(Duration::from_secs(1), blocked)
            .await
            .expect("closed receiver did not wake producer")
            .unwrap());
        let snapshot = sender.snapshot();
        assert_eq!(snapshot.closed, 1);
        assert_eq!(snapshot.depth, 0);
    });
}

#[test]
fn runtime_event_mailbox_sender_close_drains_before_eof() {
    admission_test_runtime().block_on(async {
        let (sender, mut receiver) = runtime_event_channel_with_capacity(2, 0);
        sender.send(mailbox_status_event(1, "first")).unwrap();
        sender.send(mailbox_status_event(2, "second")).unwrap();
        drop(sender);

        assert_eq!(mailbox_status(receiver.recv().await.unwrap()), "first");
        assert_eq!(mailbox_status(receiver.recv().await.unwrap()), "second");
        assert!(receiver.recv().await.is_none());
    });
}

#[test]
fn runtime_event_mailbox_preserves_startup_failure_lifecycle_order() {
    admission_test_runtime().block_on(async {
        let (sender, mut receiver) = runtime_event_channel_with_capacity(2, 0);
        let context = OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace);
        sender
            .send(mailbox_event(
                1,
                context.clone(),
                RuntimeEventKind::WorkspaceLifecycle(WorkspaceLifecycleEvent::StartupFailed),
            ))
            .unwrap();
        sender
            .send(mailbox_event(
                1,
                context,
                RuntimeEventKind::RuntimeStartFailed(RuntimeFailure::validation("runtime failed")),
            ))
            .unwrap();
        drop(sender);

        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspaceLifecycle(WorkspaceLifecycleEvent::StartupFailed)
        ));
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::RuntimeStartFailed(_)
        ));
        assert!(receiver.recv().await.is_none());
    });
}

#[test]
fn runtime_command_admission_coalesces_queued_work_with_latest_identity() {
    let (sender, receiver) =
        RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(2, 2));
    assert_eq!(
        sender.send(admission_request(
            1,
            1,
            RuntimeCommand::LoadMedia {
                url: "https://files.example.test/video.mp4".into(),
                name: "first".into(),
            },
        )),
        RuntimeCommandAdmissionOutcome::Enqueued
    );
    assert_eq!(
        sender.send(admission_request(
            1,
            2,
            RuntimeCommand::LoadMedia {
                url: "https://files.example.test/video.mp4".into(),
                name: "latest".into(),
            },
        )),
        RuntimeCommandAdmissionOutcome::Coalesced
    );

    let queued = admission_test_runtime().block_on(receiver.recv_lane(RuntimeTaskLane::Image));
    let queued = queued.expect("coalesced request should remain queued");
    assert_eq!(queued.request.identity.request, RequestId::new(2));
    assert!(matches!(
        &queued.request.command,
        RuntimeCommand::LoadMedia { name, .. } if name == "latest"
    ));
    drop(queued);
    assert_eq!(
        sender.snapshot(),
        RuntimeCommandAdmissionSnapshot {
            admitted: 2,
            dequeued: 1,
            coalesced: 1,
            peak_depth: 1,
            peak_in_flight: 1,
            ..RuntimeCommandAdmissionSnapshot::default()
        }
    );
}

#[test]
fn runtime_command_admission_keeps_one_latest_follow_up_for_in_flight_key() {
    let runtime = admission_test_runtime();
    runtime.block_on(async {
        let (sender, receiver) =
            RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(3, 3));
        let media = |request: u64, name: &str| {
            admission_request(
                1,
                request,
                RuntimeCommand::LoadMedia {
                    url: "https://files.example.test/video.mp4".into(),
                    name: name.into(),
                },
            )
        };
        assert_eq!(
            sender.send(media(1, "running")),
            RuntimeCommandAdmissionOutcome::Enqueued
        );
        let running = receiver
            .recv_lane(RuntimeTaskLane::Image)
            .await
            .expect("first media request should reserve its key");
        assert_eq!(
            sender.send(media(2, "pending")),
            RuntimeCommandAdmissionOutcome::Enqueued
        );
        assert_eq!(
            sender.send(media(3, "latest")),
            RuntimeCommandAdmissionOutcome::Coalesced
        );
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            receiver.recv_lane(RuntimeTaskLane::Image),
        )
        .await
        .is_err());

        drop(running);
        let follow_up = tokio::time::timeout(
            Duration::from_secs(1),
            receiver.recv_lane(RuntimeTaskLane::Image),
        )
        .await
        .expect("latest follow-up stayed blocked after completion")
        .expect("admission receiver closed early");
        assert_eq!(follow_up.request.identity.request, RequestId::new(3));
        assert!(matches!(
            &follow_up.request.command,
            RuntimeCommand::LoadMedia { name, .. } if name == "latest"
        ));
        let snapshot = sender.snapshot();
        assert_eq!(snapshot.peak_in_flight, 1);
        assert_eq!(snapshot.coalesced, 1);
    });
}

#[test]
fn runtime_command_admission_supersedes_queued_navigation_in_place() {
    let (sender, receiver) =
        RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(2, 2));
    assert_eq!(
        sender.send(admission_request(
            1,
            1,
            RuntimeCommand::LoadHistory {
                channel_id: "C1".into(),
            },
        )),
        RuntimeCommandAdmissionOutcome::Enqueued
    );
    assert_eq!(
        sender.send(admission_request(
            1,
            2,
            RuntimeCommand::SearchMessages {
                query: "latest".into(),
            },
        )),
        RuntimeCommandAdmissionOutcome::Superseded
    );

    let queued = admission_test_runtime()
        .block_on(receiver.recv_lane(RuntimeTaskLane::Navigation))
        .expect("superseding request should remain queued");
    assert_eq!(queued.request.identity.request, RequestId::new(2));
    assert!(matches!(
        &queued.request.command,
        RuntimeCommand::SearchMessages { .. }
    ));
    let snapshot = sender.snapshot();
    assert_eq!(snapshot.superseded, 1);
    assert_eq!(snapshot.depth, 0);
    assert_eq!(snapshot.peak_depth, 1);
}

#[test]
fn runtime_command_admission_serializes_ordered_actions_per_target() {
    let runtime = admission_test_runtime();
    runtime.block_on(async {
        let (sender, receiver) =
            RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(2, 2));
        for (request, add) in [(1, true), (2, false)] {
            assert_eq!(
                sender.send(admission_request(
                    1,
                    request,
                    RuntimeCommand::SetReaction {
                        channel_id: "C1".into(),
                        ts: "1.0".into(),
                        name: "eyes".into(),
                        add,
                        thread_ts: None,
                    },
                )),
                RuntimeCommandAdmissionOutcome::Enqueued
            );
        }

        let first = receiver
            .recv_lane(RuntimeTaskLane::Interactive)
            .await
            .expect("first reaction should reserve its target");
        assert_eq!(first.request.identity.request, RequestId::new(1));
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            receiver.recv_lane(RuntimeTaskLane::Interactive),
        )
        .await
        .is_err());
        drop(first);
        let second = tokio::time::timeout(
            Duration::from_secs(1),
            receiver.recv_lane(RuntimeTaskLane::Interactive),
        )
        .await
        .expect("second reaction stayed blocked after completion")
        .expect("admission receiver closed early");
        assert_eq!(second.request.identity.request, RequestId::new(2));
    });
}

#[test]
fn runtime_command_admission_serializes_all_huddle_commands() {
    admission_test_runtime().block_on(async {
        let (sender, receiver) =
            RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(2, 2));
        assert_eq!(
            sender.send(admission_request(
                1,
                1,
                RuntimeCommand::Huddle(HuddleCommand::OpenPreflight {
                    call_id: "R1".into(),
                }),
            )),
            RuntimeCommandAdmissionOutcome::Enqueued
        );
        assert_eq!(
            sender.send(admission_request(
                1,
                2,
                RuntimeCommand::Huddle(HuddleCommand::SetMuted(true)),
            )),
            RuntimeCommandAdmissionOutcome::Enqueued
        );

        let first = receiver
            .recv_lane(RuntimeTaskLane::Interactive)
            .await
            .expect("first huddle command should reserve shared actor target");
        assert_eq!(first.request.identity.request, RequestId::new(1));
        assert_eq!(
            first.request.descriptor.context.target,
            RuntimeTarget::Huddle("R1".into())
        );
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            receiver.recv_lane(RuntimeTaskLane::Interactive),
        )
        .await
        .is_err());

        drop(first);
        let second = tokio::time::timeout(
            Duration::from_secs(1),
            receiver.recv_lane(RuntimeTaskLane::Interactive),
        )
        .await
        .expect("second huddle command stayed blocked after completion")
        .expect("huddle admission receiver closed early");
        assert_eq!(second.request.identity.request, RequestId::new(2));
        assert_eq!(
            second.request.descriptor.context.target,
            RuntimeTarget::Huddle("active".into())
        );
    });
}

#[test]
fn runtime_command_admission_rejects_durable_overload_without_blocking() {
    let (sender, receiver) =
        RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(1, 1));
    assert_eq!(
        sender.send(admission_request(
            1,
            1,
            RuntimeCommand::UpdateAttentionPreferences(AttentionPreferences::default()),
        )),
        RuntimeCommandAdmissionOutcome::Enqueued
    );

    let rejected = sender
        .try_send(admission_request(
            1,
            2,
            RuntimeCommand::UpdateAttentionPreferences(AttentionPreferences::default()),
        ))
        .expect_err("full durable lane should reject without waiting");
    assert_eq!(
        rejected.outcome,
        RuntimeCommandAdmissionOutcome::DroppedAtCapacity
    );
    assert_eq!(rejected.request.identity.request, RequestId::new(2));

    let runtime = admission_test_runtime();
    let first = runtime
        .block_on(receiver.recv_lane(RuntimeTaskLane::Interactive))
        .expect("first durable command should remain queued");
    assert_eq!(first.request.identity.request, RequestId::new(1));
    drop(first);

    let snapshot = sender.snapshot();
    assert_eq!(snapshot.cancelled, 0);
    assert_eq!(snapshot.rejected, 1);
    assert_eq!(snapshot.depth, 0);
}

#[test]
fn runtime_command_admission_never_evicts_distinct_queued_work() {
    let (sender, receiver) =
        RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(2, 2));
    assert_eq!(
        sender.send(admission_request(
            1,
            1,
            RuntimeCommand::DownloadAttachment {
                url: "https://files.example.test/report.pdf".into(),
                name: "report.pdf".into(),
            },
        )),
        RuntimeCommandAdmissionOutcome::Enqueued
    );
    assert_eq!(
        sender.send(admission_request(
            1,
            2,
            RuntimeCommand::LoadMedia {
                url: "https://files.example.test/old.mp4".into(),
                name: "old".into(),
            },
        )),
        RuntimeCommandAdmissionOutcome::Enqueued
    );
    let rejected = sender
        .try_send(admission_request(
            1,
            3,
            RuntimeCommand::LoadMedia {
                url: "https://files.example.test/latest.mp4".into(),
                name: "latest".into(),
            },
        ))
        .expect_err("distinct full-lane request should be returned to its caller");
    assert_eq!(
        rejected.outcome,
        RuntimeCommandAdmissionOutcome::DroppedAtCapacity
    );
    assert!(matches!(
        rejected.request.command,
        RuntimeCommand::LoadMedia { name, .. } if name == "latest"
    ));

    let runtime = admission_test_runtime();
    let durable = runtime
        .block_on(receiver.recv_lane(RuntimeTaskLane::Image))
        .expect("durable attachment should remain queued");
    assert!(matches!(
        &durable.request.command,
        RuntimeCommand::DownloadAttachment { .. }
    ));
    let original = runtime
        .block_on(receiver.recv_lane(RuntimeTaskLane::Image))
        .expect("original distinct media should remain queued");
    assert!(matches!(
        &original.request.command,
        RuntimeCommand::LoadMedia { name, .. } if name == "old"
    ));
    let snapshot = sender.snapshot();
    assert_eq!(snapshot.cancelled, 0);
    assert_eq!(snapshot.rejected, 1);
    assert_eq!(snapshot.peak_depth, 2);
}

#[test]
fn runtime_command_admission_rejects_delayed_older_key_without_replacement() {
    let (sender, receiver) =
        RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(1, 1));
    let media = |name: &str| RuntimeCommand::LoadMedia {
        url: "https://files.example.test/video.mp4".into(),
        name: name.into(),
    };
    assert_eq!(
        sender.send(admission_request(2, 2, media("current"))),
        RuntimeCommandAdmissionOutcome::Enqueued
    );
    assert_eq!(
        sender.send(admission_request(2, 1, media("delayed"))),
        RuntimeCommandAdmissionOutcome::DroppedStale
    );
    assert_eq!(
        sender.send(admission_request(1, 3, media("old-session"))),
        RuntimeCommandAdmissionOutcome::StaleSession
    );

    let queued = admission_test_runtime()
        .block_on(receiver.recv_lane(RuntimeTaskLane::Image))
        .expect("current-session request should remain queued");
    assert_eq!(queued.request.identity.session, SessionId(2));
    assert!(matches!(
        &queued.request.command,
        RuntimeCommand::LoadMedia { name, .. } if name == "current"
    ));
}

#[test]
fn runtime_command_admission_preserves_control_fifo_across_sessions() {
    let (sender, receiver) =
        RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(2, 2));
    assert_eq!(
        sender.send(admission_request(1, 1, RuntimeCommand::SignOut)),
        RuntimeCommandAdmissionOutcome::Enqueued
    );
    assert_eq!(
        sender.send(admission_request(2, 2, RuntimeCommand::Disconnect)),
        RuntimeCommandAdmissionOutcome::Enqueued
    );

    let runtime = admission_test_runtime();
    let first = runtime
        .block_on(receiver.recv_control())
        .expect("sign-out should remain queued");
    assert!(matches!(&first.request.command, RuntimeCommand::SignOut));
    drop(first);
    let second = runtime
        .block_on(receiver.recv_control())
        .expect("disconnect should remain queued");
    assert!(matches!(
        &second.request.command,
        RuntimeCommand::Disconnect
    ));
}

#[test]
fn runtime_command_drivers_do_not_overtake_sign_out_with_newer_authentication() {
    let runtime = admission_test_runtime();
    runtime.block_on(async {
        let (sender, receiver) =
            RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(2, 2));
        let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
        let mut ready = spawn_runtime_command_drivers(&receiver, &limits);
        assert_eq!(
            sender.send(admission_request(1, 1, RuntimeCommand::SignOut)),
            RuntimeCommandAdmissionOutcome::Enqueued
        );
        assert_eq!(
            sender.send(admission_request(2, 2, RuntimeCommand::LoadStoredToken)),
            RuntimeCommandAdmissionOutcome::Enqueued
        );

        let first = tokio::time::timeout(Duration::from_secs(1), ready.recv())
            .await
            .expect("sign-out did not reach ready dispatch")
            .expect("ready drivers closed before sign-out");
        assert_eq!(first.request.identity.session, SessionId(1));
        assert!(matches!(&first.request.command, RuntimeCommand::SignOut));
        drop(first);

        let second = tokio::time::timeout(Duration::from_secs(1), ready.recv())
            .await
            .expect("authentication did not reach ready dispatch")
            .expect("ready drivers closed before authentication");
        assert_eq!(second.request.identity.session, SessionId(2));
        assert!(matches!(
            &second.request.command,
            RuntimeCommand::LoadStoredToken
        ));
    });
}

#[test]
fn runtime_command_admission_drains_ordered_work_before_newer_session_dispatch() {
    let runtime = admission_test_runtime();
    runtime.block_on(async {
        let (sender, receiver) =
            RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(3, 3));
        assert_eq!(
            sender.send(admission_request(
                1,
                1,
                RuntimeCommand::SetReaction {
                    channel_id: "C1".into(),
                    ts: "1.0".into(),
                    name: "eyes".into(),
                    add: true,
                    thread_ts: None,
                },
            )),
            RuntimeCommandAdmissionOutcome::Enqueued
        );
        assert_eq!(
            sender.send(admission_request(2, 2, RuntimeCommand::Disconnect)),
            RuntimeCommandAdmissionOutcome::Enqueued
        );
        assert_eq!(
            sender.send(admission_request(
                2,
                3,
                RuntimeCommand::UpdateAttentionPreferences(AttentionPreferences::default()),
            )),
            RuntimeCommandAdmissionOutcome::Enqueued
        );

        assert!(
            tokio::time::timeout(Duration::from_millis(20), receiver.recv_control())
                .await
                .is_err()
        );
        let ordered = receiver
            .recv_lane(RuntimeTaskLane::Interactive)
            .await
            .expect("older durable work should remain dispatchable");
        assert_eq!(ordered.request.identity.session, SessionId(1));
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            receiver.recv_lane(RuntimeTaskLane::Interactive),
        )
        .await
        .is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), receiver.recv_control())
                .await
                .is_err()
        );

        drop(ordered);
        let control = tokio::time::timeout(Duration::from_secs(1), receiver.recv_control())
            .await
            .expect("new-session control stayed blocked after durable completion")
            .expect("control receiver closed early");
        assert_eq!(control.request.identity.session, SessionId(2));
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            receiver.recv_lane(RuntimeTaskLane::Interactive),
        )
        .await
        .is_err());

        drop(control);
        let newer = tokio::time::timeout(
            Duration::from_secs(1),
            receiver.recv_lane(RuntimeTaskLane::Interactive),
        )
        .await
        .expect("new-session work stayed blocked after session dispatch")
        .expect("interactive receiver closed early");
        assert_eq!(newer.request.identity.session, SessionId(2));
    });
}

#[test]
fn runtime_command_admission_rejects_delayed_work_from_an_older_session() {
    let (sender, _receiver) =
        RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(2, 2));
    assert_eq!(
        sender.send(admission_request(2, 1, RuntimeCommand::LoadStoredToken)),
        RuntimeCommandAdmissionOutcome::Enqueued
    );
    let rejected = sender
        .try_send(admission_request(
            1,
            2,
            RuntimeCommand::SetConversationStarred {
                channel_id: "C1".into(),
                starred: true,
            },
        ))
        .expect_err("older-session work must not enter after a newer session");
    assert_eq!(
        rejected.outcome,
        RuntimeCommandAdmissionOutcome::StaleSession
    );
    assert!(matches!(
        rejected.request.command,
        RuntimeCommand::SetConversationStarred { .. }
    ));
}

#[test]
fn runtime_command_lane_driver_reserves_capacity_before_ready_dispatch() {
    let runtime = admission_test_runtime();
    runtime.block_on(async {
        let (sender, receiver) =
            RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(2, 2));
        let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
        let mut ready = spawn_runtime_command_drivers(&receiver, &limits);
        for request in 1..=2 {
            assert_eq!(
                sender.send(admission_request(
                    1,
                    request,
                    RuntimeCommand::LoadMedia {
                        url: format!("https://files.example.test/{request}.mp4"),
                        name: request.to_string(),
                    },
                )),
                RuntimeCommandAdmissionOutcome::Enqueued
            );
        }

        let first = tokio::time::timeout(Duration::from_secs(1), ready.recv())
            .await
            .expect("first image request did not become ready")
            .expect("ready drivers closed early");
        assert_eq!(first.request.identity.request, RequestId::new(1));
        assert!(first.lease.is_some());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), ready.recv())
                .await
                .is_err()
        );

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), ready.recv())
            .await
            .expect("second image request did not wait for capacity")
            .expect("ready drivers closed early");
        assert_eq!(second.request.identity.request, RequestId::new(2));
        assert!(second.lease.is_some());
    });
}

#[test]
fn runtime_command_sender_close_drains_ready_work_before_admission_idle() {
    let runtime = admission_test_runtime();
    runtime.block_on(async {
        let (sender, receiver) =
            RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(2, 2));
        let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
        let mut ready = spawn_runtime_command_drivers(&receiver, &limits);
        assert_eq!(
            sender.send(admission_request(
                1,
                1,
                RuntimeCommand::UpdateAttentionPreferences(AttentionPreferences::default()),
            )),
            RuntimeCommandAdmissionOutcome::Enqueued
        );
        drop(sender);

        let admitted = tokio::time::timeout(Duration::from_secs(1), ready.recv())
            .await
            .expect("accepted command did not reach ready dispatch")
            .expect("ready drivers closed before draining accepted work");
        let limits_for_idle = limits.clone();
        let mut idle = tokio::spawn(async move {
            limits_for_idle.wait_idle().await;
        });
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut idle)
            .await
            .is_err());

        drop(admitted);
        tokio::time::timeout(Duration::from_secs(1), idle)
            .await
            .expect("admission idle barrier stayed blocked after lease release")
            .expect("admission idle barrier task failed");
        assert!(tokio::time::timeout(Duration::from_secs(1), ready.recv())
            .await
            .expect("ready drivers did not close after sender shutdown")
            .is_none());
    });
}

#[test]
fn runtime_command_task_admission_waits_for_accepted_work_to_finish() {
    let runtime = admission_test_runtime();
    runtime.block_on(async {
        let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
        let permit = limits.acquire(RuntimeTaskLane::Image).await;
        let limits_for_drain = limits.clone();
        let mut drain = tokio::spawn(async move {
            limits_for_drain.wait_idle().await;
        });
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut drain)
            .await
            .is_err());
        drop(permit);
        tokio::time::timeout(Duration::from_secs(1), drain)
            .await
            .expect("accepted work barrier stayed blocked")
            .expect("accepted work barrier task failed");
    });
}

#[test]
fn runtime_command_supersedable_work_bypasses_saturated_task_admission() {
    let runtime = admission_test_runtime();
    runtime.block_on(async {
        let (sender, receiver) =
            RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(2, 2));
        let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
        let mut ready = spawn_runtime_command_drivers(&receiver, &limits);
        assert_eq!(
            sender.send(admission_request(
                1,
                1,
                RuntimeCommand::UpdateAttentionPreferences(AttentionPreferences::default()),
            )),
            RuntimeCommandAdmissionOutcome::Enqueued
        );

        let durable = tokio::time::timeout(Duration::from_secs(1), ready.recv())
            .await
            .expect("durable command did not become ready")
            .expect("ready drivers closed early");
        assert!(durable.lease.is_some());

        assert_eq!(
            sender.send(admission_request(1, 2, RuntimeCommand::LoadStoredToken)),
            RuntimeCommandAdmissionOutcome::Enqueued
        );
        let superseding = tokio::time::timeout(Duration::from_secs(1), ready.recv())
            .await
            .expect("supersedable command waited for saturated task admission")
            .expect("ready drivers closed early");
        assert_eq!(superseding.request.identity.request, RequestId::new(2));
        assert!(matches!(
            superseding.request.command,
            RuntimeCommand::LoadStoredToken
        ));
        assert!(superseding.lease.is_none());
    });
}

#[test]
fn runtime_command_ready_dispatch_prioritizes_control_and_rotates_lanes() {
    let runtime = admission_test_runtime();
    runtime.block_on(async {
        let (control_sender, control) = mpsc::channel(1);
        let (navigation_sender, navigation) = mpsc::channel(2);
        let (image_sender, image) = mpsc::channel(1);
        let mut lanes = std::array::from_fn(|_| None);
        lanes[RuntimeTaskLane::Navigation.index()] = Some(navigation);
        lanes[RuntimeTaskLane::Image.index()] = Some(image);
        let mut ready = RuntimeReadyReceivers {
            control: Some(control),
            lanes,
            next_lane: 0,
            control_budget: RUNTIME_CONTROL_DISPATCH_BURST,
        };
        let ready_request = |request| RuntimeReadyRequest {
            request,
            lease: None,
            session_start: None,
        };

        navigation_sender
            .try_send(ready_request(admission_request(
                1,
                1,
                RuntimeCommand::LoadHistory {
                    channel_id: "C1".into(),
                },
            )))
            .unwrap();
        navigation_sender
            .try_send(ready_request(admission_request(
                1,
                2,
                RuntimeCommand::SearchMessages {
                    query: "next".into(),
                },
            )))
            .unwrap();
        image_sender
            .try_send(ready_request(admission_request(
                1,
                3,
                RuntimeCommand::LoadMedia {
                    url: "https://files.example.test/video.mp4".into(),
                    name: "video".into(),
                },
            )))
            .unwrap();
        control_sender
            .try_send(ready_request(admission_request(
                1,
                4,
                RuntimeCommand::Disconnect,
            )))
            .unwrap();

        let control = ready.recv().await.expect("control command should be ready");
        assert!(matches!(
            control.request.command,
            RuntimeCommand::Disconnect
        ));
        let navigation = ready
            .recv()
            .await
            .expect("first navigation command should be ready");
        assert_eq!(navigation.request.identity.request, RequestId::new(1));
        let image = ready.recv().await.expect("image command should be ready");
        assert_eq!(image.request.identity.request, RequestId::new(3));
        let navigation = ready
            .recv()
            .await
            .expect("second navigation command should be ready");
        assert_eq!(navigation.request.identity.request, RequestId::new(2));
    });
}

#[test]
fn runtime_command_ready_dispatch_bounds_control_bursts() {
    let runtime = admission_test_runtime();
    runtime.block_on(async {
        let (control_sender, control) = mpsc::channel(3);
        let (navigation_sender, navigation) = mpsc::channel(1);
        let mut lanes = std::array::from_fn(|_| None);
        lanes[RuntimeTaskLane::Navigation.index()] = Some(navigation);
        let mut ready = RuntimeReadyReceivers {
            control: Some(control),
            lanes,
            next_lane: 0,
            control_budget: 2,
        };
        let ready_request = |request| RuntimeReadyRequest {
            request,
            lease: None,
            session_start: None,
        };
        for request in 1..=3 {
            control_sender
                .try_send(ready_request(admission_request(
                    1,
                    request,
                    RuntimeCommand::Disconnect,
                )))
                .unwrap();
        }
        navigation_sender
            .try_send(ready_request(admission_request(
                1,
                4,
                RuntimeCommand::LoadHistory {
                    channel_id: "C1".into(),
                },
            )))
            .unwrap();

        assert_eq!(
            ready.recv().await.unwrap().request.identity.request,
            RequestId::new(1)
        );
        assert_eq!(
            ready.recv().await.unwrap().request.identity.request,
            RequestId::new(2)
        );
        assert_eq!(
            ready.recv().await.unwrap().request.identity.request,
            RequestId::new(4)
        );
        assert_eq!(
            ready.recv().await.unwrap().request.identity.request,
            RequestId::new(3)
        );
    });
}

#[test]
fn runtime_command_admission_closed_receiver_returns_original_request() {
    let (sender, receiver) =
        RuntimeCommandAdmission::channel(RuntimeCommandAdmissionConfig::uniform(1, 1));
    drop(receiver);
    let rejected = sender
        .try_send(admission_request(
            1,
            1,
            RuntimeCommand::UpdateAttentionPreferences(AttentionPreferences::default()),
        ))
        .expect_err("closed receiver should reject immediately");
    assert_eq!(rejected.outcome, RuntimeCommandAdmissionOutcome::Closed);
    assert_eq!(rejected.request.identity.request, RequestId::new(1));
}

fn timeline_patch_summary(changes: &[WorkspaceChange]) -> Vec<(String, Option<String>)> {
    changes
        .iter()
        .flat_map(|change| match change {
            WorkspaceChange::TimelineChanged { changes, .. } => changes
                .iter()
                .map(|change| match change {
                    MessageChange::Upsert(message) => {
                        (message.ts.clone(), Some(message.body_text()))
                    }
                    MessageChange::Remove { message_ts } => (message_ts.clone(), None),
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn conversation_patch_summary(
    changes: &[WorkspaceChange],
) -> Vec<(&'static str, String, Option<String>)> {
    let mut summary = changes
        .iter()
        .filter_map(|change| match change {
            WorkspaceChange::ConversationUpsert(conversation)
            | WorkspaceChange::ConversationMetadataUpsert(conversation) => {
                Some(("upsert", conversation.id.clone(), conversation.name.clone()))
            }
            WorkspaceChange::ConversationRemoved { channel_id } => {
                Some(("remove", channel_id.clone(), None))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    summary.sort();
    summary
}

fn conversation_store_summary(
    changes: &[StoreChange],
) -> Vec<(&'static str, String, Option<String>)> {
    let mut summary = changes
        .iter()
        .filter_map(|change| match change {
            StoreChange::ConversationUpsert(conversation)
            | StoreChange::ConversationMetadataUpsert(conversation)
            | StoreChange::ConversationMembershipUpsert(conversation) => {
                Some(("upsert", conversation.id.clone(), conversation.name.clone()))
            }
            StoreChange::ConversationRemoved { channel_id } => {
                Some(("remove", channel_id.clone(), None))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    summary.sort();
    summary
}

#[test]
fn workspace_adapter_returns_authoritative_conversation_membership_commit() {
    let workspace = WorkspaceReducerAdapter::default();
    let general = SlackConversation {
        id: "C1".into(),
        name: Some("general".into()),
        is_channel: Some(true),
        ..Default::default()
    };
    let removable = SlackConversation {
        id: "C_OLD".into(),
        name: Some("old".into()),
        is_channel: Some(true),
        ..Default::default()
    };
    workspace
        .apply(
            MutationOrigin::Cache,
            WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                conversations: vec![general.clone(), removable],
                ..Default::default()
            }),
        )
        .expect("cache hydration should return its authoritative commit");
    let snapshot_base = workspace.revision();

    let mut renamed = general.clone();
    renamed.name = Some("announcements".into());
    workspace
        .apply(
            MutationOrigin::Realtime,
            WorkspaceMutation::ConversationUpsert(renamed.clone()),
        )
        .expect("realtime metadata should return its authoritative commit");
    let local_membership = SlackConversation {
        id: "C_LOCAL".into(),
        name: Some("joined-during-request".into()),
        is_channel: Some(true),
        ..Default::default()
    };
    workspace
        .apply(
            MutationOrigin::Local,
            WorkspaceMutation::ConversationUpsert(local_membership.clone()),
        )
        .expect("local membership should return its authoritative commit");

    let discovered = SlackConversation {
        id: "C2".into(),
        name: Some("new-channel".into()),
        is_channel: Some(true),
        ..Default::default()
    };
    let reduction = workspace
        .apply(
            MutationOrigin::WebApi,
            WorkspaceMutation::MembershipSnapshot(SnapshotEnvelope::new(
                snapshot_base,
                ConversationMembershipSnapshot {
                    conversations: vec![general, discovered],
                    starred_ids: None,
                },
            )),
        )
        .expect("membership changes should return one authoritative commit");

    let expected = vec![
        ("remove", "C_OLD".to_string(), None),
        ("upsert", "C2".to_string(), Some("new-channel".to_string())),
    ];
    assert_eq!(
        conversation_patch_summary(reduction.patch().changes()),
        expected
    );
    let store_batch = reduction
        .store_batch()
        .expect("conversation membership must be persisted");
    assert_eq!(store_batch.revision(), reduction.patch().revision());
    assert_eq!(
        conversation_store_summary(store_batch.changes()),
        expected,
        "the patch and store batch must describe the same authority change"
    );

    assert!(
        workspace
            .apply(
                MutationOrigin::Realtime,
                WorkspaceMutation::ConversationUpsert(renamed),
            )
            .is_none(),
        "a stale snapshot must not overwrite newer realtime metadata"
    );
    assert!(
        workspace
            .apply(
                MutationOrigin::Local,
                WorkspaceMutation::ConversationUpsert(local_membership),
            )
            .is_none(),
        "a stale snapshot must not remove newer local membership"
    );
}

#[test]
fn workspace_adapter_submits_each_conversation_store_batch_once() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-batch-submit-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let conversation = SlackConversation {
            id: "C1".into(),
            name: Some("general".into()),
            ..Default::default()
        };
        assert!(
            workspace
                .apply_persisted(
                    Some(&store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(conversation.clone()),
                )
                .await
                .unwrap()
                .len()
                == 1
        );
        assert!(workspace
            .apply_persisted(
                Some(&store),
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(conversation),
            )
            .await
            .unwrap()
            .is_empty());

        let stored = store.load_conversations().await.unwrap().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, "C1");
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_adapter_publishes_a_persisted_patch_as_a_session_scoped_event() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-patch-event-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let (sender, mut receiver) = runtime_event_channel();
        let session = SessionId::default().next();
        let events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session,
                request: RequestId::new(9),
            },
            OperationContext::new(
                RuntimeOperation::OpenConversation,
                RuntimeTarget::Channel("C1".to_string()),
            ),
        );
        let reductions = workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "C1".to_string(),
                    name: Some("general".to_string()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert_eq!(reductions.len(), 1);

        let event = receiver.recv().await.unwrap();
        assert_eq!(event.meta.session, session);
        assert_eq!(event.meta.request, None);
        assert_eq!(
            event.meta.context,
            OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace)
        );
        assert!(matches!(
            event.kind,
            RuntimeEventKind::WorkspacePatch(patch)
                if patch.revision() == WorkspaceRevision::INITIAL.successor()
                    && conversation_patch_summary(patch.changes())
                        == vec![("upsert", "C1".to_string(), Some("general".to_string()))]
        ));
        assert!(receiver.try_recv().is_err());
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn cached_bootstrap_publishes_one_typed_patch_and_keeps_other_projections() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-cached-bootstrap-event-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let conversation = SlackConversation {
            id: "C1".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        store
            .store_conversations(std::slice::from_ref(&conversation))
            .await
            .unwrap();
        let user_names = HashMap::from([("U1".to_string(), "Ada".to_string())]);
        store.store_user_names(&user_names).await.unwrap();
        let custom_emojis = HashMap::from([(
            "shipit".to_string(),
            "https://example.test/shipit.png".to_string(),
        )]);
        store.store_custom_emojis(&custom_emojis).await.unwrap();
        let root = SlackMessage {
            ts: "1.0".to_string(),
            reply_count: Some(1),
            latest_reply: Some("2.0".to_string()),
            ..Default::default()
        };
        let reply = SlackMessage {
            ts: "2.0".to_string(),
            thread_ts: Some("1.0".to_string()),
            ..Default::default()
        };
        let mut threads = crate::thread_catalog::ThreadCatalog::default();
        threads.observe_history("C1", &[root, reply]);
        let thread_records = threads.into_records();
        assert_eq!(thread_records.len(), 1);
        store.store_thread_catalog(&thread_records).await.unwrap();

        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace),
            workspace_patch_send_gate: None,
        };
        let (huddles, _huddle_receiver) = huddle_actor_channel();
        let connection = RuntimeConnection {
            slack: SlackApi::new(StoredToken {
                access_token: "test-token".to_string(),
                token_type: None,
                scope: None,
                refresh_token: None,
                expires_in: None,
                expires_at: None,
                team_id: None,
                team_name: None,
                user_id: None,
                client_id: None,
                browser_cookie_d: None,
                user_agent: None,
            }),
            workspace_url: None,
            workspace_store: Some(store.clone()),
            image_cache_scope: "test-workspace".to_string(),
            workspace: workspace.clone(),
            current_user_id: None,
            user_cache: Arc::new(Mutex::new(HashMap::new())),
            message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(8))),
            conversation_star_sync: ConversationStarSyncGate::default(),
            user_status_sync: UserStatusSync::default(),
            team_id: None,
            huddles,
            read_flusher: ReadFlusherHandle::new(
                Arc::new(Mutex::new(ReadFlusherQueue::new())),
                Arc::new(Notify::new()),
            ),
            scheduler: Arc::new(Mutex::new(SyncScheduler::new(
                SchedulerConfig::new(256, 8, 5).unwrap(),
            ))),
            pending_jobs: Arc::new(Mutex::new(HashMap::new())),
            next_job_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cached_bootstrap_load_gate: None,
        };

        load_cached_bootstrap(&events, &connection).await;

        let delivered = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(
            delivered
                .iter()
                .map(|event| match &event.kind {
                    RuntimeEventKind::WorkspacePatch(_) => "patch",
                    RuntimeEventKind::EmojiCatalogLoaded(_) => "emoji",
                    other => panic!("unexpected cache projection event {other:?}"),
                })
                .collect::<Vec<_>>(),
            vec!["emoji", "patch"]
        );
        let patches = delivered
            .iter()
            .filter_map(|event| match &event.kind {
                RuntimeEventKind::WorkspacePatch(patch) => Some(patch),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].revision().value(), 1);
        assert!(matches!(
            patches[0].changes(),
            [WorkspaceChange::BootstrapReset(data)]
                  if data.conversations == vec![conversation.clone()]
                      && data.users.iter().any(|user| {
                          user.id.as_deref() == Some("U1")
                              && user.display_name().as_deref() == Some("Ada")
                      })
                      && data.threads == thread_records
        ));
        assert!(delivered.iter().any(|event| {
            matches!(
                &event.kind,
                RuntimeEventKind::EmojiCatalogLoaded(emojis) if emojis == &custom_emojis
            )
        }));
        assert_eq!(
            workspace
                .conversations()
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<Vec<_>>(),
            vec!["C1"]
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn cached_bootstrap_admission_preserves_opened_conversations_for_empty_and_nonempty_cache() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        for cache_has_conversation in [false, true] {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "conduit-workspace-cache-open-race-{}-{nonce}-{cache_has_conversation}",
                std::process::id()
            ));
            let store = WorkspaceStore::new(directory.clone(), "T1:U1");
            let cached = SlackConversation {
                id: "C_CACHE".to_string(),
                name: Some("cached".to_string()),
                is_channel: Some(true),
                ..Default::default()
            };
            if cache_has_conversation {
                store
                    .store_conversations(std::slice::from_ref(&cached))
                    .await
                    .unwrap();
            }
            store
                .store_user_names(&HashMap::from([("U1".to_string(), "Ada".to_string())]))
                .await
                .unwrap();

            let workspace = WorkspaceReducerAdapter::default();
            let (sender, mut receiver) = runtime_event_channel();
            let events = RuntimeEventSender {
                sender,
                session: SessionId::default().next(),
                request: None,
                fallback: OperationContext::new(
                    RuntimeOperation::Startup,
                    RuntimeTarget::Workspace,
                ),
                workspace_patch_send_gate: None,
            };
            let (load_started, load_reached) = std::sync::mpsc::channel();
            let (release_load, load_release) = std::sync::mpsc::channel();
            let (huddles, _huddle_receiver) = huddle_actor_channel();
            let connection = RuntimeConnection {
                slack: SlackApi::new(StoredToken {
                    access_token: "test-token".to_string(),
                    token_type: None,
                    scope: None,
                    refresh_token: None,
                    expires_in: None,
                    expires_at: None,
                    team_id: None,
                    team_name: None,
                    user_id: None,
                    client_id: None,
                    browser_cookie_d: None,
                    user_agent: None,
                }),
                workspace_url: None,
                workspace_store: Some(store.clone()),
                image_cache_scope: "test-workspace".to_string(),
                workspace: workspace.clone(),
                current_user_id: None,
                user_cache: Arc::new(Mutex::new(HashMap::new())),
                message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(8))),
                conversation_star_sync: ConversationStarSyncGate::default(),
                user_status_sync: UserStatusSync::default(),
                team_id: None,
                huddles,
                read_flusher: ReadFlusherHandle::new(
                    Arc::new(Mutex::new(ReadFlusherQueue::new())),
                    Arc::new(Notify::new()),
                ),
                scheduler: Arc::new(Mutex::new(SyncScheduler::new(
                    SchedulerConfig::new(256, 8, 5).unwrap(),
                ))),
                pending_jobs: Arc::new(Mutex::new(HashMap::new())),
                next_job_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                cached_bootstrap_load_gate: Some(Arc::new(TestWorkspacePatchSendGate {
                    started: load_started,
                    release: Mutex::new(load_release),
                })),
            };
            let load_events = events.clone();
            let loading = tokio::spawn(async move {
                load_cached_bootstrap(&load_events, &connection).await;
            });
            load_reached
                .recv_timeout(Duration::from_secs(5))
                .expect("cache load did not retain admission at the test gate");

            let open_workspace = workspace.clone();
            let open_store = store.clone();
            let open_events = events.clone();
            let (open_started, open_reached) = tokio::sync::oneshot::channel();
            let opening = tokio::spawn(async move {
                let _ = open_started.send(());
                open_workspace
                    .apply_persisted_and_publish(
                        Some(&open_store),
                        &open_events,
                        MutationOrigin::Local,
                        WorkspaceMutation::ConversationUpsert(SlackConversation {
                            id: "C_LOCAL".to_string(),
                            name: Some("opened".to_string()),
                            is_channel: Some(true),
                            ..Default::default()
                        }),
                    )
                    .await
            });
            open_reached.await.unwrap();
            tokio::task::yield_now().await;
            assert!(!opening.is_finished());
            assert_eq!(workspace.revision(), WorkspaceRevision::INITIAL);

            release_load.send(()).unwrap();
            loading.await.unwrap();
            opening.await.unwrap().unwrap();

            let ids = workspace
                .conversations()
                .into_iter()
                .map(|conversation| conversation.id)
                .collect::<Vec<_>>();
            assert_eq!(
                ids,
                if cache_has_conversation {
                    vec!["C_CACHE".to_string(), "C_LOCAL".to_string()]
                } else {
                    vec!["C_LOCAL".to_string()]
                }
            );
            let delivered = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
            assert!(matches!(
                delivered.first().map(|event| &event.kind),
                Some(RuntimeEventKind::WorkspacePatch(_))
            ));
            assert_eq!(
                delivered
                    .iter()
                    .filter_map(|event| match &event.kind {
                        RuntimeEventKind::WorkspacePatch(patch) => {
                            Some(patch.revision().value())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                vec![1, 2]
            );
            let _ = std::fs::remove_dir_all(directory);
        }
    });
}

#[test]
fn workspace_adapter_withholds_failed_patch_then_publishes_ordered_recovery() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-patch-recovery-event-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let (sender, mut receiver) = runtime_event_channel();
        let session = SessionId::default().next();
        let first_events = RuntimeEventSender::new(
            sender.clone(),
            RuntimeIdentity {
                session,
                request: RequestId::new(1),
            },
            OperationContext::new(
                RuntimeOperation::OpenConversation,
                RuntimeTarget::Channel("C1".to_string()),
            ),
        );
        store
            .install_conversation_batch_failure_trigger_for("C1")
            .await
            .unwrap();
        assert!(workspace
            .apply_persisted_and_publish(
                Some(&store),
                &first_events,
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "C1".to_string(),
                    name: Some("first".to_string()),
                    ..Default::default()
                }),
            )
            .await
            .is_err());
        assert!(
            receiver.try_recv().is_err(),
            "a patch must not precede its failed store batch"
        );

        store
            .clear_conversation_batch_failure_trigger()
            .await
            .unwrap();
        let recovered_events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session,
                request: RequestId::new(2),
            },
            OperationContext::new(
                RuntimeOperation::OpenConversation,
                RuntimeTarget::Channel("C2".to_string()),
            ),
        );
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &recovered_events,
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "C2".to_string(),
                    name: Some("second".to_string()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();

        let first = receiver.recv().await.unwrap();
        let second = receiver.recv().await.unwrap();
        assert_eq!(first.meta.request, None);
        assert_eq!(second.meta.request, None);
        let revisions = [&first, &second]
            .into_iter()
            .map(|event| match &event.kind {
                RuntimeEventKind::WorkspacePatch(patch) => patch.revision().value(),
                other => panic!("expected a workspace patch, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(revisions, vec![1, 2]);
        assert_eq!(
            [&first, &second]
                .into_iter()
                .flat_map(|event| match &event.kind {
                    RuntimeEventKind::WorkspacePatch(patch) => {
                        conversation_patch_summary(patch.changes())
                    }
                    _ => Vec::new(),
                })
                .collect::<Vec<_>>(),
            vec![
                ("upsert", "C1".to_string(), Some("first".to_string())),
                ("upsert", "C2".to_string(), Some("second".to_string())),
            ]
        );
        assert!(receiver.try_recv().is_err());
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn conversation_refresh_publishes_exactly_one_patch_and_recovers_in_revision_order() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-conversation-refresh-recovery-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let (sender, mut receiver) = runtime_event_channel();
        let session = SessionId::default().next();
        let events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session,
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
        );
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::WebApi,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![
                        SlackConversation {
                            id: "C1".into(),
                            name: Some("one".into()),
                            ..Default::default()
                        },
                        SlackConversation {
                            id: "C2".into(),
                            name: Some("two".into()),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));
        assert!(receiver.try_recv().is_err());

        let first_base = workspace.revision();
        store
            .install_conversation_batch_failure_trigger_for("C1")
            .await
            .unwrap();
        assert!(workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationRefreshBatch(vec![SnapshotEnvelope::new(
                    first_base,
                    ConversationRefresh {
                        metadata: Some(SlackConversation {
                            id: "C1".into(),
                            name: Some("one refreshed".into()),
                            ..Default::default()
                        }),
                    },
                ),]),
            )
            .await
            .is_err());
        assert!(
            receiver.try_recv().is_err(),
            "a failed composite store batch must withhold its whole patch"
        );

        store
            .clear_conversation_batch_failure_trigger()
            .await
            .unwrap();
        let second_base = workspace.revision();
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationRefreshBatch(vec![SnapshotEnvelope::new(
                    second_base,
                    ConversationRefresh {
                        metadata: Some(SlackConversation {
                            id: "C2".into(),
                            name: Some("two refreshed".into()),
                            ..Default::default()
                        }),
                    },
                )]),
            )
            .await
            .unwrap();

        let first = receiver.recv().await.unwrap();
        let second = receiver.recv().await.unwrap();
        let patches = [&first, &second]
            .into_iter()
            .map(|event| match &event.kind {
                RuntimeEventKind::WorkspacePatch(patch) => patch,
                other => panic!("expected only typed workspace patches, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            patches
                .iter()
                .map(|patch| patch.revision().value())
                .collect::<Vec<_>>(),
            vec![
                first_base.successor().value(),
                second_base.successor().value()
            ]
        );
        assert_eq!(
            patches
                .iter()
                .map(|patch| patch.changes().len())
                .collect::<Vec<_>>(),
            vec![1, 1],
            "a metadata-only refresh carries exactly one change per patch"
        );
        assert!(receiver.try_recv().is_err());

        let stored = store.load_conversations().await.unwrap().unwrap();
        let one = stored
            .iter()
            .find(|conversation| conversation.id == "C1")
            .unwrap();
        assert_eq!(one.name.as_deref(), Some("one refreshed"));
        let two = stored
            .iter()
            .find(|conversation| conversation.id == "C2")
            .unwrap();
        assert_eq!(two.name.as_deref(), Some("two refreshed"));
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn prefetched_history_persists_the_coordinator_reconciliation_not_the_raw_page() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-prefetch-canonical-history-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
    let workspace = WorkspaceReducerAdapter::default();
    workspace.update_attention_context(WorkspaceAttentionContext {
        current_user_id: Some("U_SELF".into()),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace),
        );
        let original_edited = SlackMessage {
            ts: "1.0".into(),
            client_msg_id: Some("edited".into()),
            user: Some("U_OTHER".into()),
            text: Some("stale edit".into()),
            ..Default::default()
        };
        let original_deleted = SlackMessage {
            ts: "2.0".into(),
            client_msg_id: Some("deleted".into()),
            user: Some("U_OTHER".into()),
            text: Some("stale delete".into()),
            ..Default::default()
        };
        let original_moved = SlackMessage {
            ts: "3.0".into(),
            client_msg_id: Some("moved".into()),
            user: Some("U_OTHER".into()),
            text: Some("stale location".into()),
            ..Default::default()
        };
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::WebApi,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![SlackConversation {
                        id: "C1".into(),
                        name: Some("general".into()),
                        is_channel: Some(true),
                        ..Default::default()
                    }],
                    histories: HashMap::from([(
                        "C1".into(),
                        vec![
                            original_edited.clone(),
                            original_deleted.clone(),
                            original_moved.clone(),
                        ],
                    )]),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));
        let network_base = workspace.revision();

        let mutations = [
            (
                SlackMessage {
                    text: Some("authoritative edit".into()),
                    ..original_edited.clone()
                },
                MessageMutationKind::Changed,
            ),
            (original_deleted.clone(), MessageMutationKind::Deleted),
            (
                SlackMessage {
                    thread_ts: Some("9.0".into()),
                    text: Some("authoritative thread location".into()),
                    ..original_moved.clone()
                },
                MessageMutationKind::Changed,
            ),
        ];
        for (message, kind) in mutations {
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Realtime,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message,
                        kind,
                        origin: MutationOrigin::Realtime,
                    },
                )
                .await
                .unwrap();
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::WorkspacePatch(_)
            ));
        }

        let new_message = SlackMessage {
            ts: "4.0".into(),
            user: Some("U_OTHER".into()),
            text: Some("new from snapshot".into()),
            ..Default::default()
        };
        let reductions = publish_prefetched_history_snapshot(
            &events,
            &Some(store.clone()),
            &workspace,
            "C1",
            network_base,
            vec![
                original_edited,
                original_deleted,
                original_moved,
                new_message.clone(),
            ],
        )
        .await
        .unwrap();
        assert_eq!(reductions.len(), 1);
        let published = receiver.recv().await.unwrap();
        let RuntimeEventKind::WorkspacePatch(patch) = published.kind else {
            panic!("prefetched history must publish one typed patch");
        };
        assert!(matches!(
            patch.changes(),
            [WorkspaceChange::TimelineChanged { changes, .. }] if matches!(
                changes.as_slice(),
                [crate::workspace_pipeline::MessageChange::Upsert(message)]
                    if message.ts == new_message.ts
            )
        ));
        assert!(
            receiver.try_recv().is_err(),
            "migrated prefetch must not emit a duplicate legacy attention event"
        );

        let persisted = store.load_history("C1").await.unwrap().unwrap();
        assert_eq!(
            persisted
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["4.0", "1.0"]
        );
        assert_eq!(
            persisted
                .iter()
                .find(|message| message.ts == "1.0")
                .and_then(|message| message.text.as_deref()),
            Some("authoritative edit")
        );
        assert!(!persisted.iter().any(|message| message.ts == "2.0"));
        assert!(!persisted.iter().any(|message| message.ts == "3.0"));
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn cached_history_is_durable_already_and_fresh_history_publishes_fifo() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-interactive-history-recovery-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
    let workspace = WorkspaceReducerAdapter::default();
    workspace.update_attention_context(WorkspaceAttentionContext {
        current_user_id: Some("U_SELF".into()),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(
                RuntimeOperation::History,
                RuntimeTarget::Channel("C1".into()),
            ),
        );
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::WebApi,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![SlackConversation {
                        id: "C1".into(),
                        is_channel: Some(true),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));

        let cached = SlackMessage {
            ts: "1.0".into(),
            user: Some("U_OTHER".into()),
            text: Some("cached".into()),
            ..Default::default()
        };
        store
            .store_history("C1", std::slice::from_ref(&cached))
            .await
            .unwrap();
        let cached_reductions = publish_history_snapshot_with_completion(
            &events,
            &Some(store.clone()),
            &workspace,
            "C1",
            MutationOrigin::Cache,
            WorkspaceRevision::INITIAL,
            vec![cached.clone()],
            false,
            None,
            true,
            false,
            true,
        )
        .await
        .unwrap();
        assert!(
            cached_reductions
                .iter()
                .all(|reduction| reduction.store_batch().is_none()),
            "a cached page is already durable and must not write back to the store"
        );
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::HistoryLoadCompleted { cached: true, .. }
        ));
        let fresh_base = workspace.revision();
        let fresh = SlackMessage {
            ts: "2.0".into(),
            user: Some("U_OTHER".into()),
            text: Some("fresh".into()),
            ..Default::default()
        };
        let reductions = publish_history_snapshot_with_completion(
            &events,
            &Some(store.clone()),
            &workspace,
            "C1",
            MutationOrigin::WebApi,
            fresh_base,
            vec![fresh.clone(), cached.clone()],
            true,
            Some("older".into()),
            false,
            false,
            false,
        )
        .await
        .unwrap();

        assert_eq!(reductions.len(), 1);
        assert!(matches!(
            reductions[0].store_batch().unwrap().changes(),
            [StoreChange::HistoryDelta { .. }]
        ));
        let fresh_patch = receiver.recv().await.unwrap();
        let completion = receiver.recv().await.unwrap();
        assert!(matches!(
            fresh_patch.kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));
        let RuntimeEventKind::HistoryLoadCompleted {
            channel_id,
            has_more,
            next_cursor,
            append_older,
            cached: completion_cached,
        } = completion.kind
        else {
            panic!("fresh completion must follow its patch");
        };
        assert_eq!(channel_id, "C1");
        assert!(has_more);
        assert_eq!(next_cursor.as_deref(), Some("older"));
        assert!(!append_older);
        assert!(!completion_cached);
        assert!(receiver.try_recv().is_err());

        assert_eq!(
            store
                .load_history("C1")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| (message.ts.as_str(), message.body_text()))
                .collect::<Vec<_>>(),
            vec![("2.0", "fresh".into()), ("1.0", "cached".into())]
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn older_history_persists_atomically_and_completes_with_page_metadata() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-interactive-older-history-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
    let workspace = WorkspaceReducerAdapter::default();
    workspace.update_attention_context(WorkspaceAttentionContext {
        current_user_id: Some("U_SELF".into()),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(
                RuntimeOperation::OlderHistory,
                RuntimeTarget::Channel("C1".into()),
            ),
        );
        let initial = SlackMessage {
            ts: "3.0".into(),
            user: Some("U_OTHER".into()),
            text: Some("initial".into()),
            ..Default::default()
        };
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::WebApi,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![SlackConversation {
                        id: "C1".into(),
                        is_channel: Some(true),
                        ..Default::default()
                    }],
                    histories: HashMap::from([("C1".into(), vec![initial])]),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));

        let newer = SlackMessage {
            ts: "2.0".into(),
            user: Some("U_OTHER".into()),
            text: Some("newer page item".into()),
            ..Default::default()
        };
        let older = SlackMessage {
            ts: "1.0".into(),
            user: Some("U_OTHER".into()),
            text: Some("older page item".into()),
            ..Default::default()
        };
        let normal_reply = SlackMessage {
            ts: "1.5".into(),
            thread_ts: Some("3.0".into()),
            user: Some("U_OTHER".into()),
            text: Some("thread only".into()),
            ..Default::default()
        };
        let page = vec![newer.clone(), normal_reply, older.clone()];
        let base_revision = workspace.revision();
        store
            .install_history_batch_failure_trigger_for("C1")
            .await
            .unwrap();
        assert!(publish_history_snapshot_with_completion(
            &events,
            &Some(store.clone()),
            &workspace,
            "C1",
            MutationOrigin::WebApi,
            base_revision,
            page.clone(),
            true,
            Some("page-3".into()),
            false,
            true,
            false,
        )
        .await
        .is_err());
        assert!(
            receiver.try_recv().is_err(),
            "failed older history durability must withhold patch and completion"
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
            vec!["3.0"],
            "a failed history write must roll back inside its transaction"
        );
        let stored_conversation = store
            .load_conversations()
            .await
            .unwrap()
            .unwrap()
            .into_iter()
            .find(|conversation| conversation.id == "C1")
            .unwrap();
        assert_eq!(stored_conversation.id, "C1");
        store.clear_history_batch_failure_trigger().await.unwrap();

        let reductions = publish_history_snapshot_with_completion(
            &events,
            &Some(store.clone()),
            &workspace,
            "C1",
            MutationOrigin::WebApi,
            base_revision,
            page,
            true,
            Some("page-3".into()),
            false,
            true,
            false,
        )
        .await
        .unwrap();

        assert_eq!(reductions.len(), 1);
        assert!(matches!(
            reductions[0].store_batch().unwrap().changes(),
            [StoreChange::HistoryDelta { .. }]
        ));
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));
        let completion = receiver.recv().await.unwrap();
        let RuntimeEventKind::HistoryLoadCompleted {
            has_more,
            next_cursor,
            append_older,
            cached,
            ..
        } = completion.kind
        else {
            panic!("older history must complete after its durable patch");
        };
        assert!(has_more);
        assert_eq!(next_cursor.as_deref(), Some("page-3"));
        assert!(append_older);
        assert!(!cached);
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            store
                .load_history("C1")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["3.0", "2.0", "1.0"]
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn duplicate_cache_fresh_and_older_history_are_idempotent() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-interactive-history-idempotence-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
    let workspace = WorkspaceReducerAdapter::default();
    workspace.update_attention_context(WorkspaceAttentionContext {
        current_user_id: Some("U_SELF".into()),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(
                RuntimeOperation::History,
                RuntimeTarget::Channel("C1".into()),
            ),
        );
        let message = SlackMessage {
            ts: "1.0".into(),
            user: Some("U_OTHER".into()),
            text: Some("one".into()),
            ..Default::default()
        };
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::WebApi,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![SlackConversation {
                        id: "C1".into(),
                        is_channel: Some(true),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));
        store
            .store_history("C1", std::slice::from_ref(&message))
            .await
            .unwrap();
        let workspace_store = Some(store.clone());

        let first = publish_history_snapshot_with_completion(
            &events,
            &workspace_store,
            &workspace,
            "C1",
            MutationOrigin::Cache,
            WorkspaceRevision::INITIAL,
            vec![message.clone()],
            false,
            None,
            true,
            false,
            true,
        )
        .await
        .unwrap();
        assert_eq!(first.len(), 1);
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::HistoryLoadCompleted { cached: true, .. }
        ));
        let revision = workspace.revision();

        for (origin, append_older, cached) in [
            (MutationOrigin::Cache, false, true),
            (MutationOrigin::WebApi, false, false),
            (MutationOrigin::WebApi, true, false),
        ] {
            let duplicate = publish_history_snapshot_with_completion(
                &events,
                &workspace_store,
                &workspace,
                "C1",
                origin,
                revision,
                vec![message.clone()],
                false,
                None,
                !append_older,
                append_older,
                cached,
            )
            .await
            .unwrap();
            assert!(duplicate.is_empty());
            assert_eq!(workspace.revision(), revision);
            assert!(matches!(
                receiver.recv().await.unwrap().kind,
                RuntimeEventKind::HistoryLoadCompleted {
                    append_older: actual_append,
                    cached: actual_cached,
                    ..
                } if actual_append == append_older && actual_cached == cached
            ));
            assert!(receiver.try_recv().is_err());
        }

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
        let stored_conversation = store
            .load_conversations()
            .await
            .unwrap()
            .unwrap()
            .into_iter()
            .find(|conversation| conversation.id == "C1")
            .unwrap();
        assert_eq!(stored_conversation.id, "C1");
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn fresh_history_after_cache_preserves_concurrent_realtime_state_everywhere() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-interactive-history-concurrency-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(
                RuntimeOperation::History,
                RuntimeTarget::Channel("C1".into()),
            ),
        );
        let original_edit = SlackMessage {
            ts: "1.0".into(),
            client_msg_id: Some("edit".into()),
            text: Some("stale edit".into()),
            ..Default::default()
        };
        let original_deleted = SlackMessage {
            ts: "2.0".into(),
            client_msg_id: Some("deleted".into()),
            text: Some("stale delete".into()),
            ..Default::default()
        };
        let original_moved = SlackMessage {
            ts: "3.0".into(),
            client_msg_id: Some("moved".into()),
            text: Some("stale location".into()),
            ..Default::default()
        };
        let cached = vec![
            original_edit.clone(),
            original_deleted.clone(),
            original_moved.clone(),
        ];
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::WebApi,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![SlackConversation {
                        id: "C1".into(),
                        is_channel: Some(true),
                        ..Default::default()
                    }],
                    histories: HashMap::from([("C1".into(), cached.clone())]),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        let RuntimeEventKind::WorkspacePatch(bootstrap) = receiver.recv().await.unwrap().kind
        else {
            panic!("workspace bootstrap patch was not published");
        };
        let ui_session = crate::workspace_state::WorkspaceSessionState::default();
        ui_session.apply_workspace_patch(&bootstrap).unwrap();
        ui_session.view.borrow_mut().select_conversation("C1");

        publish_history_snapshot_with_completion(
            &events,
            &Some(store.clone()),
            &workspace,
            "C1",
            MutationOrigin::Cache,
            WorkspaceRevision::INITIAL,
            cached,
            false,
            None,
            true,
            false,
            true,
        )
        .await
        .unwrap();
        let cached_completion = receiver.recv().await.unwrap();
        let RuntimeEventKind::HistoryLoadCompleted { cached: true, .. } = cached_completion.kind
        else {
            panic!("cache hydration must complete before the network base is captured");
        };
        let network_base = workspace.revision();

        let authoritative_edit = SlackMessage {
            text: Some("authoritative edit".into()),
            ..original_edit.clone()
        };
        let moved_to_thread = SlackMessage {
            thread_ts: Some("9.0".into()),
            text: Some("authoritative thread location".into()),
            ..original_moved.clone()
        };
        let concurrent_post = SlackMessage {
            ts: "4.0".into(),
            text: Some("concurrent post".into()),
            ..Default::default()
        };
        let mutations = [
            (authoritative_edit.clone(), MessageMutationKind::Changed),
            (original_deleted.clone(), MessageMutationKind::Deleted),
            (moved_to_thread.clone(), MessageMutationKind::Changed),
            (concurrent_post.clone(), MessageMutationKind::Posted),
        ];
        for (message, kind) in mutations {
            workspace
                .apply_persisted_and_publish(
                    Some(&store),
                    &events,
                    MutationOrigin::Realtime,
                    WorkspaceMutation::MessageChanged {
                        channel_id: "C1".into(),
                        message: message.clone(),
                        kind,
                        origin: MutationOrigin::Realtime,
                    },
                )
                .await
                .unwrap();
            let RuntimeEventKind::WorkspacePatch(patch) = receiver.recv().await.unwrap().kind
            else {
                panic!("realtime message patch was not published");
            };
            ui_session.apply_workspace_patch(&patch).unwrap();
        }

        let fresh = SlackMessage {
            ts: "5.0".into(),
            text: Some("fresh page item".into()),
            ..Default::default()
        };
        publish_history_snapshot_with_completion(
            &events,
            &Some(store.clone()),
            &workspace,
            "C1",
            MutationOrigin::WebApi,
            network_base,
            vec![
                original_edit,
                original_deleted,
                original_moved,
                fresh.clone(),
            ],
            false,
            None,
            true,
            false,
            false,
        )
        .await
        .unwrap();
        let RuntimeEventKind::WorkspacePatch(patch) = receiver.recv().await.unwrap().kind else {
            panic!("fresh history patch was not published");
        };
        ui_session.apply_workspace_patch(&patch).unwrap();
        let completion = receiver.recv().await.unwrap();
        let RuntimeEventKind::HistoryLoadCompleted {
            append_older: false,
            cached: false,
            ..
        } = completion.kind
        else {
            panic!("fresh history must complete after its canonical patch");
        };
        let projected_messages = ui_session
            .view
            .borrow()
            .channel_messages("C1")
            .iter()
            .map(|message| (message.ts.clone(), message.body_text()))
            .collect::<Vec<_>>();
        assert_eq!(
            projected_messages,
            vec![
                ("5.0".into(), "fresh page item".into()),
                ("4.0".into(), "concurrent post".into()),
                ("1.0".into(), "authoritative edit".into()),
            ]
        );
        assert_eq!(
            workspace
                .history("C1")
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["1.0", "4.0", "5.0"]
        );
        assert_eq!(
            store
                .load_history("C1")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| (message.ts.as_str(), message.body_text()))
                .collect::<Vec<_>>(),
            vec![
                ("5.0", "fresh page item".into()),
                ("4.0", "concurrent post".into()),
                ("1.0", "authoritative edit".into()),
            ]
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_adapter_serializes_patch_and_completion_publication() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-patch-publication-order-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let (sender, mut receiver) = runtime_event_channel();
        let session = SessionId::default().next();
        let (publication_started, publication_reached) = std::sync::mpsc::channel();
        let (release_publication, release) = std::sync::mpsc::channel();
        let mut first_events = RuntimeEventSender::new(
            sender.clone(),
            RuntimeIdentity {
                session,
                request: RequestId::new(1),
            },
            OperationContext::new(
                RuntimeOperation::OpenConversation,
                RuntimeTarget::Channel("C1".to_string()),
            ),
        );
        first_events.workspace_patch_send_gate = Some(Arc::new(TestWorkspacePatchSendGate {
            started: publication_started,
            release: Mutex::new(release),
        }));
        let second_events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session,
                request: RequestId::new(2),
            },
            OperationContext::new(
                RuntimeOperation::OpenConversation,
                RuntimeTarget::Channel("C2".to_string()),
            ),
        );

        let first_workspace = workspace.clone();
        let first_store = store.clone();
        let first = tokio::spawn(async move {
            first_workspace
                .apply_persisted_and_publish_with_completion(
                    Some(&first_store),
                    &first_events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        id: "C1".to_string(),
                        ..Default::default()
                    }),
                    RuntimeEventKind::ConversationOpenCompleted {
                        channel_id: "C1".to_string(),
                    },
                )
                .await
        });
        publication_reached
            .recv_timeout(Duration::from_secs(5))
            .expect("first patch publication did not reach the test gate");

        let (second_started, second_reached) = tokio::sync::oneshot::channel();
        let second_workspace = workspace.clone();
        let second_store = store.clone();
        let second = tokio::spawn(async move {
            let _ = second_started.send(());
            second_workspace
                .apply_persisted_and_publish_with_completion(
                    Some(&second_store),
                    &second_events,
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(SlackConversation {
                        id: "C2".to_string(),
                        ..Default::default()
                    }),
                    RuntimeEventKind::ConversationOpenCompleted {
                        channel_id: "C2".to_string(),
                    },
                )
                .await
        });
        second_reached.await.unwrap();
        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        assert!(
            receiver.try_recv().is_err(),
            "the later patch must remain behind the first publication section"
        );

        release_publication.send(()).unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        let mut sequence = Vec::new();
        for _ in 0..4 {
            sequence.push(match receiver.recv().await.unwrap().kind {
                RuntimeEventKind::WorkspacePatch(patch) => {
                    format!("patch:{}", patch.revision().value())
                }
                RuntimeEventKind::ConversationOpenCompleted { channel_id } => {
                    format!("opened:{channel_id}")
                }
                other => panic!("unexpected ordered event {other:?}"),
            });
        }
        assert_eq!(
            sequence,
            vec!["patch:1", "opened:C1", "patch:2", "opened:C2"]
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_adapter_retries_failed_batch_before_accepting_the_next_mutation() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-batch-retry-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let first = SlackConversation {
            id: "C1".into(),
            name: Some("first".into()),
            ..Default::default()
        };
        store
            .install_conversation_batch_failure_trigger()
            .await
            .unwrap();
        assert!(workspace
            .apply_persisted(
                Some(&store),
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(first.clone()),
            )
            .await
            .is_err());
        assert!(store.load_conversations().await.unwrap().is_none());
        store
            .clear_conversation_batch_failure_trigger()
            .await
            .unwrap();

        let second = SlackConversation {
            id: "C2".into(),
            name: Some("second".into()),
            ..Default::default()
        };
        let (retry, next) = futures_util::future::join(
            workspace.apply_persisted(
                Some(&store),
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(first),
            ),
            workspace.apply_persisted(
                Some(&store),
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(second),
            ),
        )
        .await;
        let mut reductions = retry.unwrap();
        reductions.extend(next.unwrap());
        assert_eq!(
            reductions
                .iter()
                .map(|reduction| reduction.patch().revision().value())
                .collect::<Vec<_>>(),
            vec![1, 2],
            "the recovered patch must remain ahead of the next mutation patch"
        );

        let stored = store.load_conversations().await.unwrap().unwrap();
        assert_eq!(
            stored
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["C1", "C2"])
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_adapter_retains_an_admitted_reduction_when_persistence_is_cancelled() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-batch-cancel-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
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

        let first = SlackConversation {
            id: "C1".into(),
            name: Some("first".into()),
            ..Default::default()
        };
        let cancelled_workspace = workspace.clone();
        let cancelled_store = store.clone();
        let cancelled = tokio::spawn(async move {
            cancelled_workspace
                .apply_persisted(
                    Some(&cancelled_store),
                    MutationOrigin::WebApi,
                    WorkspaceMutation::ConversationUpsert(first),
                )
                .await
        });
        while workspace.revision() == WorkspaceRevision::INITIAL {
            tokio::task::yield_now().await;
        }
        cancelled.abort();
        assert!(cancelled.await.unwrap_err().is_cancelled());
        release_writer.send(()).unwrap();
        blocker.await.unwrap();

        let reductions = workspace
            .apply_persisted(
                Some(&store),
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "C2".into(),
                    name: Some("second".into()),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            reductions
                .iter()
                .map(|reduction| reduction.patch().revision().value())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        let stored = store.load_conversations().await.unwrap().unwrap();
        assert_eq!(
            stored
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<HashSet<_>>(),
            HashSet::from(["C1", "C2"])
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_adapter_retains_recovered_patch_when_the_following_batch_fails() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-batch-consecutive-failure-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        store
            .install_conversation_batch_failure_trigger_for("C1")
            .await
            .unwrap();
        assert!(workspace
            .apply_persisted(
                Some(&store),
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "C1".into(),
                    ..Default::default()
                }),
            )
            .await
            .is_err());

        store
            .clear_conversation_batch_failure_trigger()
            .await
            .unwrap();
        store
            .install_conversation_batch_failure_trigger_for("C2")
            .await
            .unwrap();
        assert!(workspace
            .apply_persisted(
                Some(&store),
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "C2".into(),
                    ..Default::default()
                }),
            )
            .await
            .is_err());

        store
            .clear_conversation_batch_failure_trigger()
            .await
            .unwrap();
        let reductions = workspace
            .apply_persisted(
                Some(&store),
                MutationOrigin::WebApi,
                WorkspaceMutation::ConversationUpsert(SlackConversation {
                    id: "C2".into(),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            reductions
                .iter()
                .map(|reduction| reduction.patch().revision().value())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_adapter_journals_an_opposite_star_without_premature_completion() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-star-retry-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (sender, mut receiver) = runtime_event_channel();
    let events = RuntimeEventSender {
        sender,
        session: SessionId::default().next(),
        request: None,
        fallback: OperationContext::new(
            RuntimeOperation::ConversationStar,
            RuntimeTarget::Channel("C1".to_string()),
        ),
        workspace_patch_send_gate: None,
    };

    runtime.block_on(async {
        let initial = SlackConversation {
            id: "C1".into(),
            is_channel: Some(true),
            is_starred: Some(false),
            ..Default::default()
        };
        workspace
            .apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![initial.clone()],
                    ..Default::default()
                }),
            )
            .unwrap();
        store
            .store_conversations(std::slice::from_ref(&initial))
            .await
            .unwrap();
        store
            .install_conversation_batch_failure_trigger_for("C1")
            .await
            .unwrap();

        workspace
            .apply_persisted(
                Some(&store),
                MutationOrigin::Local,
                WorkspaceMutation::ConversationStarChanged {
                    channel_id: "C1".into(),
                    starred: true,
                },
            )
            .await
            .unwrap_err();
        assert!(workspace
            .coordinator
            .lock()
            .unwrap()
            .conversation("C1")
            .unwrap()
            .is_starred());
        assert!(!store.load_conversations().await.unwrap().unwrap()[0].is_starred());

        workspace
            .apply_persisted(
                Some(&store),
                MutationOrigin::Local,
                WorkspaceMutation::ConversationStarChanged {
                    channel_id: "C1".into(),
                    starred: false,
                },
            )
            .await
            .unwrap_err();
        assert!(!workspace
            .coordinator
            .lock()
            .unwrap()
            .conversation("C1")
            .unwrap()
            .is_starred());
        assert!(!store.load_conversations().await.unwrap().unwrap()[0].is_starred());
        assert!(
            receiver.try_recv().is_err(),
            "failed star batches must not publish completion events"
        );

        store
            .clear_conversation_batch_failure_trigger()
            .await
            .unwrap();
        let reductions = workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::Local,
                WorkspaceMutation::ConversationStarChanged {
                    channel_id: "C1".into(),
                    starred: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            reductions
                .iter()
                .map(|reduction| reduction.patch().revision().value())
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(
            reductions
                .iter()
                .flat_map(|reduction| reduction.patch().changes())
                .filter_map(|change| match change {
                    WorkspaceChange::ConversationUpsert(conversation) => {
                        conversation.is_starred
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![true, false]
        );
        let first = receiver.recv().await.unwrap();
        let second = receiver.recv().await.unwrap();
        assert_eq!(
            [&first, &second]
                .into_iter()
                .map(|event| match &event.kind {
                    RuntimeEventKind::WorkspacePatch(patch) => patch.revision().value(),
                    other => panic!("expected a workspace patch, got {other:?}"),
                })
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(receiver.try_recv().is_err());
        assert!(!store.load_conversations().await.unwrap().unwrap()[0].is_starred());
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_adapter_repopulates_membership_after_cache_recovery() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-membership-recovery-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let conversation = SlackConversation {
            id: "C1".into(),
            name: Some("general".into()),
            ..Default::default()
        };
        workspace
            .apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![conversation.clone()],
                    ..Default::default()
                }),
            )
            .unwrap();
        store
            .store_conversations(std::slice::from_ref(&conversation))
            .await
            .unwrap();
        store.corrupt_conversation_payload("C1").await.unwrap();

        let generation = store.recovery_generation();
        let base_revision = workspace.revision();
        assert!(!store.conversation_cache_needs_repair());
        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: Some(RequestId::new(11)),
            fallback: OperationContext::new(
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
            ),
            workspace_patch_send_gate: None,
        };

        assert_eq!(
            apply_conversation_membership_snapshot(
                &events,
                Some(&store),
                &workspace,
                base_revision,
                vec![conversation.clone()],
                None,
            )
            .await
            .unwrap(),
            vec![conversation.clone()]
        );
        assert!(store.recovery_generation() > generation);
        assert!(!store.conversation_cache_needs_repair());
        assert_eq!(workspace.revision(), base_revision);
        let completion = receiver.recv().await.unwrap();
        assert_eq!(completion.meta.request, Some(RequestId::new(11)));
        assert!(matches!(
            completion.kind,
            RuntimeEventKind::ConversationsSynchronized
        ));
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            store.load_conversations().await.unwrap().unwrap(),
            vec![conversation]
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn workspace_adapter_orders_cache_web_local_and_realtime_mutations() {
    let workspace = WorkspaceReducerAdapter::default();
    let conversation = SlackConversation {
        id: "C1".into(),
        name: Some("general".into()),
        is_channel: Some(true),
        ..Default::default()
    };
    assert!(workspace
        .apply(
            MutationOrigin::Cache,
            WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                conversations: vec![conversation.clone()],
                ..Default::default()
            }),
        )
        .is_some());

    let base_revision = workspace.revision();
    let mut renamed = conversation;
    renamed.name = Some("announcements".into());
    assert!(workspace
        .apply(
            MutationOrigin::WebApi,
            WorkspaceMutation::MembershipSnapshot(SnapshotEnvelope::new(
                base_revision,
                ConversationMembershipSnapshot {
                    conversations: vec![renamed],
                    starred_ids: None,
                },
            )),
        )
        .is_some());

    let posted = SlackMessage {
        ts: "1.000".into(),
        text: Some("sent".into()),
        ..Default::default()
    };
    assert!(workspace
        .apply(
            MutationOrigin::Local,
            WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: posted.clone(),
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Local,
            },
        )
        .is_some());

    let mut edited = posted;
    edited.text = Some("edited".into());
    assert!(workspace
        .apply(
            MutationOrigin::Realtime,
            WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: edited.clone(),
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            },
        )
        .is_some());
    assert_eq!(workspace.revision().value(), 4);
    assert!(workspace
        .apply(
            MutationOrigin::Realtime,
            WorkspaceMutation::MessageChanged {
                channel_id: "C1".into(),
                message: edited,
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Realtime,
            },
        )
        .is_none());
    assert_eq!(workspace.revision().value(), 4);
}

#[test]
fn realtime_socket_events_enter_the_workspace_adapter_once() {
    let workspace = WorkspaceReducerAdapter::default();
    assert!(apply_realtime_workspace_event(
        &workspace,
        &SocketModeEvent::Message(Box::new(socket_mode::SocketModeMessageEvent {
            channel_id: "C1".into(),
            message: SlackMessage {
                ts: "1.000".into(),
                text: Some("hello".into()),
                ..Default::default()
            },
            kind: SocketModeMessageKind::Posted,
        })),
    )
    .is_some());
    assert!(apply_realtime_workspace_event(
        &workspace,
        &SocketModeEvent::UserChanged(Box::new(SlackUser {
            id: Some("U1".into()),
            name: Some("person".into()),
            ..Default::default()
        })),
    )
    .is_none());
    assert_eq!(workspace.revision().value(), 2);

    assert!(apply_realtime_workspace_event(
        &workspace,
        &SocketModeEvent::UserHuddleChanged(Box::new(SlackUser {
            id: Some("U1".into()),
            profile: Some(crate::models::SlackUserProfile {
                huddle_state_call_id: Some("R1".into()),
                ..Default::default()
            }),
            ..Default::default()
        })),
    )
    .is_none());
    assert_eq!(workspace.revision().value(), 3);

    assert!(apply_realtime_workspace_event(
        &workspace,
        &SocketModeEvent::Reaction(socket_mode::SocketModeReactionEvent {
            channel_id: "C1".into(),
            ts: "1.000".into(),
            name: "wave".into(),
            user_id: "U1".into(),
            added: true,
        }),
    )
    .is_none());
    assert_eq!(workspace.revision().value(), 4);
}

#[test]
fn attention_metrics_count_committed_effects_but_not_persistence_previews() {
    let workspace = WorkspaceReducerAdapter::default();
    workspace.update_attention_context(WorkspaceAttentionContext {
        current_user_id: Some("U_SELF".into()),
    });
    workspace.apply(
        MutationOrigin::Cache,
        WorkspaceMutation::ConversationUpsert(SlackConversation {
            id: "D1".into(),
            is_im: Some(true),
            ..Default::default()
        }),
    );
    let message = SlackMessage {
        ts: "1.000".into(),
        user: Some("U_OTHER".into()),
        text: Some("hello".into()),
        ..Default::default()
    };

    assert!(workspace
        .preview_message_attention(
            "D1",
            &message,
            MessageMutationKind::Posted,
            MutationOrigin::Realtime,
        )
        .is_some());
    assert_eq!(
        workspace.attention_metrics_snapshot().committed_decisions,
        0
    );

    assert!(workspace
        .apply(
            MutationOrigin::Realtime,
            WorkspaceMutation::MessageChanged {
                channel_id: "D1".into(),
                message,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            },
        )
        .is_some());
    let metrics = workspace.attention_metrics_snapshot();
    assert_eq!(metrics.committed_decisions, 1);
    assert_eq!(metrics.notification_candidates, 1);
    assert_eq!(metrics.reason_count(AttentionReason::DirectMessage), 1);
    assert_eq!(metrics.origin_count(MutationOrigin::Realtime), 1);
    assert_eq!(metrics.delivery_count(DeliveryState::Fresh), 1);
}

#[test]
fn preference_change_during_persistence_suppresses_the_stale_notification_claim() {
    let workspace = WorkspaceReducerAdapter::default();
    workspace.update_attention_context(WorkspaceAttentionContext {
        current_user_id: Some("U_SELF".into()),
    });
    workspace.apply(
        MutationOrigin::Cache,
        WorkspaceMutation::ConversationUpsert(SlackConversation {
            id: "D1".into(),
            is_channel: Some(false),
            is_im: Some(true),
            ..Default::default()
        }),
    );
    let message = SlackMessage {
        ts: "1.000".into(),
        user: Some("U_OTHER".into()),
        text: Some("hello".into()),
        ..Default::default()
    };
    let stale_preview = workspace
        .preview_message_attention(
            "D1",
            &message,
            MessageMutationKind::Posted,
            MutationOrigin::Realtime,
        )
        .expect("direct message should have an attention preview");
    assert!(stale_preview.decision.send_notification);

    workspace.update_attention_preferences(AttentionPreferences {
        direct_messages: false,
        ..AttentionPreferences::default()
    });
    let reduction = workspace
        .apply(
            MutationOrigin::Realtime,
            WorkspaceMutation::MessageChanged {
                channel_id: "D1".into(),
                message,
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            },
        )
        .expect("message should be applied after persistence");
    let live_attention = reduction.effects().iter().find_map(|effect| match effect {
        WorkspaceEffect::MessageAttention(effect) => Some(effect),
        WorkspaceEffect::ConversationReadMarked { .. } => None,
    });

    assert!(
        live_attention.is_some_and(|effect| !effect.decision.send_notification),
        "the live decision must honour the preference change, not the stale preview"
    );
    assert!(claimed_notification_candidate(true, live_attention).is_none());
}

#[test]
fn browser_session_realtime_takes_precedence_without_loading_an_app_token() {
    let app_token_loaded = Cell::new(false);
    let browser = socket_mode::SocketModeCredentials::BrowserSession {
        xoxc_token: "xoxc-browser".into(),
        xoxd_token: "xoxd-browser".into(),
        user_agent: None,
    };

    let selected = select_realtime_credentials(Some(browser.clone()), || {
        app_token_loaded.set(true);
        Ok(Some("xapp-unused".into()))
    })
    .expect("browser realtime selection should succeed");

    assert_eq!(selected, Some(browser));
    assert!(!app_token_loaded.get());
}

#[test]
fn app_token_realtime_remains_the_oauth_fallback() {
    let selected = select_realtime_credentials(None, || Ok(Some("xapp-fallback".into())))
        .expect("app-token realtime selection should succeed");

    assert_eq!(
        selected,
        Some(socket_mode::SocketModeCredentials::AppToken(
            "xapp-fallback".into()
        ))
    );
    assert_eq!(
        select_realtime_credentials(None, || Ok(None))
            .expect("missing realtime credentials are valid"),
        None
    );
}

#[derive(Clone)]
struct TraceWriter(Arc<Mutex<Vec<u8>>>);

// Tracing callsite interest is process-wide, so local subscriber captures
// must not rebuild it concurrently.
static TRACE_SUBSCRIBER_TEST_LOCK: Mutex<()> = Mutex::new(());
const TRACE_TEST_CHILD_ENV: &str = "CONDUIT_TRACE_TEST_CHILD";

fn run_trace_test_in_isolated_process(test_name: &str) -> bool {
    if std::env::var_os(TRACE_TEST_CHILD_ENV).is_some() {
        return false;
    }
    let output = std::process::Command::new(
        std::env::current_exe().expect("test executable should be available"),
    )
    .arg("--exact")
    .arg(test_name)
    .arg("--test-threads=1")
    .env_clear()
    .env("LANG", "C.UTF-8")
    .env(TRACE_TEST_CHILD_ENV, "1")
    .output()
    .expect("isolated trace test should start");
    assert!(
        output.status.success(),
        "isolated trace test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    true
}

impl Write for TraceWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("trace output lock poisoned")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn runtime_trace_output_contains_correlation_fields_and_redacts_signed_urls() {
    if run_trace_test_in_isolated_process(
        "runtime::tests::runtime_trace_output_contains_correlation_fields_and_redacts_signed_urls",
    ) {
        return;
    }
    let _trace_guard = TRACE_SUBSCRIBER_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let output = Arc::new(Mutex::new(Vec::new()));
    let writer = TraceWriter(Arc::clone(&output));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    let identity = RuntimeIdentity {
        session: SessionId::default().next(),
        request: RequestId::new(42),
    };
    let command = RuntimeCommand::LoadMedia {
        url: "https://viewer:password@files.slack.com/file?token=signed-secret#preview".to_string(),
        name: "private filename".to_string(),
    };

    tracing::subscriber::with_default(subscriber, || {
        let span = RuntimeTraceFields::for_command(identity, &command).span();
        let _entered = span.enter();
        tracing::debug!(target: "conduit::runtime", event = "scheduled");
    });

    let output = String::from_utf8(output.lock().expect("trace output lock poisoned").clone())
        .expect("trace output should be UTF-8");
    assert!(output.contains("runtime.command"));
    assert!(output.contains("SessionId(1)"));
    assert!(output.contains("RequestId(42)"));
    assert!(output.contains("operation=Media"));
    assert!(output.contains("media:https://files.slack.com/file"));
    assert!(output.contains("admission=Coalescible"));
    assert!(output.contains("replacement_key=Some(Media(OpaqueAdmissionTarget))"));
    for secret in [
        "viewer",
        "password",
        "signed-secret",
        "preview",
        "private filename",
    ] {
        assert!(
            !output.contains(secret),
            "trace leaked confidential content"
        );
    }
}

#[test]
fn attention_traces_contain_only_stable_categories_and_counters() {
    if run_trace_test_in_isolated_process(
        "runtime::tests::attention_traces_contain_only_stable_categories_and_counters",
    ) {
        return;
    }
    let _trace_guard = TRACE_SUBSCRIBER_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let output = Arc::new(Mutex::new(Vec::new()));
    let writer = TraceWriter(Arc::clone(&output));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let workspace = WorkspaceReducerAdapter::default();
    workspace.update_attention_context(WorkspaceAttentionContext {
        current_user_id: Some("U_TRACE_SECRET".into()),
    });
    workspace.update_attention_preferences(AttentionPreferences {
        names_and_aliases: vec!["ZirconAliasCanary".into()],
        keywords: vec!["TopSecretKeywordCanary".into()],
        ..AttentionPreferences::default()
    });
    workspace.apply(
        MutationOrigin::Cache,
        WorkspaceMutation::ConversationUpsert(SlackConversation {
            id: "C_TRACE_SECRET".into(),
            name: Some("PrivateConversationCanary".into()),
            ..Default::default()
        }),
    );

    tracing::subscriber::with_default(subscriber, || {
        workspace.apply(
            MutationOrigin::Realtime,
            WorkspaceMutation::MessageChanged {
                channel_id: "C_TRACE_SECRET".into(),
                message: SlackMessage {
                    ts: "1.000".into(),
                    user: Some("U_OTHER_TRACE_SECRET".into()),
                    text: Some("<@U_TRACE_SECRET> ZirconAliasCanary TopSecretKeywordCanary".into()),
                    ..Default::default()
                },
                kind: MessageMutationKind::Posted,
                origin: MutationOrigin::Realtime,
            },
        );
        workspace.record_attention_persistence(AttentionPersistenceOutcome::AlreadyObserved, false);
        let metrics = workspace.attention_metrics_handle();
        metrics.record_queue_send(|| Ok::<_, ()>(())).unwrap();
        metrics.dequeue_queue_slot();
        workspace.trace_attention_metrics_snapshot();
    });

    let output = String::from_utf8(output.lock().expect("trace output lock poisoned").clone())
        .expect("trace output should be UTF-8");
    for expected in [
        "attention_decision",
        "attention_persistence",
        "attention_queue_high_water",
        "attention_metrics_snapshot",
        "outcome=\"already_observed\"",
        "origin=\"realtime\"",
        "delivery=\"fresh\"",
        "direct_mention",
        "name_or_alias",
        "keyword_or_phrase",
    ] {
        assert!(
            output.contains(expected),
            "trace omitted expected stable trace category"
        );
    }
    for private in [
        "U_TRACE_SECRET",
        "U_OTHER_TRACE_SECRET",
        "C_TRACE_SECRET",
        "PrivateConversationCanary",
        "ZirconAliasCanary",
        "TopSecretKeywordCanary",
    ] {
        assert!(
            !output.contains(private),
            "attention trace leaked private input"
        );
    }
}

#[test]
fn runtime_trace_fields_include_identity_and_context_but_not_payloads() {
    let identity = RuntimeIdentity {
        session: SessionId::default().next(),
        request: RequestId::new(42),
    };
    let command = RuntimeCommand::PostMessage {
        channel_id: "C123".to_string(),
        text: "do not trace this message".to_string(),
        blocks_json: Some("do not trace these blocks".to_string()),
        attachments_json: None,
        thread_ts: None,
    };

    let fields = RuntimeTraceFields::for_command(identity, &command);

    assert_eq!(fields.session, identity.session);
    assert_eq!(fields.request, identity.request);
    assert_eq!(fields.operation, RuntimeOperation::PostMessage);
    assert_eq!(fields.target, "message:C123:main");
    assert!(!format!("{fields:?}").contains("do not trace this message"));
    assert!(!format!("{fields:?}").contains("do not trace these blocks"));

    let update = RuntimeCommand::UpdateMessage {
        channel_id: "C123".to_string(),
        original: Box::new(SlackMessage {
            ts: "1.0".to_string(),
            ..SlackMessage::default()
        }),
        text: "do not trace this edit".to_string(),
        blocks_json: Some("do not trace edited blocks".to_string()),
    };
    let fields = RuntimeTraceFields::for_command(identity, &update);
    assert_eq!(fields.operation, RuntimeOperation::UpdateMessage);
    assert_eq!(fields.target, "exact-message:C123:1.0");
    assert!(!format!("{fields:?}").contains("do not trace this edit"));
    assert!(!format!("{fields:?}").contains("do not trace edited blocks"));

    let slash = RuntimeCommand::ExecuteSlashCommand {
        channel_id: "C123".to_string(),
        command: "/giphy".to_string(),
        text: "do not trace slash argument".to_string(),
        thread_ts: Some("123.456".to_string()),
    };
    let slash_fields = RuntimeTraceFields::for_command(identity, &slash);
    assert_eq!(slash_fields.session, identity.session);
    assert_eq!(slash_fields.request, identity.request);
    assert_eq!(
        slash_fields.operation,
        RuntimeOperation::ExecuteSlashCommand
    );
    assert_eq!(slash_fields.target, "message:C123:123.456");
    assert!(!format!("{slash_fields:?}").contains("do not trace slash argument"));
}

#[test]
fn execute_slash_command_admits_as_durable_action_and_targets_message_context() {
    let command = RuntimeCommand::ExecuteSlashCommand {
        channel_id: "C123".to_string(),
        command: "/giphy".to_string(),
        text: "cats".to_string(),
        thread_ts: None,
    };
    let descriptor = command.descriptor();
    assert_eq!(
        descriptor.context.operation,
        RuntimeOperation::ExecuteSlashCommand
    );
    assert_eq!(
        descriptor.context.target,
        RuntimeTarget::Message {
            channel_id: "C123".to_string(),
            thread_ts: None,
        }
    );
    assert_eq!(descriptor.lane, RuntimeTaskLane::Interactive);
    assert_eq!(
        descriptor.admission.kind,
        RuntimeAdmissionKind::DurableAction
    );
}

#[test]
fn updated_content_is_atomically_merged_with_current_workspace_metadata() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-local-message-update-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let freshest = SlackMessage {
        user: Some("U1".into()),
        text: Some("old".into()),
        ts: "1.0".into(),
        reply_count: Some(4),
        reactions: Some(vec![crate::models::SlackReaction {
            name: Some("thumbsup".into()),
            count: Some(2),
            users: Some(vec!["U1".into(), "U2".into()]),
        }]),
        ..SlackMessage::default()
    };
    let mut updated = SlackMessage {
        user: Some("U1".into()),
        text: Some("edited".into()),
        ts: "1.0".into(),
        edited: Some(crate::models::SlackMessageEdit {
            user: Some("U1".into()),
            ts: None,
        }),
        ..SlackMessage::default()
    };
    updated.refresh_canonical_content();
    let original = SlackMessage {
        user: Some("U1".into()),
        text: Some("old".into()),
        ts: "1.0".into(),
        ..SlackMessage::default()
    };
    let workspace = WorkspaceReducerAdapter::default();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let merged = runtime.block_on(async {
        let (sender, _receiver) = runtime_event_channel();
        let events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::UpdateMessage, RuntimeTarget::Workspace),
        );
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::Local,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: freshest.clone(),
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Local,
                },
            )
            .await
            .unwrap();
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::Local,
                WorkspaceMutation::MessageUpdated {
                    channel_id: "C1".into(),
                    original: Box::new(original),
                    updated,
                },
            )
            .await
            .unwrap();
        workspace.message("C1", "1.0").unwrap()
    });

    assert_eq!(merged.body_text(), "edited");
    assert_eq!(merged.reactions, freshest.reactions);
    assert_eq!(merged.reply_count, Some(4));
    assert!(merged.edited.is_some());
    assert_eq!(workspace.history("C1"), vec![merged]);
    runtime.block_on(async {
        let history = store.load_history("C1").await.unwrap().unwrap();
        let thread = store.load_thread("C1", "1.0").await.unwrap().unwrap();
        let catalog = store.load_thread_catalog().await.unwrap();
        for cached in [&history[0], &thread[0]] {
            assert_eq!(cached.body_text(), "edited");
            assert_eq!(cached.reactions, freshest.reactions);
            assert_eq!(cached.reply_count, Some(4));
        }
        assert_eq!(catalog[0].root.as_ref().unwrap().body_text(), "edited");
    });
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn runtime_failures_map_typed_boundary_categories_to_safe_messages() {
    let auth = anyhow::Error::new(crate::slack::SlackError::Api {
        method: "auth.test".to_string(),
        code: "invalid_auth".to_string(),
    });
    let storage = anyhow::Error::new(crate::store::StoreError::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "secret cache path",
    )));
    let permission = anyhow::Error::new(crate::slack::SlackError::Api {
        method: "conversations.invite".to_string(),
        code: "no_permission".to_string(),
    });

    let auth = RuntimeFailure::from_error(&auth);
    let storage = RuntimeFailure::from_error(&storage);
    let permission = RuntimeFailure::from_error(&permission);

    assert_eq!(auth.category, RuntimeFailureCategory::Authentication);
    assert_eq!(auth.message, "Slack authentication failed. Sign in again.");
    assert_eq!(storage.category, RuntimeFailureCategory::Storage);
    assert_eq!(storage.message, "Conduit could not access its local data.");
    assert!(!storage.message.contains("secret cache path"));
    assert_eq!(permission.category, RuntimeFailureCategory::Validation);
    assert_eq!(
        permission.message,
        "Slack does not allow this action for this conversation."
    );
}

#[test]
fn runtime_failures_map_rate_limits_validation_and_unknown_errors() {
    let rate_limit = anyhow::Error::new(crate::slack::SlackError::RateLimited {
        method: "conversations.history".to_string(),
    });
    let validation = RuntimeFailure::validation("Enter both browser-session tokens");
    let unknown = RuntimeFailure::from_error(&anyhow::anyhow!("sensitive internals"));

    assert_eq!(
        RuntimeFailure::from_error(&rate_limit).category,
        RuntimeFailureCategory::RateLimited
    );
    assert_eq!(validation.category, RuntimeFailureCategory::Validation);
    assert_eq!(validation.message, "Enter both browser-session tokens");
    assert_eq!(unknown.category, RuntimeFailureCategory::Internal);
    assert_eq!(unknown.message, "Conduit encountered an unexpected error.");
}

#[test]
fn browser_session_connectivity_failure_explains_user_agent_recovery() {
    let timeout = anyhow::Error::new(crate::slack::SlackError::from(anyhow::Error::new(
        std::io::Error::new(std::io::ErrorKind::TimedOut, "request timed out"),
    )));

    let failure = authentication_failure(AuthenticationFailureContext::BrowserSession, &timeout);

    assert_eq!(failure.category, RuntimeFailureCategory::Network);
    assert!(failure.message.contains("exact User-Agent"));
    assert!(failure.message.contains("XOXC/XOXD"));
    assert!(failure.message.contains("use OAuth"));
    assert!(failure.message.contains("TLS fingerprint"));
    assert!(!failure.message.contains("request timed out"));
}

struct CancellationSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for CancellationSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[test]
fn background_work_does_not_block_later_interactive_work() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
        let (background_started_tx, background_started_rx) = tokio::sync::oneshot::channel();
        let background_gate = Arc::new(tokio::sync::Notify::new());
        let background_task = tokio::spawn({
            let limits = limits.clone();
            let background_gate = Arc::clone(&background_gate);
            async move {
                let _permit = limits.acquire(RuntimeTaskLane::Background).await;
                let _ = background_started_tx.send(());
                background_gate.notified().await;
            }
        });

        background_started_rx
            .await
            .expect("background task did not start");
        let interactive_permit = tokio::time::timeout(
            Duration::from_millis(100),
            limits.acquire(RuntimeTaskLane::Interactive),
        )
        .await;

        assert!(
            interactive_permit.is_ok(),
            "interactive work was blocked by background work"
        );
        background_task.abort();
    });
}

#[test]
fn image_work_does_not_block_later_upload_work() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
        let (image_started_tx, image_started_rx) = tokio::sync::oneshot::channel();
        let image_gate = Arc::new(tokio::sync::Notify::new());
        let image_task = tokio::spawn({
            let limits = limits.clone();
            let image_gate = Arc::clone(&image_gate);
            async move {
                let _permit = limits.acquire(RuntimeTaskLane::Image).await;
                let _ = image_started_tx.send(());
                image_gate.notified().await;
            }
        });

        image_started_rx.await.expect("image task did not start");
        let upload_permit = tokio::time::timeout(
            Duration::from_millis(100),
            limits.acquire(RuntimeTaskLane::Upload),
        )
        .await;

        assert!(
            upload_permit.is_ok(),
            "upload work was blocked by image work"
        );
        image_task.abort();
    });
}

#[test]
fn navigation_work_does_not_block_behind_interactive_mutation() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
        let interactive_permit = limits.acquire(RuntimeTaskLane::Interactive).await;
        let navigation_permit = tokio::time::timeout(
            Duration::from_millis(100),
            limits.acquire(RuntimeTaskLane::Navigation),
        )
        .await;

        assert!(
            navigation_permit.is_ok(),
            "navigation work was blocked by an interactive mutation"
        );
        drop(interactive_permit);
    });
}

#[test]
fn switching_main_navigation_aborts_old_target_and_starts_new_target() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let session = SessionId::default().next();
        let state = Arc::new(Mutex::new(RuntimeState::new(session)));
        let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
        let first_command = RuntimeCommand::LoadHistory {
            channel_id: "C1".to_string(),
        };
        let second_command = RuntimeCommand::LoadHistory {
            channel_id: "C2".to_string(),
        };
        let first_identity = RuntimeIdentity {
            session,
            request: RequestId::new(1),
        };
        let second_identity = RuntimeIdentity {
            session,
            request: RequestId::new(2),
        };
        let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
        let (first_cancelled_tx, first_cancelled_rx) = tokio::sync::oneshot::channel();
        let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();

        let first_limits = limits.clone();
        spawn_request_task(
            &state,
            TrackedRequest::for_command(first_identity, &first_command),
            async move {
                let _permit = first_limits.acquire(RuntimeTaskLane::Navigation).await;
                let _cancelled = CancellationSignal(Some(first_cancelled_tx));
                let _ = first_started_tx.send(());
                future::pending::<()>().await;
            },
        );
        first_started_rx
            .await
            .expect("first navigation did not start");

        let second_limits = limits;
        spawn_request_task(
            &state,
            TrackedRequest::for_command(second_identity, &second_command),
            async move {
                let _permit = second_limits.acquire(RuntimeTaskLane::Navigation).await;
                let _ = second_started_tx.send(());
            },
        );

        tokio::time::timeout(Duration::from_millis(100), first_cancelled_rx)
            .await
            .expect("abandoned navigation was not aborted")
            .expect("navigation cancellation signal dropped");
        tokio::time::timeout(Duration::from_millis(100), second_started_rx)
            .await
            .expect("new navigation did not get capacity")
            .expect("new navigation start signal dropped");
    });
}

#[test]
fn mutations_are_session_tracked_without_same_context_supersession() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let session = SessionId::default().next();
        let command = RuntimeCommand::PostMessage {
            channel_id: "C1".to_string(),
            text: "hello".to_string(),
            blocks_json: None,
            attachments_json: None,
            thread_ts: None,
        };
        let first = TrackedRequest::for_command(
            RuntimeIdentity {
                session,
                request: RequestId::new(1),
            },
            &command,
        );
        let second = TrackedRequest::for_command(
            RuntimeIdentity {
                session,
                request: RequestId::new(2),
            },
            &command,
        );
        let first_task = tokio::spawn(future::pending::<()>());
        let second_task = tokio::spawn(future::pending::<()>());
        let mut state = RuntimeState::new(session);

        assert!(state.register_task(session, 1, Some(first.clone()), first_task.abort_handle(),));
        assert!(state.register_task(session, 2, Some(second.clone()), second_task.abort_handle(),));
        assert!(!first_task.is_finished());
        assert!(!second_task.is_finished());
        assert!(state.active_requests.is_empty());

        state.finish_task(1, Some(&first));
        state.finish_task(2, Some(&second));
        first_task.abort();
        second_task.abort();
    });
}

#[test]
fn only_read_commands_supersede_previous_requests() {
    assert!(RuntimeCommand::SearchMessages {
        query: "hello".to_string(),
    }
    .supersedes_previous());
    assert!(!RuntimeCommand::SetSaved {
        channel_id: "C1".to_string(),
        ts: "1.0".to_string(),
        add: true,
        thread_ts: None,
    }
    .supersedes_previous());
    assert!(!RuntimeCommand::UploadFiles {
        channel_id: "C1".to_string(),
        thread_ts: None,
        attachments: vec![UploadAttachment {
            path: PathBuf::from("example.txt"),
            remove_after_upload: false,
        }],
        blocks_json: None,
    }
    .supersedes_previous());
}

#[test]
fn superseded_request_cleanup_does_not_remove_newer_request() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let session = SessionId::default().next();
        let context = OperationContext::new(RuntimeOperation::Search, RuntimeTarget::Workspace);
        let first = TrackedRequest::new(
            RuntimeIdentity {
                session,
                request: RequestId::new(1),
            },
            context.clone(),
        );
        let second = TrackedRequest::new(
            RuntimeIdentity {
                session,
                request: RequestId::new(2),
            },
            context.clone(),
        );
        let old_task = tokio::spawn(future::pending::<()>());
        let new_task = tokio::spawn(future::pending::<()>());
        let mut state = RuntimeState::new(session);

        state.register_task(session, 1, Some(first.clone()), old_task.abort_handle());
        state.register_task(session, 2, Some(second.clone()), new_task.abort_handle());
        let old_result = tokio::time::timeout(Duration::from_millis(100), old_task)
            .await
            .expect("superseded task was not aborted")
            .expect_err("superseded task completed normally");
        assert!(old_result.is_cancelled());

        state.finish_task(1, Some(&first));
        assert_eq!(
            state
                .active_requests
                .get(&context)
                .map(|request| request.task_id),
            Some(2)
        );

        state.finish_task(2, Some(&second));
        assert!(!state.active_requests.contains_key(&context));
        new_task.abort();
    });
}

#[test]
fn completed_newer_request_still_rejects_older_background_request() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let session = SessionId::default().next();
        let context =
            OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace);
        let newer = TrackedRequest::new(
            RuntimeIdentity {
                session,
                request: RequestId::new(2),
            },
            context.clone(),
        );
        let older = TrackedRequest::new(
            RuntimeIdentity {
                session,
                request: RequestId::new(1),
            },
            context.clone(),
        );
        let newer_task = tokio::spawn(future::pending::<()>());
        let older_task = tokio::spawn(future::pending::<()>());
        let mut state = RuntimeState::new(session);

        assert!(state.register_task(session, 2, Some(newer.clone()), newer_task.abort_handle(),));
        state.finish_task(2, Some(&newer));
        assert!(!state.register_task(session, 1, Some(older), older_task.abort_handle(),));
        let older_result = tokio::time::timeout(Duration::from_millis(100), older_task)
            .await
            .expect("older background task was not aborted")
            .expect_err("older background task completed normally");

        assert!(older_result.is_cancelled());
        assert_eq!(
            state.latest_requests.get(&context),
            Some(&RequestId::new(2))
        );
        newer_task.abort();
    });
}

#[test]
fn sign_out_still_completes_when_keyring_clear_fails() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let (sender, mut events) = runtime_event_channel();
        let event_sender = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SignOut, RuntimeTarget::Workspace),
        );

        finish_sign_out(&event_sender, Err(anyhow!("keyring unavailable")));

        assert!(matches!(
            events.recv().await.map(|event| event.kind),
            Some(RuntimeEventKind::Error(_))
        ));
        assert!(matches!(
            events.recv().await.map(|event| event.kind),
            Some(RuntimeEventKind::WorkspaceLifecycle(
                WorkspaceLifecycleEvent::SignedOut
            ))
        ));
        assert!(matches!(
            events.recv().await.map(|event| event.kind),
            Some(RuntimeEventKind::SignedOut)
        ));
    });
}

#[test]
fn message_completion_follows_its_canonical_workspace_patch() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let (sender, mut events) = runtime_event_channel();
        let event_sender = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(
                RuntimeOperation::PostMessage,
                RuntimeTarget::Message {
                    channel_id: "C1".into(),
                    thread_ts: None,
                },
            ),
        );
        let message = SlackMessage {
            ts: "1.000".into(),
            text: Some("sent".into()),
            ..SlackMessage::default()
        };
        let workspace = WorkspaceReducerAdapter::default();
        workspace
            .apply_persisted_and_publish_with_completion(
                None,
                &event_sender,
                MutationOrigin::Local,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: message.clone(),
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Local,
                },
                RuntimeEventKind::MessagePostCompleted {
                    channel_id: "C1".into(),
                    message_ts: message.ts.clone(),
                    thread_ts: None,
                },
            )
            .await
            .unwrap();

        let patch = events
            .recv()
            .await
            .expect("workspace patch should be queued");
        assert!(matches!(patch.kind, RuntimeEventKind::WorkspacePatch(_)));
        let event = events.recv().await.expect("completion should follow patch");
        let RuntimeEventKind::MessagePostCompleted {
            channel_id,
            message_ts,
            thread_ts: None,
        } = event.kind
        else {
            panic!("expected message post completion event");
        };
        assert_eq!(channel_id, "C1");
        assert_eq!(message_ts, message.ts);
        assert!(events.try_recv().is_err());
    });
}

#[test]
fn lifecycle_failure_event_distinguishes_authentication_from_retryable_failures() {
    assert_eq!(
        lifecycle_failure_event(&RuntimeFailure {
            category: RuntimeFailureCategory::Authentication,
            message: "safe".into(),
        }),
        WorkspaceLifecycleEvent::AuthenticationFailed
    );
    for category in [
        RuntimeFailureCategory::Network,
        RuntimeFailureCategory::RateLimited,
        RuntimeFailureCategory::Storage,
        RuntimeFailureCategory::Validation,
        RuntimeFailureCategory::Internal,
    ] {
        assert_eq!(
            lifecycle_failure_event(&RuntimeFailure {
                category,
                message: "safe".into(),
            }),
            WorkspaceLifecycleEvent::RetryableFailure
        );
    }
}

#[test]
fn lifecycle_runtime_event_has_workspace_operation_context() {
    let fallback = OperationContext::new(
        RuntimeOperation::History,
        RuntimeTarget::Channel("C1".into()),
    );

    assert_eq!(
        RuntimeEventKind::WorkspaceLifecycle(WorkspaceLifecycleEvent::RecoveryStarted)
            .operation_context(&fallback),
        OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace)
    );
}

#[test]
fn fresh_history_completion_keeps_older_timestamp_post_after_request_base() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(async {
        let workspace = WorkspaceReducerAdapter::default();
        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender::new(
            sender,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(
                RuntimeOperation::History,
                RuntimeTarget::Channel("C1".into()),
            ),
        );
        let requested = vec![
            SlackMessage {
                ts: "10.0".into(),
                text: Some("newest requested".into()),
                ..Default::default()
            },
            SlackMessage {
                ts: "09.0".into(),
                text: Some("oldest requested".into()),
                ..Default::default()
            },
        ];
        workspace
            .apply_persisted_and_publish(
                None,
                &events,
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![SlackConversation {
                        id: "C1".into(),
                        ..Default::default()
                    }],
                    histories: HashMap::from([("C1".into(), requested.clone())]),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));
        let request_base = workspace.revision();
        workspace
            .apply_persisted_and_publish(
                None,
                &events,
                MutationOrigin::Realtime,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: "01.0".into(),
                        user: Some("U_SELF".into()),
                        text: Some("delayed socket post".into()),
                        ..Default::default()
                    },
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Realtime,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));

        publish_history_snapshot_with_completion(
            &events,
            &None,
            &workspace,
            "C1",
            MutationOrigin::WebApi,
            request_base,
            requested,
            true,
            Some("older".into()),
            false,
            false,
            false,
        )
        .await
        .unwrap();
        let completion = receiver.recv().await.unwrap();
        let RuntimeEventKind::HistoryLoadCompleted {
            has_more: true,
            next_cursor: Some(cursor),
            ..
        } = completion.kind
        else {
            panic!("fresh history completion was not queued");
        };
        assert_eq!(cursor, "older");
        assert_eq!(
            workspace
                .history("C1")
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["01.0", "09.0", "10.0"]
        );
    });
}

#[test]
fn interactive_history_has_no_legacy_store_or_attention_bypasses() {
    let runtime_source = include_str!("../runtime.rs");
    let production = runtime_source
        .split_once("#[cfg(test)]\nmod tests")
        .unwrap()
        .0;
    let history_commands = production
        .split_once("RuntimeCommand::LoadHistory")
        .unwrap()
        .1
        .split_once("RuntimeCommand::LoadThread")
        .unwrap()
        .0;
    let (latest, _older) = history_commands
        .split_once("RuntimeCommand::LoadOlderHistory")
        .unwrap();
    let cache_read = latest.find("service.load_cached").unwrap();
    let loading_status = latest
        .find("send_status(\"Loading conversation\")")
        .unwrap();
    let network_base = latest.find("let base_revision").unwrap();
    let network_fetch = latest.find("service.fetch").unwrap();
    assert!(cache_read < loading_status);
    assert!(loading_status < network_base);
    assert!(network_base < network_fetch);
    assert!(!history_commands.contains("persist_snapshot_attention"));
    assert!(!history_commands.contains("store_merged_history"));
    assert!(!history_commands.contains("observe_thread_history("));
    assert!(!history_commands.contains("context.workspace.apply("));
    assert_eq!(
        history_commands
            .matches("publish_history_snapshot_with_completion(")
            .count(),
        3
    );
    let service_source = include_str!("../services/conversation_history.rs");
    let service_production = service_source.split_once("#[cfg(test)]").unwrap().0;
    assert!(!service_production.contains("async fn store_history"));
    assert!(!service_production.contains(".store_history("));
    assert!(!service_production.contains("CacheWriteFailed"));
}

async fn assert_history_completion_serializes_canonical_producer() {
    let workspace = WorkspaceReducerAdapter::default();
    workspace.apply(
        MutationOrigin::Cache,
        WorkspaceMutation::ConversationUpsert(SlackConversation {
            id: "C1".into(),
            ..Default::default()
        }),
    );
    let base_revision = workspace.revision();
    let (runtime_events, mut runtime_receiver) = runtime_event_channel();
    let events = RuntimeEventSender::new(
        runtime_events,
        RuntimeIdentity {
            session: SessionId::default().next(),
            request: RequestId::new(1),
        },
        OperationContext::new(
            RuntimeOperation::History,
            RuntimeTarget::Channel("C1".into()),
        ),
    );
    let (gate_started, gate_started_rx) = std::sync::mpsc::channel();
    let (release_gate, release_gate_rx) = std::sync::mpsc::channel();
    workspace.set_history_completion_send_gate(Arc::new(TestWorkspacePatchSendGate {
        started: gate_started,
        release: Mutex::new(release_gate_rx),
    }));

    let history_events = events.clone();
    let history_workspace = workspace.clone();
    let history = tokio::spawn(async move {
        publish_history_snapshot_with_completion(
            &history_events,
            &None,
            &history_workspace,
            "C1",
            MutationOrigin::WebApi,
            base_revision,
            vec![SlackMessage {
                ts: "1.0".into(),
                text: Some("history".into()),
                ..Default::default()
            }],
            false,
            None,
            true,
            false,
            false,
        )
        .await
        .unwrap();
    });
    gate_started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("history completion did not reach its publication barrier");

    let producer_workspace = workspace.clone();
    let producer_events = events.clone();
    let (producer_attempted, producer_attempted_rx) = std::sync::mpsc::channel();
    let (producer_completed, producer_completed_rx) = std::sync::mpsc::channel();
    let producer = tokio::spawn(async move {
        producer_attempted.send(()).unwrap();
        let message = SlackMessage {
            ts: "2.0".into(),
            user: Some("U_SELF".into()),
            text: Some("direct producer".into()),
            ..Default::default()
        };
        producer_workspace
            .apply_persisted_and_publish_with_completion(
                None,
                &producer_events,
                MutationOrigin::Local,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: message.clone(),
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Local,
                },
                RuntimeEventKind::MessagePostCompleted {
                    channel_id: "C1".into(),
                    message_ts: message.ts,
                    thread_ts: None,
                },
            )
            .await
            .unwrap();
        producer_completed.send(()).unwrap();
    });
    producer_attempted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("direct producer did not start");
    let producer_overtook_completion = producer_completed_rx
        .recv_timeout(Duration::from_millis(50))
        .is_ok();

    release_gate.send(()).unwrap();
    history.await.unwrap();
    producer.await.unwrap();
    assert!(
        !producer_overtook_completion,
        "direct producer entered after the canonical clone but before HistoryLoadCompleted"
    );

    let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
        .map(|event| event.kind)
        .collect::<Vec<_>>();
    let history_position = delivered
        .iter()
        .position(|event| matches!(event, RuntimeEventKind::HistoryLoadCompleted { .. }))
        .expect("history completion was not queued");
    let producer_position = delivered
        .iter()
        .position(|event| {
            matches!(
                event,
                RuntimeEventKind::WorkspacePatch(patch)
                    if timeline_patch_summary(patch.changes())
                        .iter()
                        .any(|(message_ts, _)| message_ts == "2.0")
            )
        })
        .expect("direct producer patch was not queued");
    assert!(history_position < producer_position);
}

#[test]
fn history_patch_and_completion_are_atomic_against_canonical_message_producers() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(async {
        assert_history_completion_serializes_canonical_producer().await;
    });
}

#[test]
fn realtime_persistence_channel_backpressures_at_capacity_and_preserves_fifo() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let metrics = Arc::new(AttentionMetrics::default());
        let (sender, mut receiver) =
            realtime_persistence_channel_with_capacity(Arc::clone(&metrics), 2);
        let event = |ts: &str| RealtimePersistenceEvent::Message {
            event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                channel_id: "C1".into(),
                message: SlackMessage {
                    ts: ts.into(),
                    ..Default::default()
                },
                kind: SocketModeMessageKind::Posted,
            }),
        };
        let event_ts = |event: RealtimePersistenceEvent| match event {
            RealtimePersistenceEvent::Message { event } => event.message.ts,
            other => panic!("expected message event, got {other:?}"),
        };

        sender.send(event("1.0")).await.unwrap();
        sender.send(event("2.0")).await.unwrap();
        let third_sender = sender.clone();
        let (attempted_sender, attempted_receiver) = oneshot::channel();
        let mut third_send = tokio::spawn(async move {
            let _ = attempted_sender.send(());
            third_sender.send(event("3.0")).await.unwrap();
        });
        attempted_receiver.await.unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut third_send)
                .await
                .is_err(),
            "send unexpectedly completed while the queue was full"
        );
        let saturated = metrics.snapshot();
        assert_eq!(saturated.queue_enqueued, 2);
        assert_eq!(saturated.queue_depth, 2);
        assert_eq!(saturated.queue_peak_depth, 2);
        assert_eq!(event_ts(receiver.recv().await.unwrap()), "1.0");

        third_send.await.unwrap();
        drop(sender);
        assert_eq!(event_ts(receiver.recv().await.unwrap()), "2.0");
        assert_eq!(event_ts(receiver.recv().await.unwrap()), "3.0");
        assert!(receiver.recv().await.is_none());

        let drained = metrics.snapshot();
        assert_eq!(drained.queue_enqueued, 3);
        assert_eq!(drained.queue_dequeued, 3);
        assert_eq!(drained.queue_depth, 0);
        assert_eq!(drained.queue_peak_depth, 2);
        assert_eq!(drained.queue_rejected, 0);
    });
}

#[test]
fn realtime_persistence_drain_waits_for_fifo_and_reconciles_depth() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let workspace = WorkspaceReducerAdapter::default();
        let metrics = workspace.attention_metrics_handle();
        let (sender, mut receiver) =
            realtime_persistence_channel_with_capacity(Arc::clone(&metrics), 3);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_by_worker = Arc::clone(&observed);
        let (release_worker, worker_gate) = oneshot::channel();
        let mut persistence_tasks = tokio::task::JoinSet::new();
        persistence_tasks.spawn(async move {
            let _ = worker_gate.await;
            while let Some(event) = receiver.recv().await {
                let RealtimePersistenceEvent::Message { event } = event else {
                    panic!("expected message event");
                };
                observed_by_worker
                    .lock()
                    .expect("observed event lock poisoned")
                    .push(event.message.ts);
            }
        });
        let event = |ts: &str| RealtimePersistenceEvent::Message {
            event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                channel_id: "C1".into(),
                message: SlackMessage {
                    ts: ts.into(),
                    ..Default::default()
                },
                kind: SocketModeMessageKind::Posted,
            }),
        };

        sender.send(event("1.0")).await.unwrap();
        sender.send(event("2.0")).await.unwrap();
        sender.send(event("3.0")).await.unwrap();
        assert_eq!(metrics.snapshot().queue_depth, 3);

        release_worker.send(()).unwrap();
        drain_realtime_persistence(Some(sender), &mut persistence_tasks, &workspace).await;

        assert_eq!(
            *observed.lock().expect("observed event lock poisoned"),
            ["1.0", "2.0", "3.0"]
        );
        let drained = metrics.snapshot();
        assert_eq!(drained.queue_enqueued, 3);
        assert_eq!(drained.queue_dequeued, 3);
        assert_eq!(drained.queue_depth, 0);
        assert_eq!(drained.queue_peak_depth, 3);
        assert_eq!(drained.queue_rejected, 0);
    });
}

#[test]
fn realtime_status_persistence_skips_superseded_user_updates() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-realtime-status-persistence-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let (runtime_events, _receiver) = runtime_event_channel();
        let event_sender = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );
        let workspace = WorkspaceReducerAdapter::default();
        let sync = UserStatusSync::default();
        let old_revision = sync.publish_change("U1", || {});
        let current_revision = sync.publish_change("U1", || {});
        let (sender, receiver) = realtime_persistence_channel(workspace.attention_metrics_handle());
        let worker = tokio::spawn(persist_realtime_events(
            receiver,
            store.clone(),
            Some("U_SELF".into()),
            event_sender,
            workspace,
            sync,
        ));
        let status_user = |text: &str| {
            Box::new(SlackUser {
                id: Some("U1".into()),
                profile: Some(crate::models::SlackUserProfile {
                    status_text: Some(text.to_string()),
                    status_emoji: Some(String::new()),
                    status_expiration: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            })
        };
        sender
            .send(RealtimePersistenceEvent::UserChanged {
                user: status_user("Current"),
                status_revision: Some(current_revision),
            })
            .await
            .unwrap();
        sender
            .send(RealtimePersistenceEvent::UserChanged {
                user: status_user("Stale"),
                status_revision: Some(old_revision),
            })
            .await
            .unwrap();
        drop(sender);
        worker.await.unwrap();

        let statuses = store.load_user_statuses().await.unwrap();
        assert_eq!(
            statuses.get("U1").map(|status| status.text.as_str()),
            Some("Current")
        );
        std::fs::remove_dir_all(directory).unwrap();
    });
}

#[test]
fn socket_message_deltas_wait_for_store_admission_and_persist_post_edit_delete() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-realtime-message-authority-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let conversation = SlackConversation {
            id: "C1".into(),
            ..Default::default()
        };
        store
            .store_conversations(std::slice::from_ref(&conversation))
            .await
            .unwrap();

        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(conversation),
        );
        let admission = workspace.store_batch_admission.lock().await;
        let (runtime_events, mut runtime_receiver) = runtime_event_channel();
        let event_sender = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );
        let (sender, receiver) = realtime_persistence_channel(workspace.attention_metrics_handle());
        let worker = tokio::spawn(persist_realtime_events(
            receiver,
            store.clone(),
            Some("U_SELF".into()),
            event_sender,
            workspace.clone(),
            UserStatusSync::default(),
        ));

        let socket_message = |ts: &str, text: &str, kind| RealtimePersistenceEvent::Message {
            event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                channel_id: "C1".into(),
                message: SlackMessage {
                    ts: ts.into(),
                    user: Some("U_OTHER".into()),
                    text: Some(text.into()),
                    ..Default::default()
                },
                kind,
            }),
        };
        sender
            .send(socket_message(
                "1.0",
                "original",
                SocketModeMessageKind::Posted,
            ))
            .await
            .unwrap();
        sender
            .send(socket_message(
                "1.0",
                "edited",
                SocketModeMessageKind::Changed,
            ))
            .await
            .unwrap();
        sender
            .send(socket_message(
                "1.0",
                "deleted",
                SocketModeMessageKind::Deleted,
            ))
            .await
            .unwrap();
        sender
            .send(socket_message(
                "2.0",
                "survives",
                SocketModeMessageKind::Posted,
            ))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), runtime_receiver.recv())
                .await
                .is_err(),
            "socket message authority must wait behind an admitted history mutation"
        );
        drop(admission);
        drop(sender);
        worker.await.unwrap();

        let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
            .flat_map(|event| match event.kind {
                RuntimeEventKind::WorkspacePatch(patch) => timeline_patch_summary(patch.changes()),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            delivered,
            [
                ("1.0".into(), Some("original".into())),
                ("1.0".into(), Some("edited".into())),
                ("1.0".into(), None),
                ("2.0".into(), Some("survives".into())),
            ],
            "canonical timeline patches must retain socket FIFO order"
        );

        let stored = store.load_history("C1").await.unwrap().unwrap();
        assert_eq!(
            stored
                .iter()
                .map(|message| (message.ts.as_str(), message.body_text()))
                .collect::<Vec<_>>(),
            vec![("2.0", "survives".into())]
        );
        assert_eq!(
            workspace
                .history("C1")
                .iter()
                .map(|message| (message.ts.as_str(), message.body_text()))
                .collect::<Vec<_>>(),
            vec![("2.0", "survives".into())]
        );
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn socket_recovery_failure_skips_claim_and_queues_delta_behind_pending_write() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-realtime-pending-read-recovery-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let conversation = SlackConversation {
            id: "D1".into(),
            is_im: Some(true),
            ..Default::default()
        };
        store
            .store_conversations(std::slice::from_ref(&conversation))
            .await
            .unwrap();
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(conversation),
        );
        let (runtime_events, mut runtime_receiver) = runtime_event_channel();
        let events = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );

        store
            .install_conversation_batch_failure_trigger_for("D1")
            .await
            .unwrap();
        assert!(workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::Local,
                WorkspaceMutation::ConversationStarChanged {
                    channel_id: "D1".into(),
                    starred: true,
                },
            )
            .await
            .is_err());
        let (sender, receiver) = realtime_persistence_channel(workspace.attention_metrics_handle());
        let worker = tokio::spawn(persist_realtime_events(
            receiver,
            store.clone(),
            Some("U_SELF".into()),
            events,
            workspace.clone(),
            UserStatusSync::default(),
        ));
        sender
            .send(RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "D1".into(),
                    message: SlackMessage {
                        ts: "10.0".into(),
                        user: Some("U_OTHER".into()),
                        text: Some("must not claim".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                }),
            })
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(1), runtime_receiver.recv())
            .await
            .expect("recovery-failed message did not reach canonical UI")
            .expect("runtime event channel closed");
        assert!(matches!(first.kind, RuntimeEventKind::WorkspacePatch(patch)
            if timeline_patch_summary(patch.changes())
                == [("10.0".into(), Some("must not claim".into()))]));
        assert!(runtime_receiver.try_recv().is_err());
        {
            let pending = workspace
                .pending_writes
                .lock()
                .expect("pending workspace writes lock poisoned");
            assert_eq!(pending.len(), 2);
            let revisions = pending
                .iter()
                .map(|entry| entry.batch.as_ref().unwrap().revision())
                .collect::<Vec<_>>();
            assert!(revisions[0] < revisions[1]);
        }
        let metrics = workspace.attention_metrics_snapshot();
        assert_eq!(
            metrics.persistence_count(AttentionPersistenceOutcome::Failed),
            1
        );
        assert_eq!(
            metrics.persistence_count(AttentionPersistenceOutcome::Accepted),
            0
        );
        assert_eq!(metrics.notification_claims, 0);

        sender
            .send(RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "D1".into(),
                    message: SlackMessage {
                        ts: "30.0".into(),
                        user: Some("U_OTHER".into()),
                        text: Some("new while recovery is blocked".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                }),
            })
            .await
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(1), runtime_receiver.recv())
            .await
            .expect("second recovery-failed message did not reach canonical UI")
            .expect("runtime event channel closed");
        assert!(
            matches!(second.kind, RuntimeEventKind::WorkspacePatch(patch)
            if timeline_patch_summary(patch.changes())
                == [("30.0".into(), Some("new while recovery is blocked".into()))])
        );
        assert!(runtime_receiver.try_recv().is_err());
        assert_eq!(
            workspace
                .pending_writes
                .lock()
                .expect("pending workspace writes lock poisoned")
                .len(),
            3
        );
        let metrics = workspace.attention_metrics_snapshot();
        assert_eq!(
            metrics.persistence_count(AttentionPersistenceOutcome::Failed),
            2
        );
        assert_eq!(metrics.notification_claims, 0);

        store
            .clear_conversation_batch_failure_trigger()
            .await
            .unwrap();
        sender
            .send(RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "D1".into(),
                    message: SlackMessage {
                        ts: "40.0".into(),
                        user: Some("U_SELF".into()),
                        text: Some("recovery trigger".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                }),
            })
            .await
            .unwrap();
        drop(sender);
        worker.await.unwrap();

        let recovered_star = runtime_receiver.recv().await.unwrap();
        let recovered_stale_message = runtime_receiver.recv().await.unwrap();
        let recovered_new_message = runtime_receiver.recv().await.unwrap();
        let current_patch = runtime_receiver.recv().await.unwrap();
        let (
            RuntimeEventKind::WorkspacePatch(star_patch),
            RuntimeEventKind::WorkspacePatch(stale_message_patch),
            RuntimeEventKind::WorkspacePatch(new_message_patch),
        ) = (
            recovered_star.kind,
            recovered_stale_message.kind,
            recovered_new_message.kind,
        )
        else {
            panic!("the older star change and both message patches must recover first");
        };
        assert!(star_patch.revision() < stale_message_patch.revision());
        assert!(stale_message_patch.revision() < new_message_patch.revision());
        assert!(
            matches!(current_patch.kind, RuntimeEventKind::WorkspacePatch(patch)
            if timeline_patch_summary(patch.changes())
                == [("40.0".into(), Some("recovery trigger".into()))])
        );
        assert!(runtime_receiver.try_recv().is_err());

        let persisted_conversation = store
            .load_conversations()
            .await
            .unwrap()
            .unwrap()
            .into_iter()
            .find(|conversation| conversation.id == "D1")
            .unwrap();
        assert_eq!(persisted_conversation.is_starred, Some(true));
        assert_eq!(
            store
                .load_history("D1")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| message.ts.as_str())
                .collect::<Vec<_>>(),
            vec!["40.0", "30.0", "10.0"]
        );
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn message_authority_uses_canonical_batches_and_keeps_queue_fallback_explicit() {
    let source = include_str!("../runtime.rs");
    let post_command = source
        .split_once("RuntimeCommand::PostMessage {")
        .unwrap()
        .1
        .split_once("RuntimeCommand::SetReaction {")
        .unwrap()
        .0;
    assert!(post_command.contains("apply_persisted_and_publish_with_completion("));
    assert!(!post_command.contains("persist_local_post_message("));
    assert!(!post_command.contains("store_merged_history("));
    assert!(!post_command.contains("store_merged_thread("));

    let socket_persistence = source
        .split_once("async fn persist_socket_message(")
        .unwrap()
        .1
        .split_once("#[derive(Debug, Clone, Copy, PartialEq, Eq)]")
        .unwrap()
        .0;
    assert!(
        socket_persistence.contains("acquire_store_admission(&workspace.store_batch_admission)")
    );
    assert!(socket_persistence.contains("recover_persisted_admitted(Some(store))"));
    assert!(socket_persistence.contains("apply_and_enqueue("));
    assert!(!socket_persistence.contains("store_merged_history("));
    assert!(!socket_persistence.contains("store_merged_thread("));
    assert!(!socket_persistence.contains("workspace.apply("));
    assert!(!socket_persistence.contains("RuntimeEventKind::SocketModeEvent"));
    assert!(!socket_persistence.contains("observe_socket_thread_catalog("));

    let worker_message_path = source
        .split_once("RealtimePersistenceEvent::Message { event } => {")
        .unwrap()
        .1
        .split_once("RealtimePersistenceEvent::OrderedEvent { event } => {")
        .unwrap()
        .0;
    assert!(worker_message_path.contains("persist_socket_message("));
    assert!(!worker_message_path.contains("workspace.apply("));
    assert!(!worker_message_path.contains("store_merged_history("));
    assert!(!worker_message_path.contains("store_merged_thread("));

    let queue_fallback = source
        .split_once("RealtimePersistenceQueueRejected reason=worker_closed")
        .unwrap()
        .1
        .split_once("drain_realtime_persistence")
        .unwrap()
        .0;
    assert!(queue_fallback.contains("apply_realtime_persistence_queue_fallback("));
}

#[test]
fn failed_socket_delta_recovers_typed_patches_fifo() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-realtime-message-recovery-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let conversation = SlackConversation {
            id: "C1".into(),
            ..Default::default()
        };
        store
            .store_conversations(std::slice::from_ref(&conversation))
            .await
            .unwrap();
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(conversation),
        );
        store
            .install_history_batch_failure_trigger_for("C1")
            .await
            .unwrap();

        let (runtime_events, mut runtime_receiver) = runtime_event_channel();
        let event_sender = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );
        let (sender, receiver) = realtime_persistence_channel(workspace.attention_metrics_handle());
        let worker = tokio::spawn(persist_realtime_events(
            receiver,
            store.clone(),
            Some("U_SELF".into()),
            event_sender,
            workspace.clone(),
            UserStatusSync::default(),
        ));
        let self_post = |ts: &str, text: &str| RealtimePersistenceEvent::Message {
            event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                channel_id: "C1".into(),
                message: SlackMessage {
                    ts: ts.into(),
                    user: Some("U_SELF".into()),
                    text: Some(text.into()),
                    ..Default::default()
                },
                kind: SocketModeMessageKind::Posted,
            }),
        };
        sender.send(self_post("1.0", "first")).await.unwrap();

        let visible = tokio::time::timeout(Duration::from_secs(1), runtime_receiver.recv())
            .await
            .expect("unpersisted delta did not reach canonical UI")
            .expect("runtime event channel closed");
        let RuntimeEventKind::WorkspacePatch(visible_patch) = visible.kind else {
            panic!("unpersisted delta must publish its canonical patch");
        };
        assert_eq!(
            timeline_patch_summary(visible_patch.changes()),
            [("1.0".into(), Some("first".into()))]
        );
        assert_eq!(
            workspace
                .pending_writes
                .lock()
                .expect("pending workspace writes lock poisoned")
                .len(),
            1
        );

        store.clear_history_batch_failure_trigger().await.unwrap();
        sender.send(self_post("2.0", "second")).await.unwrap();
        drop(sender);
        worker.await.unwrap();

        let recovered = runtime_receiver.recv().await.unwrap();
        let current = runtime_receiver.recv().await.unwrap();
        let RuntimeEventKind::WorkspacePatch(recovered_patch) = recovered.kind else {
            panic!("older failed delta patch must recover first");
        };
        let RuntimeEventKind::WorkspacePatch(current_patch) = current.kind else {
            panic!("current patch must follow the recovered patch");
        };
        assert_eq!(visible_patch.revision(), recovered_patch.revision());
        assert!(recovered_patch.revision() < current_patch.revision());
        assert_eq!(
            timeline_patch_summary(recovered_patch.changes()),
            [("1.0".into(), Some("first".into()))]
        );
        assert_eq!(
            timeline_patch_summary(current_patch.changes()),
            [("2.0".into(), Some("second".into()))]
        );
        assert!(runtime_receiver.try_recv().is_err());

        drop(store);
        let reopened = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        assert_eq!(
            reopened
                .load_history("C1")
                .await
                .unwrap()
                .unwrap()
                .iter()
                .map(|message| (message.ts.as_str(), message.body_text()))
                .collect::<Vec<_>>(),
            vec![("2.0", "second".into()), ("1.0", "first".into())]
        );
        assert_eq!(
            workspace
                .history("C1")
                .iter()
                .map(|message| (message.ts.as_str(), message.body_text()))
                .collect::<Vec<_>>(),
            vec![("1.0", "first".into()), ("2.0", "second".into())]
        );
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn socket_thread_catalog_patch_and_notification_order_is_explicit() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-realtime-event-order-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let conversation = SlackConversation {
            id: "D1".into(),
            is_im: Some(true),
            ..Default::default()
        };
        let root = SlackMessage {
            ts: "1.0".into(),
            user: Some("U_OTHER".into()),
            text: Some("root".into()),
            ..Default::default()
        };
        store
            .store_conversations(std::slice::from_ref(&conversation))
            .await
            .unwrap();
        store
            .store_history("D1", std::slice::from_ref(&root))
            .await
            .unwrap();
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                conversations: vec![conversation],
                histories: HashMap::from([("D1".into(), vec![root])]),
                ..Default::default()
            }),
        );

        let (runtime_events, mut runtime_receiver) = runtime_event_channel();
        let event_sender = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );
        let (sender, receiver) = realtime_persistence_channel(workspace.attention_metrics_handle());
        let worker = tokio::spawn(persist_realtime_events(
            receiver,
            store,
            Some("U_SELF".into()),
            event_sender,
            workspace,
            UserStatusSync::default(),
        ));
        sender
            .send(RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "D1".into(),
                    message: SlackMessage {
                        ts: "2.0".into(),
                        thread_ts: Some("1.0".into()),
                        user: Some("U_OTHER".into()),
                        text: Some("reply".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                }),
            })
            .await
            .unwrap();
        drop(sender);
        worker.await.unwrap();

        let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        assert!(matches!(
            delivered.as_slice(),
            [
                RuntimeEventKind::WorkspacePatch(patch),
                RuntimeEventKind::AttentionNotificationCandidate { message: notified, .. },
            ] if patch.changes().iter().any(|change| matches!(
                    change,
                    WorkspaceChange::ThreadCatalogChanged(records) if !records.is_empty()
                ))
                && notified.ts == "2.0"
                && timeline_patch_summary(patch.changes())
                    .iter()
                    .any(|(message_ts, body)| {
                        message_ts == "2.0" && body.as_deref() == Some("reply")
                    })
        ));
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn local_post_cache_and_closed_socket_fallback_preserve_canonical_state() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-local-post-cache-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let (runtime_events, mut runtime_receiver) = runtime_event_channel();
        let events = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );
        let workspace = WorkspaceReducerAdapter::default();
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::Local,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: "1.0".into(),
                        user: Some("U_SELF".into()),
                        text: Some("local channel post".into()),
                        ..Default::default()
                    },
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Local,
                },
            )
            .await
            .unwrap();
        workspace
            .apply_persisted_and_publish(
                Some(&store),
                &events,
                MutationOrigin::Local,
                WorkspaceMutation::MessageChanged {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: "2.0".into(),
                        thread_ts: Some("1.0".into()),
                        user: Some("U_SELF".into()),
                        text: Some("local reply".into()),
                        ..Default::default()
                    },
                    kind: MessageMutationKind::Posted,
                    origin: MutationOrigin::Local,
                },
            )
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
            vec!["2.0"]
        );
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            }),
        );
        let fallback = crate::socket_mode::SocketModeMessageEvent {
            channel_id: "D1".into(),
            message: SlackMessage {
                ts: "3.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("queue fallback".into()),
                ..Default::default()
            },
            kind: SocketModeMessageKind::Posted,
        };
        apply_realtime_persistence_queue_fallback(
            &workspace,
            &events,
            SocketModeEvent::Message(Box::new(fallback)),
        );
        assert_eq!(workspace.history("D1")[0].ts, "3.0");
        assert_eq!(
            workspace
                .coordinator
                .lock()
                .expect("workspace coordinator lock poisoned")
                .conversation("D1")
                .map(|conversation| conversation.id.clone()),
            Some("D1".to_string())
        );
        let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        assert!(delivered.iter().any(|event| matches!(
            event,
            RuntimeEventKind::WorkspacePatch(patch)
                if timeline_patch_summary(patch.changes())
                    == [("3.0".into(), Some("queue fallback".into()))]
        )));
        assert!(delivered.iter().any(|event| matches!(
            event,
            RuntimeEventKind::AttentionNotificationCandidate { message, .. }
                if message.ts == "3.0"
        )));
        let _ = std::fs::remove_dir_all(directory);
    });
}

async fn run_realtime_message_events_for_test(
    store: WorkspaceStore,
    workspace: WorkspaceReducerAdapter,
    events: RuntimeEventSender,
    messages: Vec<crate::socket_mode::SocketModeMessageEvent>,
) {
    let (sender, receiver) = realtime_persistence_channel(workspace.attention_metrics_handle());
    let worker = tokio::spawn(persist_realtime_events(
        receiver,
        store,
        Some("U_SELF".into()),
        events,
        workspace,
        UserStatusSync::default(),
    ));
    for event in messages {
        sender
            .send(RealtimePersistenceEvent::Message {
                event: Box::new(event),
            })
            .await
            .unwrap();
    }
    drop(sender);
    worker.await.unwrap();
}

async fn assert_socket_history_admission_order(realtime_first: bool) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let order = if realtime_first {
        "realtime-first"
    } else {
        "history-first"
    };
    let directory = std::env::temp_dir().join(format!(
        "conduit-realtime-history-{order}-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
    let workspace = WorkspaceReducerAdapter::default();
    workspace.update_attention_context(WorkspaceAttentionContext {
        current_user_id: Some("U_SELF".into()),
    });
    let (runtime_events, mut runtime_receiver) = runtime_event_channel();
    let events = RuntimeEventSender::new(
        runtime_events,
        RuntimeIdentity {
            session: SessionId::default().next(),
            request: RequestId::new(1),
        },
        OperationContext::new(
            RuntimeOperation::History,
            RuntimeTarget::Channel("C1".into()),
        ),
    );

    let original_edit = SlackMessage {
        ts: "1.0".into(),
        client_msg_id: Some("edit".into()),
        user: Some("U_OTHER".into()),
        text: Some("stale edit".into()),
        ..Default::default()
    };
    let original_deleted = SlackMessage {
        ts: "2.0".into(),
        client_msg_id: Some("delete".into()),
        user: Some("U_OTHER".into()),
        text: Some("stale delete".into()),
        ..Default::default()
    };
    let original_moved = SlackMessage {
        ts: "3.0".into(),
        client_msg_id: Some("move".into()),
        user: Some("U_OTHER".into()),
        text: Some("stale location".into()),
        ..Default::default()
    };
    let stale_history = vec![
        original_moved.clone(),
        original_deleted.clone(),
        original_edit.clone(),
    ];
    workspace
        .apply_persisted_and_publish(
            Some(&store),
            &events,
            MutationOrigin::WebApi,
            WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                conversations: vec![SlackConversation {
                    id: "C1".into(),
                    ..Default::default()
                }],
                histories: HashMap::from([("C1".into(), stale_history.clone())]),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert!(matches!(
        runtime_receiver.recv().await.unwrap().kind,
        RuntimeEventKind::WorkspacePatch(_)
    ));
    let history_base = workspace.revision();

    let authoritative_edit = SlackMessage {
        text: Some("authoritative edit".into()),
        ..original_edit.clone()
    };
    let moved_to_thread = SlackMessage {
        thread_ts: Some("9.0".into()),
        text: Some("authoritative thread location".into()),
        ..original_moved.clone()
    };
    let concurrent_post = SlackMessage {
        ts: "4.0".into(),
        user: Some("U_OTHER".into()),
        text: Some("concurrent post".into()),
        ..Default::default()
    };
    let socket_events = vec![
        crate::socket_mode::SocketModeMessageEvent {
            channel_id: "C1".into(),
            message: authoritative_edit,
            kind: SocketModeMessageKind::Changed,
        },
        crate::socket_mode::SocketModeMessageEvent {
            channel_id: "C1".into(),
            message: original_deleted,
            kind: SocketModeMessageKind::Deleted,
        },
        crate::socket_mode::SocketModeMessageEvent {
            channel_id: "C1".into(),
            message: moved_to_thread,
            kind: SocketModeMessageKind::Changed,
        },
        crate::socket_mode::SocketModeMessageEvent {
            channel_id: "C1".into(),
            message: concurrent_post,
            kind: SocketModeMessageKind::Posted,
        },
    ];
    let workspace_store = Some(store.clone());
    if realtime_first {
        run_realtime_message_events_for_test(
            store.clone(),
            workspace.clone(),
            events.clone(),
            socket_events.clone(),
        )
        .await;
    }
    publish_history_snapshot_with_completion(
        &events,
        &workspace_store,
        &workspace,
        "C1",
        MutationOrigin::WebApi,
        history_base,
        stale_history,
        false,
        None,
        true,
        false,
        false,
    )
    .await
    .unwrap();
    if !realtime_first {
        run_realtime_message_events_for_test(
            store.clone(),
            workspace.clone(),
            events,
            socket_events,
        )
        .await;
    }

    let stored = store.load_history("C1").await.unwrap().unwrap();
    assert_eq!(
        stored
            .iter()
            .map(|message| (message.ts.as_str(), message.body_text()))
            .collect::<Vec<_>>(),
        vec![
            ("4.0", "concurrent post".into()),
            ("1.0", "authoritative edit".into()),
        ]
    );
    assert_eq!(
        store
            .load_thread("C1", "9.0")
            .await
            .unwrap()
            .unwrap()
            .iter()
            .map(|message| (message.ts.as_str(), message.body_text()))
            .collect::<Vec<_>>(),
        vec![("3.0", "authoritative thread location".into())]
    );
    assert_eq!(
        workspace
            .history("C1")
            .iter()
            .map(|message| (message.ts.as_str(), message.body_text()))
            .collect::<Vec<_>>(),
        vec![
            ("1.0", "authoritative edit".into()),
            ("4.0", "concurrent post".into()),
        ]
    );
    let delivered_timeline_changes = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
        .flat_map(|event| match event.kind {
            RuntimeEventKind::WorkspacePatch(patch) => timeline_patch_summary(patch.changes()),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        delivered_timeline_changes,
        [
            ("1.0".into(), Some("authoritative edit".into())),
            ("2.0".into(), None),
            ("3.0".into(), Some("authoritative thread location".into())),
            ("3.0".into(), None),
            ("4.0".into(), Some("concurrent post".into())),
        ]
    );
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn socket_and_history_both_admission_orders_preserve_edit_delete_move_and_post() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(async {
        assert_socket_history_admission_order(false).await;
        assert_socket_history_admission_order(true).await;
    });
}

#[test]
fn realtime_persistence_worker_drains_events_in_session_scope() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-realtime-persistence-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        store
            .store_conversations(&[crate::models::SlackConversation {
                id: "C1".into(),
                ..Default::default()
            }])
            .await
            .unwrap();
        let (runtime_events, _receiver) = runtime_event_channel();
        let event_sender = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        let (sender, receiver) = realtime_persistence_channel(workspace.attention_metrics_handle());
        let worker = tokio::spawn(persist_realtime_events(
            receiver,
            store.clone(),
            Some("U_SELF".into()),
            event_sender,
            workspace.clone(),
            UserStatusSync::default(),
        ));
        for ts in ["1.0", "2.0"] {
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                        channel_id: "C1".into(),
                        message: SlackMessage {
                            ts: ts.into(),
                            user: Some("U_OTHER".into()),
                            text: Some(format!("message {ts}")),
                            ..Default::default()
                        },
                        kind: SocketModeMessageKind::Posted,
                    }),
                })
                .await
                .unwrap();
        }
        sender
            .send(RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: "3.0".into(),
                        thread_ts: Some("1.0".into()),
                        user: Some("U_OTHER".into()),
                        text: Some("thread reply".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                }),
            })
            .await
            .unwrap();
        drop(sender);
        worker.await.unwrap();

        let history = store.load_history("C1").await.unwrap().unwrap();
        assert_eq!(history.len(), 2);
        let thread = store.load_thread("C1", "1.0").await.unwrap().unwrap();
        assert_eq!(thread.len(), 1);
        assert_eq!(thread[0].ts, "3.0");
        let conversation = store.load_conversations().await.unwrap().unwrap().remove(0);
        assert_eq!(conversation.id, "C1");
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn realtime_persistence_preserves_socket_event_order() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-realtime-message-order-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        store
            .store_conversations(&[SlackConversation {
                id: "C1".into(),
                ..Default::default()
            }])
            .await
            .unwrap();
        let (runtime_events, mut runtime_receiver) = runtime_event_channel();
        let event_sender = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        let (sender, receiver) = realtime_persistence_channel(workspace.attention_metrics_handle());
        let worker = tokio::spawn(persist_realtime_events(
            receiver,
            store,
            Some("U_SELF".into()),
            event_sender,
            workspace.clone(),
            UserStatusSync::default(),
        ));
        sender
            .send(RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: "1.0".into(),
                        user: Some("U_OTHER".into()),
                        text: Some("message".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                }),
            })
            .await
            .unwrap();
        sender
            .send(RealtimePersistenceEvent::OrderedEvent {
                event: SocketModeEvent::Reaction(socket_mode::SocketModeReactionEvent {
                    channel_id: "C1".into(),
                    ts: "1.0".into(),
                    name: "wave".into(),
                    user_id: "U_OTHER".into(),
                    added: true,
                }),
            })
            .await
            .unwrap();
        sender
            .send(RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "C1".into(),
                    message: SlackMessage {
                        ts: "1.0".into(),
                        user: Some("U_OTHER".into()),
                        text: Some("message".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Deleted,
                }),
            })
            .await
            .unwrap();
        drop(sender);
        worker.await.unwrap();

        let delivered = std::iter::from_fn(|| runtime_receiver.try_recv().ok())
            .flat_map(|event| match event.kind {
                RuntimeEventKind::WorkspacePatch(patch) => timeline_patch_summary(patch.changes()),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            delivered,
            [
                ("1.0".into(), Some("message".into())),
                ("1.0".into(), Some("message".into())),
                ("1.0".into(), None),
            ]
        );
        let coordinator = workspace
            .coordinator
            .lock()
            .expect("workspace coordinator lock poisoned");
        assert!(coordinator.history("C1").is_empty());
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn realtime_persistence_claims_each_notification_once() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-realtime-notification-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        store
            .store_conversations(&[SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            }])
            .await
            .unwrap();
        let (runtime_events, mut receiver) = runtime_event_channel();
        let event_sender = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        let (sender, persistence_receiver) =
            realtime_persistence_channel(workspace.attention_metrics_handle());
        let worker = tokio::spawn(persist_realtime_events(
            persistence_receiver,
            store.clone(),
            Some("U_SELF".into()),
            event_sender,
            workspace.clone(),
            UserStatusSync::default(),
        ));
        let event = crate::socket_mode::SocketModeMessageEvent {
            channel_id: "D1".into(),
            message: SlackMessage {
                ts: "1.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("hello".into()),
                ..Default::default()
            },
            kind: SocketModeMessageKind::Posted,
        };
        for _ in 0..2 {
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(event.clone()),
                })
                .await
                .unwrap();
        }
        drop(sender);
        worker.await.unwrap();

        let mut notifications = 0;
        while let Ok(event) = receiver.try_recv() {
            if matches!(
                event.kind,
                RuntimeEventKind::AttentionNotificationCandidate { .. }
            ) {
                notifications += 1;
            }
        }
        assert_eq!(notifications, 1);
        let metrics = workspace.attention_metrics_snapshot();
        assert_eq!(
            metrics.persistence_count(AttentionPersistenceOutcome::Accepted),
            1
        );
        assert_eq!(
            metrics.persistence_count(AttentionPersistenceOutcome::AlreadyObserved),
            1
        );
        assert_eq!(metrics.notification_claims, 1);
        assert_eq!(metrics.queue_enqueued, 2);
        assert_eq!(metrics.queue_dequeued, 2);
        assert_eq!(metrics.queue_depth, 0);
        assert_eq!(metrics.queue_peak_depth, 2);
        let conversation = store
            .load_conversations()
            .await
            .unwrap()
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(conversation.id, "D1");
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn realtime_attention_persistence_failure_queues_delta_and_publishes_typed_patch() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let invalid_directory = std::env::temp_dir().join(format!(
            "conduit-invalid-attention-store-{}-{nonce}",
            std::process::id()
        ));
        std::fs::write(&invalid_directory, b"not a directory").unwrap();
        let store = WorkspaceStore::new(invalid_directory.clone(), "T1:U_SELF");
        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            }),
        );
        let (runtime_events, mut receiver) = runtime_event_channel();
        let event_sender = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );
        let (sender, persistence_receiver) =
            realtime_persistence_channel(workspace.attention_metrics_handle());
        let worker = tokio::spawn(persist_realtime_events(
            persistence_receiver,
            store,
            Some("U_SELF".into()),
            event_sender,
            workspace.clone(),
            UserStatusSync::default(),
        ));
        sender
            .send(RealtimePersistenceEvent::Message {
                event: Box::new(crate::socket_mode::SocketModeMessageEvent {
                    channel_id: "D1".into(),
                    message: SlackMessage {
                        ts: "1.0".into(),
                        user: Some("U_OTHER".into()),
                        text: Some("deferred".into()),
                        ..Default::default()
                    },
                    kind: SocketModeMessageKind::Posted,
                }),
            })
            .await
            .unwrap();
        drop(sender);
        worker.await.unwrap();

        let metrics = workspace.attention_metrics_snapshot();
        assert_eq!(
            metrics.persistence_count(AttentionPersistenceOutcome::Failed),
            1
        );
        assert_eq!(metrics.committed_decisions, 1);
        assert_eq!(metrics.delivery_count(DeliveryState::Fresh), 1);
        assert_eq!(metrics.notification_claims, 0);
        assert_eq!(metrics.queue_enqueued, 1);
        assert_eq!(metrics.queue_dequeued, 1);
        assert_eq!(metrics.queue_depth, 0);
        assert_eq!(workspace.history("D1").len(), 1);
        assert_eq!(
            workspace
                .coordinator
                .lock()
                .expect("workspace coordinator lock poisoned")
                .conversation("D1")
                .map(|conversation| conversation.id.clone()),
            Some("D1".to_string())
        );
        {
            let pending = workspace
                .pending_writes
                .lock()
                .expect("pending workspace writes lock poisoned");
            assert_eq!(pending.len(), 1);
            let changes = pending[0].batch.as_ref().unwrap().changes();
            assert!(changes.iter().any(|change| matches!(
                change,
                StoreChange::MessageDelta {
                    channel_id,
                    message,
                    kind: MessageMutationKind::Posted,
                } if channel_id == "D1" && message.ts == "1.0"
            )));
        }
        let event = receiver.recv().await.unwrap();
        assert!(matches!(event.kind, RuntimeEventKind::WorkspacePatch(patch)
            if timeline_patch_summary(patch.changes())
                == [("1.0".into(), Some("deferred".into()))]));
        assert!(
            receiver.try_recv().is_err(),
            "failed persistence must not emit duplicate projections"
        );
        std::fs::remove_file(invalid_directory).unwrap();
    });
}

#[test]
#[ignore = "release-mode realtime persistence measurement"]
fn realtime_attention_persistence_burst_measurement() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-realtime-attention-burst-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T1:U_SELF");
        let conversations = vec![
            SlackConversation {
                id: "C1".into(),
                ..Default::default()
            },
            SlackConversation {
                id: "D1".into(),
                is_im: Some(true),
                ..Default::default()
            },
        ];
        store.store_conversations(&conversations).await.unwrap();

        let workspace = WorkspaceReducerAdapter::default();
        workspace.update_attention_context(WorkspaceAttentionContext {
            current_user_id: Some("U_SELF".into()),
        });
        workspace.update_attention_preferences(AttentionPreferences {
            keywords: vec!["priority phrase".into()],
            ..AttentionPreferences::default()
        });
        for conversation in conversations {
            workspace.apply(
                MutationOrigin::Cache,
                WorkspaceMutation::ConversationUpsert(conversation),
            );
        }
        let thread_root = SlackMessage {
            ts: "0.000001".into(),
            user: Some("U_OTHER".into()),
            text: Some("thread root".into()),
            reply_count: Some(1),
            reply_users: Some(vec!["U_SELF".into()]),
            ..Default::default()
        };
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::HistorySnapshot {
                channel_id: "C1".into(),
                snapshot: SnapshotEnvelope::new(
                    workspace.revision(),
                    crate::workspace_pipeline::MessagePage {
                        messages: vec![thread_root.clone()],
                        complete: false,
                        ..Default::default()
                    },
                ),
            },
        );
        let baseline = workspace.attention_metrics_snapshot();

        let (runtime_events, mut receiver) = runtime_event_channel();
        let event_sender = RuntimeEventSender::new(
            runtime_events,
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace),
        );
        let event_collector = tokio::spawn(async move {
            let mut notification_events = 0;
            let mut message_patches = 0;
            while let Some(event) = receiver.recv().await {
                match event.kind {
                    RuntimeEventKind::AttentionNotificationCandidate { .. } => {
                        notification_events += 1;
                    }
                    RuntimeEventKind::WorkspacePatch(patch)
                        if !timeline_patch_summary(patch.changes()).is_empty() =>
                    {
                        message_patches += 1;
                    }
                    _ => {}
                }
            }
            (notification_events, message_patches, receiver.snapshot())
        });
        let (sender, persistence_receiver) =
            realtime_persistence_channel(workspace.attention_metrics_handle());
        let started = Instant::now();
        let worker = tokio::spawn(persist_realtime_events(
            persistence_receiver,
            store.clone(),
            Some("U_SELF".into()),
            event_sender,
            workspace.clone(),
            UserStatusSync::default(),
        ));
        tokio::task::yield_now().await;
        let mut channel_messages = Vec::with_capacity(700);
        let mut direct_messages = Vec::with_capacity(200);
        let mut thread_messages = Vec::with_capacity(100);
        let mut duplicate_direct_messages = Vec::with_capacity(200);
        let mut persistence_events = Vec::with_capacity(1_200);
        {
            let mut enqueue = |channel_id: &str,
                               index: usize,
                               text: &str,
                               subtype: Option<&str>,
                               thread_ts: Option<&str>| {
                let message = SlackMessage {
                    ts: format!("{index}.000000"),
                    user: Some("U_OTHER".into()),
                    text: Some(text.into()),
                    subtype: subtype.map(str::to_string),
                    thread_ts: thread_ts.map(str::to_string),
                    ..Default::default()
                };
                if channel_id == "D1" {
                    direct_messages.push(message.clone());
                } else if thread_ts.is_some() {
                    thread_messages.push(message.clone());
                } else {
                    channel_messages.push(message.clone());
                }
                let event = crate::socket_mode::SocketModeMessageEvent {
                    channel_id: channel_id.into(),
                    message,
                    kind: SocketModeMessageKind::Posted,
                };
                if channel_id == "D1" {
                    duplicate_direct_messages.push(event.clone());
                }
                persistence_events.push(event);
            };

            for index in 1..=400 {
                enqueue("C1", index, "ordinary channel update", None, None);
            }
            for index in 401..=600 {
                enqueue("D1", index, "direct conversation update", None, None);
            }
            for index in 601..=700 {
                enqueue("C1", index, "explicit <@U_SELF> update", None, None);
            }
            for index in 701..=800 {
                enqueue("C1", index, "configured priority phrase", None, None);
            }
            for index in 801..=900 {
                enqueue(
                    "C1",
                    index,
                    "participated thread update",
                    None,
                    Some("0.000001"),
                );
            }
            for index in 901..=1000 {
                enqueue("C1", index, "membership update", Some("channel_join"), None);
            }
        }
        persistence_events.extend(duplicate_direct_messages);
        for event in persistence_events {
            sender
                .send(RealtimePersistenceEvent::Message {
                    event: Box::new(event),
                })
                .await
                .unwrap();
        }
        drop(sender);

        worker.await.unwrap();
        let drain_milliseconds = started.elapsed().as_millis();

        let metrics = workspace
            .attention_metrics_snapshot()
            .delta_since(&baseline);
        assert_eq!(metrics.committed_decisions, 1_000);
        assert_eq!(metrics.notification_candidates, 500);
        assert_eq!(
            metrics.reason_count(AttentionReason::MembershipLifecycle),
            100
        );
        assert_eq!(metrics.origin_count(MutationOrigin::Realtime), 1_000);
        assert_eq!(metrics.delivery_count(DeliveryState::Fresh), 1_000);
        assert_eq!(
            metrics.persistence_count(AttentionPersistenceOutcome::Accepted),
            1_000
        );
        assert_eq!(
            metrics.persistence_count(AttentionPersistenceOutcome::AlreadyObserved),
            200
        );
        assert_eq!(metrics.notification_claims, 500);
        assert_eq!(metrics.queue_enqueued, 1_200);
        assert_eq!(metrics.queue_dequeued, 1_200);
        assert_eq!(metrics.queue_depth, 0);
        let queue_peak = metrics.queue_peak_depth;
        assert!((1..=REALTIME_PERSISTENCE_QUEUE_CAPACITY as u64).contains(&queue_peak));
        assert_eq!(metrics.queue_rejected, 0);

        let (notification_events, message_patches, event_metrics) = event_collector.await.unwrap();
        assert_eq!(notification_events, 500);
        assert_eq!(message_patches, 1_000);
        assert_eq!(event_metrics.admitted, 1_500);
        assert_eq!(event_metrics.dequeued, 1_500);
        assert_eq!(event_metrics.depth, 0);
        assert!(event_metrics.peak_depth <= RUNTIME_EVENT_QUEUE_CAPACITY);
        assert_eq!(event_metrics.dropped_progress, 0);

        eprintln!(
            "attention_realtime_burst ingress=1200 unique=1000 drain_ms={drain_milliseconds} \
             queue_peak={queue_peak} persistence_accepted=1000 already_observed=200 \
             notification_claims=500"
        );
        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn replacing_session_aborts_registered_session_tasks() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let first_session = SessionId::default().next();
        let second_session = first_session.next();
        let state = Arc::new(Mutex::new(RuntimeState::new(first_session)));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();

        spawn_session_task(&state, first_session, async move {
            let _signal = CancellationSignal(Some(cancelled_tx));
            let _ = started_tx.send(());
            future::pending::<()>().await;
        });
        started_rx.await.expect("session task did not start");

        replace_session_and_drain(&state, second_session).await;

        tokio::time::timeout(Duration::from_millis(100), cancelled_rx)
            .await
            .expect("old session task was not aborted")
            .expect("cancellation signal was dropped");
        assert_eq!(
            state
                .lock()
                .expect("runtime state lock poisoned")
                .active_session,
            second_session
        );
    });
}

#[test]
fn realtime_wait_prioritizes_shutdown_over_ready_work() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let (shutdown, mut shutdown_receiver) = oneshot::channel();
        shutdown.send(()).unwrap();

        assert_eq!(
            wait_for_realtime_or_shutdown(&mut shutdown_receiver, future::ready(42)).await,
            None
        );
    });
}

#[test]
fn session_replacement_marks_new_session_before_realtime_drain_completes() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let first_session = SessionId::default().next();
        let second_session = first_session.next();
        let state = Arc::new(Mutex::new(RuntimeState::new(first_session)));
        let (huddle_events, mut huddle_event_receiver) = huddle_test_event_channel();
        let (huddle_handle, huddle_receiver) = huddle_actor_channel();
        let huddle_supervisor = HuddleSessionSupervisor::spawn(
            first_session,
            huddle_receiver,
            huddle_events,
            production_native_join_capability(false),
        );
        assert!(state
            .lock()
            .expect("runtime state lock poisoned")
            .install_huddle_supervisor(huddle_supervisor)
            .is_ok());
        let (started, mut started_receiver) = oneshot::channel();
        let (drain_started, drain_started_receiver) = oneshot::channel();
        let (release_drain, release_drain_receiver) = oneshot::channel();
        let (drained, drained_receiver) = oneshot::channel();
        let supervisor =
            RealtimeSessionSupervisor::spawn(first_session, move |shutdown| async move {
                let _ = started.send(());
                let _ = shutdown.await;
                let _ = drain_started.send(());
                let _ = release_drain_receiver.await;
                let _ = drained.send(());
            });

        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut started_receiver)
                .await
                .is_err(),
            "realtime supervisor started before installation"
        );
        assert!(state
            .lock()
            .expect("runtime state lock poisoned")
            .install_realtime_supervisor(supervisor)
            .is_ok());
        started_receiver
            .await
            .expect("installed realtime supervisor did not start");

        let state_for_replacement = Arc::clone(&state);
        let replacement = tokio::spawn(async move {
            replace_session_and_drain(&state_for_replacement, second_session).await;
        });
        drain_started_receiver
            .await
            .expect("realtime drain did not start");

        assert_eq!(
            state
                .lock()
                .expect("runtime state lock poisoned")
                .active_session,
            second_session
        );
        assert!(
            !replacement.is_finished(),
            "session replacement completed before realtime drain"
        );
        huddle_handle
            .observe_huddle(test_huddle("R_DRAIN"))
            .await
            .expect("huddle actor closed before realtime drained");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), huddle_event_receiver.recv())
                .await
                .expect("huddle actor did not process during realtime drain")
                .unwrap()
                .kind,
            RuntimeEventKind::Huddle(HuddleEvent::Snapshot(snapshot))
                if snapshot.phase == HuddlePhase::Discovered
        ));

        let (old_task_started, old_task_started_receiver) = oneshot::channel();
        spawn_session_task(&state, first_session, async move {
            let _ = old_task_started.send(());
        });
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(100), old_task_started_receiver).await,
            Ok(Err(_))
        ));

        release_drain.send(()).unwrap();
        replacement.await.unwrap();
        drained_receiver
            .await
            .expect("realtime drain did not finish");
        assert!(huddle_handle
            .observe_huddle(test_huddle("R_CLOSED"))
            .await
            .is_err());
        assert!(matches!(
            huddle_event_receiver.recv().await.unwrap().kind,
            RuntimeEventKind::Huddle(HuddleEvent::Snapshot(snapshot))
                if snapshot.phase == HuddlePhase::Idle
        ));
        assert!(state
            .lock()
            .expect("runtime state lock poisoned")
            .realtime
            .is_none());
        assert!(state
            .lock()
            .expect("runtime state lock poisoned")
            .huddle
            .is_none());
    });
}

#[test]
fn stale_realtime_supervisor_is_rejected_before_start() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        let first_session = SessionId::default().next();
        let second_session = first_session.next();
        let state = Arc::new(Mutex::new(RuntimeState::new(second_session)));
        let (started, started_receiver) = oneshot::channel();
        let supervisor =
            RealtimeSessionSupervisor::spawn(first_session, move |_shutdown| async move {
                let _ = started.send(());
            });

        let rejected = {
            let mut state = state.lock().expect("runtime state lock poisoned");
            match state.install_realtime_supervisor(supervisor) {
                Ok(()) => panic!("stale realtime supervisor was installed"),
                Err(rejected) => rejected,
            }
        };
        rejected.shutdown().await;

        assert!(started_receiver.await.is_err());
        assert!(state
            .lock()
            .expect("runtime state lock poisoned")
            .realtime
            .is_none());
    });
}

#[test]
fn latest_attention_preferences_seed_new_sessions_without_a_connection() {
    let first_session = SessionId::default().next();
    let second_session = first_session.next();
    let mut state = RuntimeState::new(first_session);
    let first = AttentionPreferences {
        direct_messages: false,
        ..AttentionPreferences::default()
    };
    let latest = AttentionPreferences {
        desktop_notifications: false,
        direct_messages: false,
        names_and_aliases: vec!["Vincent".to_string()],
        keywords: vec!["incident review".to_string()],
        ..AttentionPreferences::default()
    };

    state.set_attention_preferences(first);
    state.set_attention_preferences(latest.clone());
    assert_eq!(state.attention_preferences, latest);

    let supervisors = state.begin_session_replacement(second_session);
    assert!(supervisors.realtime.is_none());
    assert!(supervisors.huddle.is_none());
    let seeded = state.attention_context(Some("U_NEXT".to_string()));
    assert_eq!(seeded.current_user_id.as_deref(), Some("U_NEXT"));
    assert_eq!(state.attention_preferences, latest);
}

#[test]
fn connected_attention_update_changes_runtime_and_coordinator_together() {
    let session = SessionId::default().next();
    let workspace = WorkspaceReducerAdapter::default();
    workspace.update_attention_context(WorkspaceAttentionContext {
        current_user_id: Some("U_SELF".into()),
    });
    workspace.apply(
        MutationOrigin::Cache,
        WorkspaceMutation::ConversationUpsert(SlackConversation {
            id: "D1".into(),
            is_channel: Some(false),
            is_im: Some(true),
            ..Default::default()
        }),
    );
    let (huddles, _huddle_receiver) = huddle_actor_channel();
    let mut state = RuntimeState::new(session);
    state.connection = Some(RuntimeConnection {
        slack: SlackApi::new(StoredToken {
            access_token: "test-token".into(),
            token_type: None,
            scope: None,
            refresh_token: None,
            expires_in: None,
            expires_at: None,
            team_id: None,
            team_name: None,
            user_id: Some("U_SELF".into()),
            client_id: None,
            browser_cookie_d: None,
            user_agent: None,
        }),
        workspace_url: None,
        workspace_store: None,
        image_cache_scope: "test-workspace".to_string(),
        workspace: workspace.clone(),
        current_user_id: Some("U_SELF".into()),
        user_cache: Arc::new(Mutex::new(HashMap::new())),
        message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(256))),
        conversation_star_sync: ConversationStarSyncGate::default(),
        user_status_sync: UserStatusSync::default(),
        team_id: None,
        huddles,
        read_flusher: ReadFlusherHandle::new(
            Arc::new(Mutex::new(ReadFlusherQueue::new())),
            Arc::new(Notify::new()),
        ),
        scheduler: Arc::new(Mutex::new(SyncScheduler::new(
            SchedulerConfig::new(256, 8, 5).unwrap(),
        ))),
        pending_jobs: Arc::new(Mutex::new(HashMap::new())),
        next_job_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        cached_bootstrap_load_gate: None,
    });
    let disabled = AttentionPreferences {
        direct_messages: false,
        ..AttentionPreferences::default()
    };

    state.set_attention_preferences(disabled.clone());

    assert_eq!(state.attention_preferences, disabled);
    let effect = workspace
        .preview_message_attention(
            "D1",
            &SlackMessage {
                ts: "1.0".into(),
                user: Some("U_OTHER".into()),
                text: Some("hello".into()),
                ..Default::default()
            },
            MessageMutationKind::Posted,
            MutationOrigin::Realtime,
        )
        .expect("direct message should still be classified");
    assert!(!effect.decision.send_notification);
}

#[test]
fn image_asset_cache_key_is_stable_hex_digest() {
    assert_eq!(
        image_asset_cache_key("https://files.example/image.png"),
        "7db09e79cb28f1be72da3c1449cd42619e048f148310325cc2c8f55cd713aa0e"
    );
}

#[test]
fn attachment_cache_path_is_stable_and_sanitizes_remote_filename() {
    let path = attachment_cache_path(
        "https://files.slack.com/files-pri/F1/download/report",
        "../../Quarterly: report?.pdf",
    );
    let filename = path.file_name().and_then(|name| name.to_str()).unwrap();

    assert!(path.starts_with(config::attachment_cache_dir()));
    assert!(filename.ends_with("-Quarterly_ report_.pdf"));
    assert!(!filename.contains('/'));
    assert!(!filename.contains(".."));
    assert_eq!(
        path,
        attachment_cache_path(
            "https://files.slack.com/files-pri/F1/download/report",
            "../../Quarterly: report?.pdf",
        )
    );
}

#[test]
fn attachment_cache_filename_stays_within_a_byte_safe_component_limit() {
    let name = format!("{}.pdf", "é".repeat(200));
    let path = attachment_cache_path("https://files.slack.com/long-name", &name);
    let filename = path.file_name().and_then(|name| name.to_str()).unwrap();
    let basename = filename.split_once('-').unwrap().1;

    assert!(basename.len() <= ATTACHMENT_BASENAME_MAX_BYTES);
    assert!(filename.len() <= 64 + 1 + ATTACHMENT_BASENAME_MAX_BYTES);
    assert!(basename.is_char_boundary(basename.len()));
}

#[test]
fn attachment_cache_prunes_expired_files_but_preserves_active_download() {
    let directory = std::env::temp_dir().join(format!(
        "conduit-attachment-age-test-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let expired = directory.join("expired");
    let protected = directory.join("protected");
    std::fs::write(&expired, b"old").unwrap();
    std::fs::write(&protected, b"active").unwrap();

    prune_attachment_cache(
        &directory,
        Some(&protected),
        AttachmentCachePolicy {
            max_age: Duration::from_secs(5),
            max_bytes: u64::MAX,
        },
        SystemTime::now() + Duration::from_secs(10),
    )
    .unwrap();

    assert!(!expired.exists());
    assert!(protected.exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn attachment_cache_evicts_to_size_cap_without_removing_active_download() {
    let directory = std::env::temp_dir().join(format!(
        "conduit-attachment-size-test-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let protected = directory.join("protected");
    let partial = directory.join("concurrent.123.part");
    std::fs::write(directory.join("first"), b"1111").unwrap();
    std::fs::write(directory.join("second"), b"2222").unwrap();
    std::fs::write(&protected, b"3333").unwrap();
    std::fs::write(&partial, b"download in progress").unwrap();

    prune_attachment_cache(
        &directory,
        Some(&protected),
        AttachmentCachePolicy {
            max_age: Duration::MAX,
            max_bytes: 7,
        },
        SystemTime::now(),
    )
    .unwrap();

    let retained_size = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "part")
        })
        .map(|entry| entry.metadata().unwrap().len())
        .sum::<u64>();
    assert!(protected.exists());
    assert!(partial.exists());
    assert!(retained_size <= 7);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn completed_upload_cleanup_removes_only_owned_staged_files() {
    let owned = std::env::temp_dir().join(format!(
        "conduit-upload-cleanup-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let source = owned.with_extension("source");
    std::fs::write(&owned, b"screenshot").unwrap();
    std::fs::write(&source, b"source").unwrap();

    remove_completed_upload_files(&[
        UploadAttachment {
            path: owned.clone(),
            remove_after_upload: true,
        },
        UploadAttachment {
            path: source.clone(),
            remove_after_upload: false,
        },
    ]);

    assert!(!owned.exists());
    assert!(source.exists());
    std::fs::remove_file(source).unwrap();
}

#[test]
fn workspace_store_id_uses_team_and_user_identity() {
    let auth = AuthInfo {
        team: Some("Example".to_string()),
        team_id: Some("T123".to_string()),
        user_id: Some("U123".to_string()),
        ..Default::default()
    };

    assert_eq!(workspace_store_id(&auth), "T123:U123");
    assert_eq!(preview_workspace_scope(&auth), "T123:U123");
}

#[test]
fn preview_workspace_scope_uses_stable_trimmed_fallbacks() {
    let auth = AuthInfo {
        team: Some(" Ignored team ".to_string()),
        team_id: Some("   ".to_string()),
        url: Some(" https://example.slack.com ".to_string()),
        user: Some(" Ada ".to_string()),
        user_id: Some("  ".to_string()),
    };

    assert_eq!(
        preview_workspace_scope(&auth),
        "https://example.slack.com:Ada"
    );
    assert_eq!(
        preview_workspace_scope(&AuthInfo::default()),
        "unknown-team:unknown-user"
    );
}

#[test]
fn runtime_command_context_identifies_operation_and_target() {
    assert_eq!(
        RuntimeCommand::SearchMessages {
            query: "from:ada".to_string(),
        }
        .operation_context(),
        OperationContext::new(RuntimeOperation::Search, RuntimeTarget::Workspace)
    );
    assert_eq!(
        RuntimeCommand::LoadThread {
            channel_id: "C123".to_string(),
            ts: "1710000000.000001".to_string(),
        }
        .operation_context(),
        OperationContext::new(
            RuntimeOperation::Thread,
            RuntimeTarget::Thread {
                channel_id: "C123".to_string(),
                thread_ts: "1710000000.000001".to_string(),
            },
        )
    );
    assert_eq!(
        RuntimeCommand::DiscoverConversations.operation_context(),
        OperationContext::new(
            RuntimeOperation::ConversationDiscovery,
            RuntimeTarget::Workspace,
        )
    );
    assert_eq!(
        RuntimeCommand::OpenDirectMessage {
            user_id: "U123".to_string(),
        }
        .operation_context(),
        OperationContext::new(
            RuntimeOperation::OpenConversation,
            RuntimeTarget::User("U123".to_string()),
        )
    );
    assert_eq!(
        RuntimeCommand::LoadFile {
            file_id: "F123".to_string(),
            share_requested: false,
        }
        .operation_context(),
        OperationContext::new(
            RuntimeOperation::Files,
            RuntimeTarget::File("F123".to_string()),
        )
    );
    assert_eq!(
        RuntimeCommand::Huddle(crate::huddles::state::HuddleCommand::OpenPreflight {
            call_id: "R123".to_string(),
        })
        .operation_context(),
        OperationContext::new(
            RuntimeOperation::Huddle,
            RuntimeTarget::Huddle("R123".to_string()),
        )
    );
    assert_eq!(
        RuntimeCommand::ResolveMessagePermalink {
            channel_id: "C123".to_string(),
            ts: "1710000000.000100".to_string(),
        }
        .operation_context(),
        OperationContext::new(
            RuntimeOperation::MessagePermalink,
            RuntimeTarget::ExactMessage {
                channel_id: "C123".to_string(),
                ts: "1710000000.000100".to_string(),
            },
        )
    );

    let channel_context = RuntimeCommand::LoadMessageContext(
        SearchMessageLocation::new("C123", "1710000000.000100", None).unwrap(),
    );
    assert_eq!(
        channel_context.operation_context(),
        OperationContext::new(
            RuntimeOperation::History,
            RuntimeTarget::Channel("C123".to_string()),
        )
    );
    assert_eq!(
        channel_context.navigation_slot(),
        Some(NavigationSlot::Main)
    );

    let thread_context = RuntimeCommand::LoadMessageContext(
        SearchMessageLocation::new("C123", "1710000001.000100", Some("1710000000.000100")).unwrap(),
    );
    assert_eq!(
        thread_context.operation_context(),
        OperationContext::new(
            RuntimeOperation::Thread,
            RuntimeTarget::Thread {
                channel_id: "C123".to_string(),
                thread_ts: "1710000000.000100".to_string(),
            },
        )
    );
    assert_eq!(
        thread_context.navigation_slot(),
        Some(NavigationSlot::Thread)
    );
}

fn expected_runtime_admission(command: &RuntimeCommand) -> RuntimeAdmissionPolicy {
    match command {
        RuntimeCommand::LoadStoredToken
        | RuntimeCommand::StartOAuth { .. }
        | RuntimeCommand::StartBrowserSession { .. } => {
            RuntimeAdmissionPolicy::supersedable(RuntimeAdmissionKey::Authentication)
        }
        RuntimeCommand::SignOut | RuntimeCommand::Disconnect => RuntimeAdmissionPolicy::control(),
        RuntimeCommand::RefreshConversations => {
            RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::WorkspaceRefresh)
        }
        RuntimeCommand::DiscoverConversations => RuntimeAdmissionPolicy::coalescible(
            RuntimeAdmissionKey::ConversationDiscovery(ConversationDiscoveryScope::Full),
        ),
        RuntimeCommand::DiscoverChannels => RuntimeAdmissionPolicy::coalescible(
            RuntimeAdmissionKey::ConversationDiscovery(ConversationDiscoveryScope::Channels),
        ),
        RuntimeCommand::LoadHistory { .. }
        | RuntimeCommand::LoadOlderHistory { .. }
        | RuntimeCommand::SearchMessages { .. }
        | RuntimeCommand::LoadFiles
        | RuntimeCommand::LoadFile { .. }
        | RuntimeCommand::LoadSavedItems
        | RuntimeCommand::LoadFollowedThreads { .. } => RuntimeAdmissionPolicy::supersedable(
            RuntimeAdmissionKey::Navigation(NavigationSlot::Main),
        ),
        RuntimeCommand::LoadThread { .. } | RuntimeCommand::LoadOlderThread { .. } => {
            RuntimeAdmissionPolicy::supersedable(RuntimeAdmissionKey::Navigation(
                NavigationSlot::Thread,
            ))
        }
        RuntimeCommand::LoadMessageContext(location) => RuntimeAdmissionPolicy::supersedable(
            RuntimeAdmissionKey::Navigation(if location.thread_ts().is_some() {
                NavigationSlot::Thread
            } else {
                NavigationSlot::Main
            }),
        ),
        RuntimeCommand::LoadConversationDetails { .. }
        | RuntimeCommand::SetConversationText { .. } => RuntimeAdmissionPolicy::durable_action(),
        RuntimeCommand::LoadUser { user_id } => {
            RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::User {
                scope: UserLoadScope::Basic,
                target: OpaqueAdmissionTarget::digest(&[user_id]),
            })
        }
        RuntimeCommand::LoadUserProfile { user_id } => {
            RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::User {
                scope: UserLoadScope::Profile,
                target: OpaqueAdmissionTarget::digest(&[user_id]),
            })
        }
        RuntimeCommand::LoadImageAsset { key, .. } => RuntimeAdmissionPolicy::coalescible(
            RuntimeAdmissionKey::ImageAsset(OpaqueAdmissionTarget::digest(&[key])),
        ),
        RuntimeCommand::LoadMedia { url, .. } => RuntimeAdmissionPolicy::coalescible(
            RuntimeAdmissionKey::Media(OpaqueAdmissionTarget::digest(&[url])),
        ),
        RuntimeCommand::ResolveMessagePermalink { channel_id, ts } => {
            RuntimeAdmissionPolicy::coalescible(RuntimeAdmissionKey::MessagePermalink(
                OpaqueAdmissionTarget::digest(&[channel_id, ts]),
            ))
        }
        RuntimeCommand::UpdateAttentionPreferences(_)
        | RuntimeCommand::JoinConversation { .. }
        | RuntimeCommand::LeaveConversation { .. }
        | RuntimeCommand::OpenDirectMessage { .. }
        | RuntimeCommand::OpenGroupDirectMessage { .. }
        | RuntimeCommand::CreateChannel { .. }
        | RuntimeCommand::InviteToChannel { .. }
        | RuntimeCommand::DownloadAttachment { .. }
        | RuntimeCommand::ExecuteMessageAction { .. }
        | RuntimeCommand::PostMessage { .. }
        | RuntimeCommand::ExecuteSlashCommand { .. }
        | RuntimeCommand::UpdateMessage { .. }
        | RuntimeCommand::SetReaction { .. }
        | RuntimeCommand::SetSaved { .. }
        | RuntimeCommand::SetConversationStarred { .. }
        | RuntimeCommand::SetCurrentUserStatus { .. }
        | RuntimeCommand::MarkConversationRead { .. }
        | RuntimeCommand::MarkConversationUnread { .. }
        | RuntimeCommand::MarkThreadRead { .. }
        | RuntimeCommand::UploadFiles { .. }
        | RuntimeCommand::Huddle(_) => RuntimeAdmissionPolicy::durable_action(),
    }
}

fn expected_legacy_scheduling(
    command: &RuntimeCommand,
) -> (bool, Option<NavigationSlot>, RuntimeTaskLane) {
    match command {
        RuntimeCommand::LoadHistory { .. }
        | RuntimeCommand::LoadOlderHistory { .. }
        | RuntimeCommand::SearchMessages { .. }
        | RuntimeCommand::LoadFiles
        | RuntimeCommand::LoadFile { .. }
        | RuntimeCommand::LoadSavedItems
        | RuntimeCommand::LoadFollowedThreads { .. } => (
            true,
            Some(NavigationSlot::Main),
            RuntimeTaskLane::Navigation,
        ),
        RuntimeCommand::LoadThread { .. } | RuntimeCommand::LoadOlderThread { .. } => (
            true,
            Some(NavigationSlot::Thread),
            RuntimeTaskLane::Navigation,
        ),
        RuntimeCommand::LoadMessageContext(location) => (
            true,
            Some(if location.thread_ts().is_some() {
                NavigationSlot::Thread
            } else {
                NavigationSlot::Main
            }),
            RuntimeTaskLane::Navigation,
        ),
        RuntimeCommand::LoadStoredToken
        | RuntimeCommand::StartOAuth { .. }
        | RuntimeCommand::StartBrowserSession { .. }
        | RuntimeCommand::JoinConversation { .. }
        | RuntimeCommand::LoadConversationDetails { .. }
        | RuntimeCommand::OpenDirectMessage { .. }
        | RuntimeCommand::ResolveMessagePermalink { .. } => {
            (true, None, RuntimeTaskLane::Interactive)
        }
        RuntimeCommand::RefreshConversations
        | RuntimeCommand::DiscoverChannels
        | RuntimeCommand::DiscoverConversations
        | RuntimeCommand::LoadUser { .. }
        | RuntimeCommand::LoadUserProfile { .. } => (true, None, RuntimeTaskLane::Background),
        RuntimeCommand::LoadImageAsset { .. }
        | RuntimeCommand::LoadMedia { .. }
        | RuntimeCommand::DownloadAttachment { .. } => (true, None, RuntimeTaskLane::Image),
        RuntimeCommand::UploadFiles { .. } => (false, None, RuntimeTaskLane::Upload),
        RuntimeCommand::SignOut
        | RuntimeCommand::Disconnect
        | RuntimeCommand::UpdateAttentionPreferences(_)
        | RuntimeCommand::LeaveConversation { .. }
        | RuntimeCommand::SetConversationText { .. }
        | RuntimeCommand::OpenGroupDirectMessage { .. }
        | RuntimeCommand::CreateChannel { .. }
        | RuntimeCommand::InviteToChannel { .. }
        | RuntimeCommand::ExecuteMessageAction { .. }
        | RuntimeCommand::PostMessage { .. }
        | RuntimeCommand::ExecuteSlashCommand { .. }
        | RuntimeCommand::UpdateMessage { .. }
        | RuntimeCommand::SetReaction { .. }
        | RuntimeCommand::SetSaved { .. }
        | RuntimeCommand::SetConversationStarred { .. }
        | RuntimeCommand::SetCurrentUserStatus { .. }
        | RuntimeCommand::MarkConversationRead { .. }
        | RuntimeCommand::MarkConversationUnread { .. }
        | RuntimeCommand::MarkThreadRead { .. }
        | RuntimeCommand::Huddle(_) => (false, None, RuntimeTaskLane::Interactive),
    }
}

fn runtime_command_fixtures() -> Vec<RuntimeCommand> {
    vec![
        RuntimeCommand::LoadStoredToken,
        RuntimeCommand::StartOAuth {
            client_id: "client".to_string(),
            debug_auth: false,
        },
        RuntimeCommand::StartBrowserSession {
            xoxc_token: "browser-token-canary".to_string(),
            xoxd_token: "cookie-token-canary".to_string(),
            user_agent: Some("agent-canary".to_string()),
        },
        RuntimeCommand::SignOut,
        RuntimeCommand::Disconnect,
        RuntimeCommand::RefreshConversations,
        RuntimeCommand::UpdateAttentionPreferences(AttentionPreferences::default()),
        RuntimeCommand::DiscoverChannels,
        RuntimeCommand::DiscoverConversations,
        RuntimeCommand::JoinConversation {
            channel_id: "C1".to_string(),
        },
        RuntimeCommand::LeaveConversation {
            channel_id: "C1".to_string(),
        },
        RuntimeCommand::LoadConversationDetails {
            channel_id: "C1".to_string(),
        },
        RuntimeCommand::SetConversationText {
            channel_id: "C1".to_string(),
            field: ConversationTextField::Topic,
            text: "topic".to_string(),
        },
        RuntimeCommand::OpenDirectMessage {
            user_id: "U1".to_string(),
        },
        RuntimeCommand::OpenGroupDirectMessage {
            user_ids: vec!["U1".to_string(), "U2".to_string()],
        },
        RuntimeCommand::CreateChannel {
            name: "channel".to_string(),
            is_private: false,
        },
        RuntimeCommand::InviteToChannel {
            channel_id: "C1".to_string(),
            user_ids: vec!["U1".to_string()],
        },
        RuntimeCommand::LoadHistory {
            channel_id: "C1".to_string(),
        },
        RuntimeCommand::LoadOlderHistory {
            channel_id: "C1".to_string(),
            cursor: "cursor".to_string(),
        },
        RuntimeCommand::LoadThread {
            channel_id: "C1".to_string(),
            ts: "1.0".to_string(),
        },
        RuntimeCommand::LoadOlderThread {
            channel_id: "C1".to_string(),
            ts: "1.0".to_string(),
            cursor: "cursor".to_string(),
        },
        RuntimeCommand::LoadMessageContext(SearchMessageLocation::new("C1", "1.0", None).unwrap()),
        RuntimeCommand::SearchMessages {
            query: "query-canary".to_string(),
        },
        RuntimeCommand::LoadFiles,
        RuntimeCommand::LoadFile {
            file_id: "F1".to_string(),
            share_requested: false,
        },
        RuntimeCommand::LoadSavedItems,
        RuntimeCommand::LoadFollowedThreads { cursor: None },
        RuntimeCommand::LoadUser {
            user_id: "U1".to_string(),
        },
        RuntimeCommand::LoadUserProfile {
            user_id: "U1".to_string(),
        },
        RuntimeCommand::LoadImageAsset {
            key: "image-key-canary".to_string(),
            url: "https://files.slack.com/image?token=image-url-canary".to_string(),
        },
        RuntimeCommand::LoadMedia {
            url: "https://files.slack.com/media?token=media-url-canary".to_string(),
            name: "media-name-canary".to_string(),
        },
        RuntimeCommand::DownloadAttachment {
            url: "https://files.slack.com/download?token=download-url-canary".to_string(),
            name: "download-name-canary".to_string(),
        },
        RuntimeCommand::ResolveMessagePermalink {
            channel_id: "C1".to_string(),
            ts: "1.0".to_string(),
        },
        RuntimeCommand::ExecuteMessageAction {
            request: SlackMessageActionRequest {
                channel_id: "C1".to_string(),
                message_ts: "1.0".to_string(),
                thread_ts: None,
                service_id: "B1".to_string(),
                app_id: None,
                bot_user_id: None,
                is_ephemeral: false,
                action: crate::rich_message::SlackControlAction::Block {
                    action: crate::rich_message::SensitiveValue::new("action-canary"),
                    attachment_id: None,
                },
            },
            control_handle: MessageControlHandle::synthetic(),
        },
        RuntimeCommand::PostMessage {
            channel_id: "C1".to_string(),
            text: "message-text-canary".to_string(),
            blocks_json: Some("blocks-canary".to_string()),
            attachments_json: None,
            thread_ts: None,
        },
        RuntimeCommand::ExecuteSlashCommand {
            channel_id: "C1".to_string(),
            command: "/giphy".to_string(),
            text: "command-text-canary".to_string(),
            thread_ts: None,
        },
        RuntimeCommand::UpdateMessage {
            channel_id: "C1".to_string(),
            original: Box::new(SlackMessage {
                ts: "1.0".to_string(),
                ..SlackMessage::default()
            }),
            text: "edit-text-canary".to_string(),
            blocks_json: Some("edit-blocks-canary".to_string()),
        },
        RuntimeCommand::SetReaction {
            channel_id: "C1".to_string(),
            ts: "1.0".to_string(),
            name: "reaction-canary".to_string(),
            add: true,
            thread_ts: None,
        },
        RuntimeCommand::SetSaved {
            channel_id: "C1".to_string(),
            ts: "1.0".to_string(),
            add: true,
            thread_ts: None,
        },
        RuntimeCommand::SetConversationStarred {
            channel_id: "C1".to_string(),
            starred: true,
        },
        RuntimeCommand::SetCurrentUserStatus {
            status: SlackUserStatus::default(),
        },
        RuntimeCommand::MarkConversationRead {
            channel_id: "C1".to_string(),
            ts: "1.0".to_string(),
        },
        RuntimeCommand::MarkConversationUnread {
            channel_id: "C1".to_string(),
            ts: "1.0".to_string(),
        },
        RuntimeCommand::MarkThreadRead {
            channel_id: "C1".to_string(),
            thread_ts: "1.0".to_string(),
            target_ts: Some("1.0".to_string()),
        },
        RuntimeCommand::UploadFiles {
            channel_id: "C1".to_string(),
            thread_ts: None,
            attachments: vec![UploadAttachment {
                path: PathBuf::from("upload-path-canary"),
                remove_after_upload: false,
            }],
            blocks_json: Some("upload-blocks-canary".to_string()),
        },
        RuntimeCommand::Huddle(HuddleCommand::SetMuted(true)),
    ]
}

#[test]
fn runtime_command_admission_metadata_is_exhaustive_and_behavior_neutral() {
    let commands = runtime_command_fixtures();
    assert_eq!(commands.len(), 46);

    for command in commands {
        let descriptor = command.descriptor();
        assert_eq!(descriptor.admission, expected_runtime_admission(&command));

        let (supersedes_previous, navigation_slot, lane) = expected_legacy_scheduling(&command);
        assert_eq!(descriptor.supersedes_previous, supersedes_previous);
        assert_eq!(descriptor.navigation_slot, navigation_slot);
        assert_eq!(descriptor.lane, lane);

        assert_eq!(
            descriptor.admission.replacement_key.is_some(),
            matches!(
                descriptor.admission.kind,
                RuntimeAdmissionKind::Coalescible | RuntimeAdmissionKind::Supersedable
            )
        );
    }
}

#[test]
fn every_huddle_command_is_a_durable_action() {
    let commands = [
        HuddleCommand::OpenPreflight {
            call_id: "R1".to_string(),
        },
        HuddleCommand::Join {
            call_id: "R1".to_string(),
        },
        HuddleCommand::OpenExternally {
            call_id: "R1".to_string(),
        },
        HuddleCommand::Leave,
        HuddleCommand::Dismiss,
        HuddleCommand::SetMuted(true),
        HuddleCommand::SetCameraEnabled(true),
        HuddleCommand::SetScreenShareEnabled(true),
        HuddleCommand::SelectDevice {
            kind: crate::huddles::state::HuddleDeviceKind::Microphone,
            id: "device-canary".to_string(),
        },
    ];

    for command in commands {
        assert_eq!(
            huddle_admission_policy(&command),
            RuntimeAdmissionPolicy::durable_action()
        );
    }
}

#[test]
fn runtime_admission_keys_partition_replaceable_work() {
    let key = |command: RuntimeCommand| {
        command
            .descriptor()
            .admission
            .replacement_key
            .expect("replaceable command should have a key")
    };

    let main_navigation = key(RuntimeCommand::LoadHistory {
        channel_id: "C1".to_string(),
    });
    assert_eq!(
        main_navigation,
        key(RuntimeCommand::SearchMessages {
            query: "different query".to_string(),
        })
    );
    let thread_navigation = key(RuntimeCommand::LoadThread {
        channel_id: "C1".to_string(),
        ts: "1.0".to_string(),
    });
    assert_eq!(
        thread_navigation,
        key(RuntimeCommand::LoadMessageContext(
            SearchMessageLocation::new("C1", "2.0", Some("1.0")).unwrap(),
        ))
    );
    assert_ne!(main_navigation, thread_navigation);

    assert_eq!(
        key(RuntimeCommand::LoadStoredToken),
        key(RuntimeCommand::StartBrowserSession {
            xoxc_token: "token-a".to_string(),
            xoxd_token: "token-b".to_string(),
            user_agent: None,
        })
    );
    assert_ne!(
        key(RuntimeCommand::DiscoverConversations),
        key(RuntimeCommand::DiscoverChannels)
    );
    assert_ne!(
        key(RuntimeCommand::LoadUser {
            user_id: "U1".to_string(),
        }),
        key(RuntimeCommand::LoadUserProfile {
            user_id: "U1".to_string(),
        })
    );
    assert_ne!(
        key(RuntimeCommand::LoadUser {
            user_id: "U1".to_string(),
        }),
        key(RuntimeCommand::LoadUser {
            user_id: "U2".to_string(),
        })
    );
    assert_ne!(
        key(RuntimeCommand::LoadImageAsset {
            key: "asset-a".to_string(),
            url: "https://files.slack.com/a".to_string(),
        }),
        key(RuntimeCommand::LoadImageAsset {
            key: "asset-b".to_string(),
            url: "https://files.slack.com/a".to_string(),
        })
    );
    // The viewer matches completion by URL, so one completion serves duplicate names.
    assert_eq!(
        key(RuntimeCommand::LoadMedia {
            url: "https://files.slack.com/media-a".to_string(),
            name: "first-name.mp4".to_string(),
        }),
        key(RuntimeCommand::LoadMedia {
            url: "https://files.slack.com/media-a".to_string(),
            name: "second-name.mp4".to_string(),
        })
    );
    assert_ne!(
        key(RuntimeCommand::LoadMedia {
            url: "https://files.slack.com/media-a".to_string(),
            name: "media.mp4".to_string(),
        }),
        key(RuntimeCommand::LoadMedia {
            url: "https://files.slack.com/media-b".to_string(),
            name: "media.mp4".to_string(),
        })
    );
    assert_ne!(
        key(RuntimeCommand::ResolveMessagePermalink {
            channel_id: "C1".to_string(),
            ts: "1.0".to_string(),
        }),
        key(RuntimeCommand::ResolveMessagePermalink {
            channel_id: "C1".to_string(),
            ts: "2.0".to_string(),
        })
    );
}

#[test]
fn runtime_admission_regressions_preserve_durable_intent() {
    let policy = |command: RuntimeCommand| command.descriptor().admission;

    assert_eq!(
        policy(RuntimeCommand::JoinConversation {
            channel_id: "C1".to_string(),
        })
        .kind,
        RuntimeAdmissionKind::DurableAction
    );
    assert_eq!(
        policy(RuntimeCommand::OpenDirectMessage {
            user_id: "U1".to_string(),
        })
        .kind,
        RuntimeAdmissionKind::DurableAction
    );
    assert_eq!(
        policy(RuntimeCommand::RefreshConversations).kind,
        RuntimeAdmissionKind::Coalescible
    );
}

#[test]
fn runtime_admission_debug_output_excludes_command_payloads() {
    let debug = runtime_command_fixtures()
        .iter()
        .map(|command| format!("{:?}", command.descriptor()))
        .collect::<Vec<_>>()
        .join("\n");

    for private in [
        "browser-token-canary",
        "cookie-token-canary",
        "agent-canary",
        "query-canary",
        "image-key-canary",
        "image-url-canary",
        "media-url-canary",
        "media-name-canary",
        "download-url-canary",
        "download-name-canary",
        "action-canary",
        "message-text-canary",
        "blocks-canary",
        "edit-text-canary",
        "edit-blocks-canary",
        "reaction-canary",
        "upload-path-canary",
        "upload-blocks-canary",
    ] {
        assert!(!debug.contains(private), "descriptor leaked {private}");
    }
}

#[test]
fn runtime_command_descriptor_owns_scheduling_policy() {
    let main_navigation = RuntimeCommand::LoadFiles.descriptor();
    assert_eq!(main_navigation.lane, RuntimeTaskLane::Navigation);
    assert_eq!(main_navigation.navigation_slot, Some(NavigationSlot::Main));
    assert!(main_navigation.supersedes_previous);

    let file_navigation = RuntimeCommand::LoadFile {
        file_id: "F123".to_string(),
        share_requested: false,
    }
    .descriptor();
    assert_eq!(file_navigation.lane, RuntimeTaskLane::Navigation);
    assert_eq!(file_navigation.navigation_slot, Some(NavigationSlot::Main));

    let background = RuntimeCommand::DiscoverConversations.descriptor();
    assert_eq!(background.lane, RuntimeTaskLane::Background);
    assert_eq!(background.navigation_slot, None);

    let channel_discovery = RuntimeCommand::DiscoverChannels.descriptor();
    assert_eq!(channel_discovery.lane, RuntimeTaskLane::Background);
    assert_eq!(
        channel_discovery.context,
        OperationContext::new(
            RuntimeOperation::ConversationDiscovery,
            RuntimeTarget::Workspace,
        )
    );

    let image = RuntimeCommand::LoadImageAsset {
        key: "preview".to_string(),
        url: "https://files.slack.com/preview".to_string(),
    }
    .descriptor();
    assert_eq!(image.lane, RuntimeTaskLane::Image);

    let upload = RuntimeCommand::UploadFiles {
        channel_id: "C123".to_string(),
        thread_ts: None,
        attachments: vec![UploadAttachment {
            path: PathBuf::from("upload.png"),
            remove_after_upload: false,
        }],
        blocks_json: None,
    }
    .descriptor();
    assert_eq!(upload.lane, RuntimeTaskLane::Upload);
    assert!(!upload.supersedes_previous);

    let interactive = RuntimeCommand::OpenDirectMessage {
        user_id: "U123".to_string(),
    }
    .descriptor();
    assert_eq!(interactive.lane, RuntimeTaskLane::Interactive);
    assert!(interactive.supersedes_previous);

    let durable_mutation = RuntimeCommand::SetConversationStarred {
        channel_id: "C123".to_string(),
        starred: true,
    }
    .descriptor();
    assert_eq!(durable_mutation.lane, RuntimeTaskLane::Interactive);
    assert!(!durable_mutation.supersedes_previous);

    let permalink = RuntimeCommand::ResolveMessagePermalink {
        channel_id: "C123".to_string(),
        ts: "1710000000.000100".to_string(),
    }
    .descriptor();
    assert_eq!(permalink.lane, RuntimeTaskLane::Interactive);
    assert!(permalink.supersedes_previous);

    let message_action = RuntimeCommand::ExecuteMessageAction {
        request: SlackMessageActionRequest {
            channel_id: "C123".to_string(),
            message_ts: "1710000000.000100".to_string(),
            thread_ts: None,
            service_id: "B123".to_string(),
            app_id: Some("A123".to_string()),
            bot_user_id: Some("U123".to_string()),
            is_ephemeral: false,
            action: crate::rich_message::SlackControlAction::Block {
                action: crate::rich_message::SensitiveValue::new(
                    r#"{"type":"button","block_id":"block","action_id":"approve"}"#,
                ),
                attachment_id: None,
            },
        },
        control_handle: MessageControlHandle::synthetic(),
    }
    .descriptor();
    assert_eq!(message_action.lane, RuntimeTaskLane::Interactive);
    assert!(!message_action.supersedes_previous);
    assert_eq!(
        message_action.context,
        OperationContext::new(
            RuntimeOperation::MessageAction,
            RuntimeTarget::ExactMessage {
                channel_id: "C123".to_string(),
                ts: "1710000000.000100".to_string(),
            },
        )
    );

    let leave = RuntimeCommand::LeaveConversation {
        channel_id: "C123".to_string(),
    }
    .descriptor();
    assert_eq!(leave.lane, RuntimeTaskLane::Interactive);
    assert!(!leave.supersedes_previous);
    assert_eq!(
        leave.context,
        OperationContext::new(
            RuntimeOperation::LeaveConversation,
            RuntimeTarget::Channel("C123".to_string()),
        )
    );

    let huddle =
        RuntimeCommand::Huddle(crate::huddles::state::HuddleCommand::SetMuted(true)).descriptor();
    assert_eq!(huddle.lane, RuntimeTaskLane::Interactive);
    assert!(!huddle.supersedes_previous);
    assert_eq!(
        huddle.context,
        OperationContext::new(
            RuntimeOperation::Huddle,
            RuntimeTarget::Huddle("active".to_string()),
        )
    );
}

#[test]
fn follow_up_workspace_sync_jobs_have_distinct_cancellation_ids() {
    let startup = sync_job_cancellation_id(&SyncJobPayload::WorkspaceStartup, 0);
    let refresh = sync_job_cancellation_id(&SyncJobPayload::WorkspaceRefresh, 1);
    let directory = sync_job_cancellation_id(
        &SyncJobPayload::MembershipSync {
            channel_id: "user_directory".to_string(),
        },
        2,
    );

    assert_ne!(startup, refresh);
    assert_ne!(startup, directory);
    assert_ne!(refresh, directory);
}

#[test]
fn conversation_star_command_is_an_interactive_channel_mutation() {
    let descriptor = RuntimeCommand::SetConversationStarred {
        channel_id: "C123".to_string(),
        starred: true,
    }
    .descriptor();

    assert_eq!(descriptor.lane, RuntimeTaskLane::Interactive);
    assert!(!descriptor.supersedes_previous);
    assert_eq!(
        descriptor.context,
        OperationContext::new(
            RuntimeOperation::ConversationStar,
            RuntimeTarget::Channel("C123".to_string()),
        )
    );
}

#[test]
fn current_user_status_command_is_an_interactive_user_mutation() {
    let descriptor = RuntimeCommand::SetCurrentUserStatus {
        status: SlackUserStatus {
            text: "Focus time".to_string(),
            emoji: ":headphones:".to_string(),
            expiration: 2_000_000_000,
        },
    }
    .descriptor();

    assert_eq!(descriptor.lane, RuntimeTaskLane::Interactive);
    assert!(!descriptor.supersedes_previous);
    assert_eq!(
        descriptor.context,
        OperationContext::new(RuntimeOperation::UserStatus, RuntimeTarget::Workspace,)
    );
}

#[test]
fn failed_conversation_star_persistence_emits_no_patch_or_completion() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-star-no-premature-event-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (sender, mut receiver) = runtime_event_channel();
    let events = RuntimeEventSender {
        sender,
        session: SessionId::default().next(),
        request: None,
        fallback: OperationContext::new(
            RuntimeOperation::ConversationStar,
            RuntimeTarget::Channel("C1".to_string()),
        ),
        workspace_patch_send_gate: None,
    };

    runtime.block_on(async {
        let initial = SlackConversation {
            id: "C1".to_string(),
            is_channel: Some(true),
            is_starred: Some(false),
            ..Default::default()
        };
        workspace
            .apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![initial.clone()],
                    ..Default::default()
                }),
            )
            .unwrap();
        store.store_conversation(&initial).await.unwrap();
        store
            .install_conversation_batch_failure_trigger_for("C1")
            .await
            .unwrap();

        assert!(persist_confirmed_conversation_star(
            &events,
            &workspace,
            Some(&store),
            "C1".to_string(),
            true,
        )
        .await
        .is_err());
        assert!(
            receiver.try_recv().is_err(),
            "neither patch nor completion may precede persistence"
        );
        assert!(workspace
            .coordinator
            .lock()
            .unwrap()
            .conversation("C1")
            .unwrap()
            .is_starred());
        assert!(!store.load_conversations().await.unwrap().unwrap()[0].is_starred());
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn conversation_star_sync_keeps_a_newer_toggle_after_a_stale_refresh() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-conversation-star-race-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let workspace = WorkspaceReducerAdapter::default();
        let initial = SlackConversation {
            id: "C1".to_string(),
            is_channel: Some(true),
            is_starred: Some(false),
            ..Default::default()
        };
        store.store_conversation(&initial).await.unwrap();
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(initial.clone()),
        );
        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
            ),
            workspace_patch_send_gate: None,
        };
        let gate = ConversationStarSyncGate::default();
        let refresh_guard = gate.lock().await;

        let toggle_gate = gate.clone();
        let toggle_store = store.clone();
        let toggle_workspace = workspace.clone();
        let toggle_events = events.clone();
        let toggle = tokio::spawn(async move {
            let _guard = toggle_gate.lock().await;
            persist_confirmed_conversation_star(
                &toggle_events,
                &toggle_workspace,
                Some(&toggle_store),
                "C1".to_string(),
                true,
            )
            .await
            .unwrap();
        });
        tokio::task::yield_now().await;
        assert!(!toggle.is_finished());

        store.store_conversation(&initial).await.unwrap();
        workspace.apply(
            MutationOrigin::WebApi,
            WorkspaceMutation::ConversationUpsert(initial.clone()),
        );
        drop(refresh_guard);
        toggle.await.unwrap();

        let persisted = store
            .load_conversations()
            .await
            .unwrap()
            .expect("missing cached conversations");
        assert!(persisted[0].is_starred());
        assert!(workspace
            .coordinator
            .lock()
            .unwrap()
            .conversation("C1")
            .unwrap()
            .is_starred());
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::WorkspacePatch(_)
        ));
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::ConversationStarUpdateCompleted {
                channel_id,
                starred: true,
            } if channel_id == "C1"
        ));

        let _ = std::fs::remove_dir_all(directory);
    });
}

fn test_interactive_runtime_connection(
    slack: SlackApi,
    store: Option<WorkspaceStore>,
    workspace: WorkspaceReducerAdapter,
    current_user_id: Option<String>,
) -> RuntimeConnection {
    let (huddles, _huddle_receiver) = huddle_actor_channel();
    RuntimeConnection {
        slack,
        workspace_url: None,
        workspace_store: store,
        image_cache_scope: "test-workspace".to_string(),
        workspace,
        current_user_id,
        user_cache: Arc::new(Mutex::new(HashMap::new())),
        message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(8))),
        conversation_star_sync: ConversationStarSyncGate::default(),
        user_status_sync: UserStatusSync::default(),
        team_id: None,
        huddles,
        read_flusher: ReadFlusherHandle::new(
            Arc::new(Mutex::new(ReadFlusherQueue::new())),
            Arc::new(Notify::new()),
        ),
        scheduler: Arc::new(Mutex::new(SyncScheduler::new(
            SchedulerConfig::new(256, 8, 5).unwrap(),
        ))),
        pending_jobs: Arc::new(Mutex::new(HashMap::new())),
        next_job_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        cached_bootstrap_load_gate: None,
    }
}

#[test]
fn custom_emoji_sync_persists_and_emits_on_success() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-custom-emoji-sync-success-test-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let workspace = WorkspaceReducerAdapter::default();

        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
            ),
            workspace_patch_send_gate: None,
        };

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let server_addr = server.server_addr();
        let server_thread = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(
                        r#"{"ok":true,"emoji":{"partyparrot":"https://example.com/parrot.gif"}}"#,
                    )
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                    ),
                )
                .unwrap();
        });

        let mut slack = SlackApi::new(StoredToken {
            access_token: "test-token".to_string(),
            ..Default::default()
        });
        slack.api_base_url = format!("http://{server_addr}/api");

        let connection = test_interactive_runtime_connection(
            slack,
            Some(store.clone()),
            workspace,
            Some("U123".to_string()),
        );

        sync_custom_emojis_best_effort(&events, &connection).await;
        server_thread.join().unwrap();

        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::EmojiCatalogLoaded(emojis)
                if emojis.get("partyparrot").map(String::as_str)
                    == Some("https://example.com/parrot.gif")
        ));

        let bootstrap = store
            .load_bootstrap()
            .await
            .unwrap()
            .expect("missing cached bootstrap");
        assert_eq!(
            bootstrap
                .custom_emojis
                .get("partyparrot")
                .map(String::as_str),
            Some("https://example.com/parrot.gif")
        );

        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn custom_emoji_sync_failure_is_swallowed_and_does_not_emit() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-custom-emoji-sync-failure-test-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let workspace = WorkspaceReducerAdapter::default();

        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
            ),
            workspace_patch_send_gate: None,
        };

        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let server_addr = server.server_addr();
        let server_thread = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(r#"{"ok":false,"error":"internal_error"}"#)
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Type", "application/json")
                                .unwrap(),
                        ),
                )
                .unwrap();
        });

        let mut slack = SlackApi::new(StoredToken {
            access_token: "test-token".to_string(),
            ..Default::default()
        });
        slack.api_base_url = format!("http://{server_addr}/api");

        let connection = test_interactive_runtime_connection(
            slack,
            Some(store.clone()),
            workspace,
            Some("U123".to_string()),
        );

        // Must not panic and must not block on a failed fetch.
        sync_custom_emojis_best_effort(&events, &connection).await;
        server_thread.join().unwrap();

        assert!(receiver.try_recv().is_err());

        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn optimistic_reaction_success_and_failure_rollback() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-reaction-optimistic-test-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let workspace = WorkspaceReducerAdapter::default();
        let initial_message = SlackMessage {
            ts: "1710000000.000100".to_string(),
            text: Some("Hello reaction".to_string()),
            user: Some("U_OTHER".to_string()),
            reactions: Some(vec![]),
            ..Default::default()
        };
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: initial_message.clone(),
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Cache,
            },
        );

        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
            ),
            workspace_patch_send_gate: None,
        };
        let image_cache = ImageAssetCache::new(directory.join("images"));

        // 1. Success case: optimistic add reaction succeeds and persists
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let server_addr = server.server_addr();
        let server_thread = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(r#"{"ok":true}"#).with_header(
                        tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                    ),
                )
                .unwrap();
        });

        let mut slack = SlackApi::new(StoredToken {
            access_token: "test-token".to_string(),
            ..Default::default()
        });
        slack.api_base_url = format!("http://{server_addr}/api");

        let connection = test_interactive_runtime_connection(
            slack,
            Some(store.clone()),
            workspace.clone(),
            Some("U123".to_string()),
        );

        let result = handle_connected_command(
            RuntimeCommand::SetReaction {
                channel_id: "C1".to_string(),
                ts: "1710000000.000100".to_string(),
                name: "thumbsup".to_string(),
                add: true,
                thread_ts: None,
            },
            connection,
            &events,
            &image_cache,
        )
        .await;
        server_thread.join().unwrap();
        assert!(result.is_ok());

        let updated_msg = workspace.message("C1", "1710000000.000100").unwrap();
        let reactions = updated_msg.reactions.as_deref().unwrap_or(&[]);
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].name.as_deref(), Some("thumbsup"));
        assert_eq!(reactions[0].count, Some(1));
        assert_eq!(
            reactions[0].users.as_deref(),
            Some(&["U123".to_string()][..])
        );

        let mut got_patch = false;
        let mut got_completion = false;
        while let Ok(event) = receiver.try_recv() {
            match event.kind {
                RuntimeEventKind::WorkspacePatch(_) => got_patch = true,
                RuntimeEventKind::ReactionUpdateCompleted {
                    channel_id,
                    message_ts,
                    projected,
                    ..
                } => {
                    assert_eq!(channel_id, "C1");
                    assert_eq!(message_ts, "1710000000.000100");
                    assert!(projected);
                    got_completion = true;
                }
                _ => {}
            }
        }
        assert!(got_patch, "WorkspacePatch should be emitted");
        assert!(got_completion, "ReactionUpdateCompleted should be emitted");

        // 2. Failure case: optimistic add reaction rolls back on API failure
        let fail_server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let fail_server_addr = fail_server.server_addr();
        let fail_thread = std::thread::spawn(move || {
            let request = fail_server.recv().unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(r#"{"ok":false,"error":"bad_reaction"}"#)
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Type", "application/json")
                                .unwrap(),
                        ),
                )
                .unwrap();
        });

        let mut fail_slack = SlackApi::new(StoredToken {
            access_token: "test-token".to_string(),
            ..Default::default()
        });
        fail_slack.api_base_url = format!("http://{fail_server_addr}/api");

        let fail_connection = test_interactive_runtime_connection(
            fail_slack,
            Some(store.clone()),
            workspace.clone(),
            Some("U123".to_string()),
        );

        let fail_result = handle_connected_command(
            RuntimeCommand::SetReaction {
                channel_id: "C1".to_string(),
                ts: "1710000000.000100".to_string(),
                name: "heart".to_string(),
                add: true,
                thread_ts: None,
            },
            fail_connection,
            &events,
            &image_cache,
        )
        .await;
        fail_thread.join().unwrap();
        assert!(fail_result.is_err());

        let rolled_back_msg = workspace.message("C1", "1710000000.000100").unwrap();
        let rolled_back_reactions = rolled_back_msg.reactions.as_deref().unwrap_or(&[]);
        assert_eq!(rolled_back_reactions.len(), 1);
        assert_eq!(rolled_back_reactions[0].name.as_deref(), Some("thumbsup"));

        let mut patch_count = 0;
        let mut got_fail_completion = false;
        while let Ok(event) = receiver.try_recv() {
            match event.kind {
                RuntimeEventKind::WorkspacePatch(_) => patch_count += 1,
                RuntimeEventKind::ReactionUpdateCompleted { .. } => {
                    got_fail_completion = true;
                }
                _ => {}
            }
        }
        assert!(
            patch_count >= 2,
            "both optimistic and rollback patches should be emitted"
        );
        assert!(
            !got_fail_completion,
            "ReactionUpdateCompleted must NOT be emitted on API failure"
        );

        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn optimistic_saved_success_and_failure_rollback() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-saved-optimistic-test-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let workspace = WorkspaceReducerAdapter::default();
        let initial_message = SlackMessage {
            ts: "1710000000.000200".to_string(),
            text: Some("Hello saved".to_string()),
            is_starred: Some(false),
            ..Default::default()
        };
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::MessageChanged {
                channel_id: "C1".to_string(),
                message: initial_message.clone(),
                kind: MessageMutationKind::Changed,
                origin: MutationOrigin::Cache,
            },
        );

        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
            ),
            workspace_patch_send_gate: None,
        };
        let image_cache = ImageAssetCache::new(directory.join("images"));

        // 1. Success case: optimistic saved succeeds
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let server_addr = server.server_addr();
        let server_thread = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(r#"{"ok":true}"#).with_header(
                        tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                    ),
                )
                .unwrap();
        });

        let mut slack = SlackApi::new(StoredToken {
            access_token: "test-token".to_string(),
            ..Default::default()
        });
        slack.api_base_url = format!("http://{server_addr}/api");

        let connection = test_interactive_runtime_connection(
            slack,
            Some(store.clone()),
            workspace.clone(),
            Some("U123".to_string()),
        );

        let result = handle_connected_command(
            RuntimeCommand::SetSaved {
                channel_id: "C1".to_string(),
                ts: "1710000000.000200".to_string(),
                add: true,
                thread_ts: None,
            },
            connection,
            &events,
            &image_cache,
        )
        .await;
        server_thread.join().unwrap();
        assert!(result.is_ok());

        let updated_msg = workspace.message("C1", "1710000000.000200").unwrap();
        assert_eq!(updated_msg.is_starred, Some(true));

        let mut got_saved_event = false;
        while let Ok(event) = receiver.try_recv() {
            if let RuntimeEventKind::SavedUpdated {
                channel_id,
                message_ts,
                saved,
                ..
            } = event.kind
            {
                assert_eq!(channel_id, "C1");
                assert_eq!(message_ts, "1710000000.000200");
                assert!(saved);
                got_saved_event = true;
            }
        }
        assert!(got_saved_event, "SavedUpdated event should be emitted");

        // 2. Failure case: optimistic saved rolls back on API failure
        let fail_server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let fail_server_addr = fail_server.server_addr();
        let fail_thread = std::thread::spawn(move || {
            let request = fail_server.recv().unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(r#"{"ok":false,"error":"cant_star"}"#)
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Type", "application/json")
                                .unwrap(),
                        ),
                )
                .unwrap();
        });

        let mut fail_slack = SlackApi::new(StoredToken {
            access_token: "test-token".to_string(),
            ..Default::default()
        });
        fail_slack.api_base_url = format!("http://{fail_server_addr}/api");

        let fail_connection = test_interactive_runtime_connection(
            fail_slack,
            Some(store.clone()),
            workspace.clone(),
            Some("U123".to_string()),
        );

        let fail_result = handle_connected_command(
            RuntimeCommand::SetSaved {
                channel_id: "C1".to_string(),
                ts: "1710000000.000200".to_string(),
                add: false,
                thread_ts: None,
            },
            fail_connection,
            &events,
            &image_cache,
        )
        .await;
        fail_thread.join().unwrap();
        assert!(fail_result.is_err());

        let rolled_back_msg = workspace.message("C1", "1710000000.000200").unwrap();
        assert_eq!(
            rolled_back_msg.is_starred,
            Some(true),
            "is_starred should roll back to true"
        );

        let mut got_fail_saved_event = false;
        while let Ok(event) = receiver.try_recv() {
            if let RuntimeEventKind::SavedUpdated { .. } = event.kind {
                got_fail_saved_event = true;
            }
        }
        assert!(
            !got_fail_saved_event,
            "SavedUpdated must NOT be emitted on API failure"
        );

        let _ = std::fs::remove_dir_all(directory);
    });
}

#[test]
fn optimistic_conversation_star_success_and_failure_rollback() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "conduit-conv-star-optimistic-test-{}-{nonce}",
            std::process::id()
        ));
        let store = WorkspaceStore::new(directory.clone(), "T123:U123");
        let workspace = WorkspaceReducerAdapter::default();
        let initial = SlackConversation {
            id: "C1".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            is_starred: Some(false),
            ..Default::default()
        };
        workspace.apply(
            MutationOrigin::Cache,
            WorkspaceMutation::ConversationUpsert(initial.clone()),
        );

        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
            ),
            workspace_patch_send_gate: None,
        };
        let image_cache = ImageAssetCache::new(directory.join("images"));

        // 1. Success case: optimistic conversation star succeeds
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let server_addr = server.server_addr();
        let server_thread = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(r#"{"ok":true}"#).with_header(
                        tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                    ),
                )
                .unwrap();
        });

        let mut slack = SlackApi::new(StoredToken {
            access_token: "test-token".to_string(),
            ..Default::default()
        });
        slack.api_base_url = format!("http://{server_addr}/api");

        let connection = test_interactive_runtime_connection(
            slack,
            Some(store.clone()),
            workspace.clone(),
            Some("U123".to_string()),
        );

        let result = handle_connected_command(
            RuntimeCommand::SetConversationStarred {
                channel_id: "C1".to_string(),
                starred: true,
            },
            connection,
            &events,
            &image_cache,
        )
        .await;
        server_thread.join().unwrap();
        assert!(result.is_ok());

        assert!(workspace
            .coordinator
            .lock()
            .unwrap()
            .conversation("C1")
            .unwrap()
            .is_starred());

        let mut got_conv_star_event = false;
        while let Ok(event) = receiver.try_recv() {
            if let RuntimeEventKind::ConversationStarUpdateCompleted {
                channel_id,
                starred,
            } = event.kind
            {
                assert_eq!(channel_id, "C1");
                assert!(starred);
                got_conv_star_event = true;
            }
        }
        assert!(
            got_conv_star_event,
            "ConversationStarUpdateCompleted event should be emitted"
        );

        // 2. Failure case: optimistic conversation star rolls back on API failure
        let fail_server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let fail_server_addr = fail_server.server_addr();
        let fail_thread = std::thread::spawn(move || {
            let request = fail_server.recv().unwrap();
            request
                .respond(
                    tiny_http::Response::from_string(r#"{"ok":false,"error":"channel_not_found"}"#)
                        .with_header(
                            tiny_http::Header::from_bytes("Content-Type", "application/json")
                                .unwrap(),
                        ),
                )
                .unwrap();
        });

        let mut fail_slack = SlackApi::new(StoredToken {
            access_token: "test-token".to_string(),
            ..Default::default()
        });
        fail_slack.api_base_url = format!("http://{fail_server_addr}/api");

        let fail_connection = test_interactive_runtime_connection(
            fail_slack,
            Some(store.clone()),
            workspace.clone(),
            Some("U123".to_string()),
        );

        let fail_result = handle_connected_command(
            RuntimeCommand::SetConversationStarred {
                channel_id: "C1".to_string(),
                starred: false,
            },
            fail_connection,
            &events,
            &image_cache,
        )
        .await;
        fail_thread.join().unwrap();
        assert!(fail_result.is_err());

        assert!(
            workspace
                .coordinator
                .lock()
                .unwrap()
                .conversation("C1")
                .unwrap()
                .is_starred(),
            "conversation is_starred should roll back to true"
        );

        let mut got_fail_conv_star_event = false;
        while let Ok(event) = receiver.try_recv() {
            if let RuntimeEventKind::ConversationStarUpdateCompleted { .. } = event.kind {
                got_fail_conv_star_event = true;
            }
        }
        assert!(
            !got_fail_conv_star_event,
            "ConversationStarUpdateCompleted must NOT be emitted on API failure"
        );

        let _ = std::fs::remove_dir_all(directory);
    });
}

fn test_huddle(call_id: &str) -> ActiveHuddle {
    ActiveHuddle {
        team_id: "T123".to_string(),
        channel_id: "C123".to_string(),
        call_id: call_id.to_string(),
        name: None,
        participant_ids: Vec::new(),
        started_at: None,
        huddle_link: None,
    }
}

fn huddle_test_event_channel() -> (RuntimeEventSender, RuntimeEventReceiver) {
    let (event_sender, event_receiver) = runtime_event_channel();
    (
        RuntimeEventSender {
            sender: event_sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(
                RuntimeOperation::Huddle,
                RuntimeTarget::Huddle("active".to_string()),
            ),
            workspace_patch_send_gate: None,
        },
        event_receiver,
    )
}

fn available_huddle_join_capability() -> NativeJoinCapability {
    NativeJoinCapability::Available {
        slack_contract_revision: "test-slack",
        chime_bridge_revision: "test-chime",
    }
}

#[test]
fn huddle_actor_admission_bounds_observations_and_preserves_command_reserve_fifo() {
    admission_test_runtime().block_on(async {
        assert_eq!(HUDDLE_ACTOR_QUEUE_CAPACITY, 64);
        assert_eq!(HUDDLE_ACTOR_RESERVED_CAPACITY, 8);
        assert_eq!(HUDDLE_ACTOR_OBSERVATION_CAPACITY, 56);
        let (handle, mut receiver) = huddle_actor_channel();
        let observation_half = HUDDLE_ACTOR_OBSERVATION_CAPACITY / 2;
        let command_half = HUDDLE_ACTOR_RESERVED_CAPACITY / 2;
        let mut completions = Vec::new();

        for index in 0..observation_half {
            handle
                .observe(CoordinatorInput::PresenceChanged {
                    user_id: format!("U{index}"),
                    presence: None,
                })
                .await
                .unwrap();
        }
        for _ in 0..command_half {
            completions.push(handle.admit_command(HuddleCommand::Dismiss).await.unwrap());
        }
        for index in observation_half..HUDDLE_ACTOR_OBSERVATION_CAPACITY {
            handle
                .observe(CoordinatorInput::PresenceChanged {
                    user_id: format!("U{index}"),
                    presence: None,
                })
                .await
                .unwrap();
        }
        let blocked_observation_handle = handle.clone();
        let blocked_observation = tokio::spawn(async move {
            blocked_observation_handle
                .observe(CoordinatorInput::PresenceChanged {
                    user_id: "U_BLOCKED".into(),
                    presence: None,
                })
                .await
        });

        for _ in command_half..HUDDLE_ACTOR_RESERVED_CAPACITY {
            completions.push(handle.admit_command(HuddleCommand::Dismiss).await.unwrap());
        }
        let blocked_command_handle = handle.clone();
        let blocked_command = tokio::spawn(async move {
            blocked_command_handle
                .admit_command(HuddleCommand::Dismiss)
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.snapshot().blocked < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("huddle producers did not enter admission backpressure");
        assert!(!blocked_observation.is_finished());
        assert!(!blocked_command.is_finished());
        assert_eq!(
            handle.snapshot(),
            HuddleActorAdmissionSnapshot {
                admitted: 64,
                admitted_commands: 8,
                admitted_observations: 56,
                blocked: 2,
                depth: 64,
                observation_depth: 56,
                peak_depth: 64,
                peak_observation_depth: 56,
                ..HuddleActorAdmissionSnapshot::default()
            }
        );

        blocked_observation.abort();
        blocked_command.abort();
        assert!(blocked_observation.await.unwrap_err().is_cancelled());
        assert!(blocked_command.await.unwrap_err().is_cancelled());

        for index in 0..HUDDLE_ACTOR_QUEUE_CAPACITY {
            let message = receiver.recv().await.unwrap();
            if index < observation_half
                || (observation_half + command_half
                    ..HUDDLE_ACTOR_OBSERVATION_CAPACITY + command_half)
                    .contains(&index)
            {
                assert!(matches!(
                    message.kind(),
                    HuddleActorMessageKind::Observation(HuddleObservationKind::Presence)
                ));
            } else {
                assert_eq!(message.kind(), HuddleActorMessageKind::Command);
            }
            drop(message);
        }
        drop(completions);
        let snapshot = receiver.snapshot();
        assert_eq!(snapshot.dequeued, 64);
        assert_eq!(snapshot.depth, 0);
        assert_eq!(snapshot.observation_depth, 0);
    });
}

#[test]
fn huddle_actor_close_wakes_waiters_and_drains_accepted_fifo() {
    admission_test_runtime().block_on(async {
        let (handle, mut receiver) = huddle_actor_channel_with_capacity(1, 1);
        handle
            .observe(CoordinatorInput::PresenceChanged {
                user_id: "U1".into(),
                presence: None,
            })
            .await
            .unwrap();
        let waiting_handle = handle.clone();
        let waiting = tokio::spawn(async move {
            waiting_handle
                .observe(CoordinatorInput::PresenceChanged {
                    user_id: "U2".into(),
                    presence: None,
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.snapshot().blocked < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("huddle observation did not enter admission backpressure");
        assert!(!waiting.is_finished());

        receiver.close();
        assert!(tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("closed observation admission did not wake")
            .unwrap()
            .is_err());
        assert!(matches!(
            receiver.recv().await.unwrap().kind(),
            HuddleActorMessageKind::Observation(HuddleObservationKind::Presence)
        ));
        assert!(receiver.recv().await.is_none());
        assert!(handle.admit_command(HuddleCommand::Dismiss).await.is_err());
    });
}

#[test]
fn huddle_actor_metrics_never_exceed_physical_queue_capacity() {
    admission_test_runtime().block_on(async {
        const MESSAGE_COUNT: usize = 1_000;
        let (handle, mut receiver) = huddle_actor_channel_with_capacity(1, 1);
        let producer_handle = handle.clone();
        let producer = tokio::spawn(async move {
            for _ in 0..MESSAGE_COUNT {
                let completion = producer_handle
                    .admit_command(HuddleCommand::Dismiss)
                    .await
                    .unwrap();
                drop(completion);
            }
        });
        for _ in 0..MESSAGE_COUNT {
            assert_eq!(
                receiver.recv().await.unwrap().kind(),
                HuddleActorMessageKind::Command
            );
        }
        producer.await.unwrap();

        let snapshot = receiver.snapshot();
        assert_eq!(snapshot.admitted, MESSAGE_COUNT as u64);
        assert_eq!(snapshot.dequeued, MESSAGE_COUNT as u64);
        assert_eq!(snapshot.depth, 0);
        assert_eq!(snapshot.peak_depth, 1);
    });
}

#[test]
fn cancelled_huddle_observation_restores_reserved_observation_slot() {
    admission_test_runtime().block_on(async {
        let (handle, mut receiver) = huddle_actor_channel_with_capacity(1, 1);
        let command_completion = handle.admit_command(HuddleCommand::Dismiss).await.unwrap();
        let waiting_handle = handle.clone();
        let waiting = tokio::spawn(async move {
            waiting_handle
                .observe(CoordinatorInput::PresenceChanged {
                    user_id: "U_CANCELLED".into(),
                    presence: None,
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.snapshot().blocked < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("observation did not wait behind full actor channel");
        assert_eq!(handle.admission.observation_slots.available_permits(), 0);

        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert_eq!(handle.admission.observation_slots.available_permits(), 1);

        assert_eq!(
            receiver.recv().await.unwrap().kind(),
            HuddleActorMessageKind::Command
        );
        assert!(command_completion.await.is_err());
        handle
            .observe(CoordinatorInput::PresenceChanged {
                user_id: "U_LATER".into(),
                presence: None,
            })
            .await
            .expect("cancelled observation leaked reserved capacity");
        assert!(matches!(
            receiver.recv().await.unwrap().kind(),
            HuddleActorMessageKind::Observation(HuddleObservationKind::Presence)
        ));
    });
}

#[test]
fn huddle_supervisor_drains_commands_applies_reset_and_closes_retained_handle() {
    admission_test_runtime().block_on(async {
        let (events, mut event_receiver) = huddle_test_event_channel();
        let (handle, receiver) = huddle_actor_channel();
        handle.observe_huddle(test_huddle("R123")).await.unwrap();
        let completion = handle
            .admit_command(HuddleCommand::OpenPreflight {
                call_id: "R123".into(),
            })
            .await
            .unwrap();
        let mut supervisor = HuddleSessionSupervisor::spawn(
            SessionId::default().next(),
            receiver,
            events,
            available_huddle_join_capability(),
        );
        assert!(supervisor.start());
        supervisor.shutdown().await;

        completion.await.unwrap().unwrap();
        let phases = std::iter::from_fn(|| event_receiver.try_recv().ok())
            .filter_map(|event| match event.kind {
                RuntimeEventKind::Huddle(HuddleEvent::Snapshot(snapshot)) => {
                    Some((snapshot.phase, snapshot.native_join_available))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            phases,
            vec![
                (HuddlePhase::Discovered, true),
                (HuddlePhase::Preflight, true),
                (HuddlePhase::Idle, true),
            ]
        );
        assert!(handle.observe_huddle(test_huddle("R999")).await.is_err());
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.admitted, 2);
        assert_eq!(snapshot.dequeued, 2);
        assert_eq!(snapshot.depth, 0);
    });
}

#[test]
fn unstarted_huddle_supervisor_rejects_admitted_commands() {
    admission_test_runtime().block_on(async {
        let (events, mut event_receiver) = huddle_test_event_channel();
        let (handle, receiver) = huddle_actor_channel();
        let completion = handle.admit_command(HuddleCommand::Dismiss).await.unwrap();
        let supervisor = HuddleSessionSupervisor::spawn(
            SessionId::default().next(),
            receiver,
            events,
            production_native_join_capability(false),
        );
        supervisor.shutdown().await;

        assert!(completion.await.unwrap().is_err());
        assert!(event_receiver.try_recv().is_err());
        assert!(handle.admit_command(HuddleCommand::Dismiss).await.is_err());
    });
}

#[test]
fn cancelled_huddle_command_waiter_does_not_cancel_admitted_command() {
    admission_test_runtime().block_on(async {
        let (events, mut event_receiver) = huddle_test_event_channel();
        let (handle, receiver) = huddle_actor_channel();
        handle.observe_huddle(test_huddle("R123")).await.unwrap();
        let command_handle = handle.clone();
        let command = tokio::spawn(async move {
            command_handle
                .command(HuddleCommand::OpenPreflight {
                    call_id: "R123".into(),
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.snapshot().admitted < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        command.abort();
        assert!(command.await.unwrap_err().is_cancelled());

        let (_shutdown, shutdown_receiver) = oneshot::channel();
        let actor = tokio::spawn(run_huddle_actor(
            receiver,
            events,
            production_native_join_capability(false),
            shutdown_receiver,
        ));
        assert!(matches!(
            event_receiver.recv().await.unwrap().kind,
            RuntimeEventKind::Huddle(HuddleEvent::Snapshot(snapshot))
                if snapshot.phase == HuddlePhase::Discovered
        ));
        assert!(matches!(
            event_receiver.recv().await.unwrap().kind,
            RuntimeEventKind::Huddle(HuddleEvent::Snapshot(snapshot))
                if snapshot.phase == HuddlePhase::Preflight
        ));
        drop(handle);
        actor.await.unwrap();
    });
}

#[test]
fn huddle_command_reports_invalid_transition_after_actor_processing() {
    admission_test_runtime().block_on(async {
        let (events, _event_receiver) = huddle_test_event_channel();
        let (handle, receiver) = huddle_actor_channel();
        let (_shutdown, shutdown_receiver) = oneshot::channel();
        let actor = tokio::spawn(run_huddle_actor(
            receiver,
            events,
            production_native_join_capability(false),
            shutdown_receiver,
        ));

        assert!(handle.command(HuddleCommand::Dismiss).await.is_err());
        drop(handle);
        actor.await.unwrap();
    });
}

#[test]
fn closed_huddle_actor_propagates_socket_observation_failure() {
    admission_test_runtime().block_on(async {
        let (handle, mut receiver) = huddle_actor_channel();
        receiver.close();
        let message = serde_json::from_value::<SlackMessage>(serde_json::json!({
            "ts": "1.0",
            "room": {
                "id": "R123",
                "date_start": 1,
                "channels": ["C123"]
            }
        }))
        .unwrap();
        let event =
            SocketModeEvent::Message(Box::new(crate::socket_mode::SocketModeMessageEvent {
                channel_id: "C123".into(),
                message,
                kind: SocketModeMessageKind::Posted,
            }));

        assert!(observe_huddle_socket_event(&handle, Some("T123"), &event)
            .await
            .is_err());
    });
}

#[test]
fn huddle_actor_serializes_observation_and_user_commands() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (events, mut event_receiver) = huddle_test_event_channel();
        let (handle, receiver) = huddle_actor_channel();
        let (_shutdown, shutdown_receiver) = oneshot::channel();
        let actor = tokio::spawn(run_huddle_actor(
            receiver,
            events,
            production_native_join_capability(false),
            shutdown_receiver,
        ));
        let huddle = test_huddle("R123");

        handle.observe_huddle(huddle).await.unwrap();
        let discovered = event_receiver.recv().await.unwrap();
        assert!(matches!(
            discovered.kind,
            RuntimeEventKind::Huddle(crate::huddles::state::HuddleEvent::Snapshot(snapshot))
                if snapshot.phase == crate::huddles::state::HuddlePhase::Discovered
        ));

        handle
            .command(crate::huddles::state::HuddleCommand::OpenPreflight {
                call_id: "R123".to_string(),
            })
            .await
            .unwrap();
        let preflight = event_receiver.recv().await.unwrap();
        assert!(matches!(
            preflight.kind,
            RuntimeEventKind::Huddle(crate::huddles::state::HuddleEvent::Snapshot(snapshot))
                if snapshot.phase == crate::huddles::state::HuddlePhase::Preflight
        ));

        drop(handle);
        actor.await.unwrap();
    });
}

#[test]
fn history_huddle_observation_is_chronological_and_workspace_scoped() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (event_sender, mut event_receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender: event_sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(
                RuntimeOperation::Huddle,
                RuntimeTarget::Huddle("active".to_string()),
            ),
            workspace_patch_send_gate: None,
        };
        let (handle, receiver) = huddle_actor_channel();
        let (_shutdown, shutdown_receiver) = oneshot::channel();
        let actor = tokio::spawn(run_huddle_actor(
            receiver,
            events,
            production_native_join_capability(false),
            shutdown_receiver,
        ));
        let messages = serde_json::from_value::<Vec<SlackMessage>>(serde_json::json!([
            {
                "ts": "2.0",
                "room": {"id": "R123", "date_end": 2, "channels": ["C123"]}
            },
            {
                "ts": "1.0",
                "room": {
                    "id": "R123",
                    "date_start": 1,
                    "channels": ["C123"],
                    "participants": ["U123"]
                }
            },
            {
                "ts": "3.0",
                "room": {"id": "R999", "channels": ["C999"]}
            }
        ]))
        .unwrap();

        observe_huddle_messages(&handle, Some("T123"), "C123", &messages)
            .await
            .unwrap();

        let discovered = event_receiver.recv().await.unwrap();
        assert!(matches!(
            discovered.kind,
            RuntimeEventKind::Huddle(HuddleEvent::Snapshot(snapshot))
                if snapshot.phase == HuddlePhase::Discovered
        ));
        let ended = event_receiver.recv().await.unwrap();
        assert!(matches!(
            ended.kind,
            RuntimeEventKind::Huddle(HuddleEvent::Snapshot(snapshot))
                if snapshot.phase == HuddlePhase::Idle
        ));
        assert!(event_receiver.try_recv().is_err());

        drop(handle);
        actor.await.unwrap();
    });
}

#[test]
fn runtime_event_context_uses_loaded_resource_target() {
    let fallback = OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace);
    for event in [
        RuntimeEventKind::ConversationChannelsDiscovered(Vec::new()),
        RuntimeEventKind::ConversationPeopleDiscovered(Vec::new()),
    ] {
        assert_eq!(
            event.operation_context(&fallback),
            OperationContext::new(
                RuntimeOperation::ConversationDiscovery,
                RuntimeTarget::Workspace,
            )
        );
    }

    let event = RuntimeEventKind::HistoryLoadCompleted {
        channel_id: "C123".to_string(),
        has_more: false,
        next_cursor: None,
        append_older: false,
        cached: false,
    };

    assert_eq!(
        event.operation_context(&fallback),
        OperationContext::new(
            RuntimeOperation::History,
            RuntimeTarget::Channel("C123".to_string()),
        )
    );

    let event = RuntimeEventKind::FileLoaded {
        file: Box::new(SlackFile {
            id: Some("F123".to_string()),
            ..Default::default()
        }),
        share_requested: false,
    };
    assert_eq!(
        event.operation_context(&fallback),
        OperationContext::new(
            RuntimeOperation::Files,
            RuntimeTarget::File("F123".to_string()),
        )
    );

    let event = RuntimeEventKind::MessageContextLoadCompleted {
        location: SearchMessageLocation::new(
            "C123",
            "1710000001.000100",
            Some("1710000000.000100"),
        )
        .unwrap(),
        message_timestamps: Vec::new(),
    };
    assert_eq!(
        event.operation_context(&fallback),
        OperationContext::new(
            RuntimeOperation::Thread,
            RuntimeTarget::Thread {
                channel_id: "C123".into(),
                thread_ts: "1710000000.000100".into(),
            },
        )
    );

    let event = RuntimeEventKind::CurrentUserStatusUpdateCompleted {
        user_id: "U123".to_string(),
        cleared: false,
    };
    assert_eq!(
        event.operation_context(&fallback),
        OperationContext::new(RuntimeOperation::UserStatus, RuntimeTarget::Workspace)
    );
}

#[test]
fn socket_mode_reconnect_timing_backs_off_and_resets_after_socket_disconnects() {
    assert_eq!(
        socket_mode_reconnect_timing(SOCKET_MODE_INITIAL_RECONNECT_DELAY, None),
        SocketModeReconnectTiming {
            sleep: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
            next_backoff: Duration::from_secs(2),
        }
    );
    assert_eq!(
        socket_mode_reconnect_timing(Duration::from_secs(20), None),
        SocketModeReconnectTiming {
            sleep: Duration::from_secs(20),
            next_backoff: SOCKET_MODE_MAX_RECONNECT_DELAY,
        }
    );
    assert_eq!(
        socket_mode_reconnect_timing(
            SOCKET_MODE_MAX_RECONNECT_DELAY,
            Some(SocketModeDisconnect::LinkDisabled),
        ),
        SocketModeReconnectTiming {
            sleep: SOCKET_MODE_MAX_RECONNECT_DELAY,
            next_backoff: SOCKET_MODE_MAX_RECONNECT_DELAY,
        }
    );
    assert_eq!(
        socket_mode_reconnect_timing(
            SOCKET_MODE_MAX_RECONNECT_DELAY,
            Some(SocketModeDisconnect::RefreshRequested),
        ),
        SocketModeReconnectTiming {
            sleep: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
            next_backoff: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
        }
    );
    assert_eq!(
        socket_mode_reconnect_timing(Duration::from_secs(20), Some(SocketModeDisconnect::Warning),),
        SocketModeReconnectTiming {
            sleep: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
            next_backoff: SOCKET_MODE_INITIAL_RECONNECT_DELAY,
        }
    );
}

#[test]
fn cached_conversation_user_ids_selects_known_direct_message_members() {
    let conversations = vec![
        SlackConversation {
            id: "D123".to_string(),
            user: Some("U123".to_string()),
            is_im: Some(true),
            ..Default::default()
        },
        SlackConversation {
            id: "D999".to_string(),
            user: Some("U999".to_string()),
            is_im: Some(true),
            ..Default::default()
        },
        SlackConversation {
            id: "C123".to_string(),
            user: Some("U123".to_string()),
            is_channel: Some(true),
            ..Default::default()
        },
    ];
    let user_cache = HashMap::from([("U123".to_string(), "Ada".to_string())]);

    assert_eq!(
        cached_conversation_user_ids(&conversations, &user_cache),
        vec!["U123"]
    );
}

fn channel(id: &str, latest: Option<&str>) -> SlackConversation {
    let mut conversation = SlackConversation {
        id: id.to_string(),
        name: Some(
            id.trim_start_matches("C-")
                .trim_start_matches('C')
                .to_string(),
        ),
        is_channel: Some(true),
        ..Default::default()
    };
    if let Some(latest) = latest {
        conversation
            .extra
            .insert("latest".to_string(), serde_json::json!(latest));
    }
    conversation
}

fn private_channel(id: &str, latest: Option<&str>) -> SlackConversation {
    SlackConversation {
        is_channel: Some(false),
        is_group: Some(true),
        is_private: Some(true),
        ..channel(id, latest)
    }
}

fn archived_channel(id: &str) -> SlackConversation {
    SlackConversation {
        is_archived: Some(true),
        ..channel(id, None)
    }
}

fn dm(id: &str, active: bool) -> SlackConversation {
    let mut conversation = SlackConversation {
        id: id.to_string(),
        user: Some("U123".to_string()),
        is_im: Some(true),
        ..Default::default()
    };
    if active {
        conversation
            .extra
            .insert("is_open".to_string(), serde_json::json!(true));
    }
    conversation
}

#[test]
fn channel_history_prefetch_candidates_rank_open_dms_with_recent_channels() {
    let conversations = vec![
        channel("C-old", None),
        dm("D-active", true),
        dm("D-idle", false),
        archived_channel("C-archived"),
        channel("C-recent", Some("1710000300.000000")),
        channel("C-stale", Some("1710000000.000000")),
        private_channel("G-private", Some("1710000200.000000")),
    ];

    // Without unreads an open DM ranks by activity like any channel. Idle
    // DMs and archived conversations are not prefetched at all.
    assert_eq!(
        channel_history_prefetch_candidates(&conversations),
        vec!["C-recent", "G-private", "C-stale", "C-old", "D-active"]
    );
}

#[test]
fn channel_history_prefetch_candidates_are_bounded() {
    let conversations = (0..CHANNEL_HISTORY_PREFETCH_LIMIT + 3)
        .map(|index| {
            channel(
                &format!("C{index}"),
                Some(&format!("17100000{index:02}.000000")),
            )
        })
        .collect::<Vec<_>>();

    let candidates = channel_history_prefetch_candidates(&conversations);

    assert_eq!(candidates.len(), CHANNEL_HISTORY_PREFETCH_LIMIT);
    assert_eq!(candidates.first().map(String::as_str), Some("C14"));
    assert_eq!(candidates.last().map(String::as_str), Some("C3"));
}

fn message_at(ts: &str) -> SlackMessage {
    SlackMessage {
        ts: ts.to_string(),
        ..Default::default()
    }
}

fn dm_with_unreads(id: &str) -> SlackConversation {
    let mut conversation = dm(id, true);
    conversation.extra.insert(
        "last_read".to_string(),
        serde_json::json!("1710000000.000000"),
    );
    conversation
        .extra
        .insert("latest".to_string(), serde_json::json!("1710000500.000000"));
    conversation
}

fn channels_by_activity(count: usize) -> Vec<SlackConversation> {
    (0..count)
        .map(|index| {
            channel(
                &format!("C{index}"),
                Some(&format!("17100000{index:02}.000000")),
            )
        })
        .collect()
}

#[test]
fn channel_history_prefetch_ranks_unread_dms_above_channels_and_read_dms() {
    let mut conversations = channels_by_activity(CHANNEL_HISTORY_PREFETCH_LIMIT + 3);
    let mut read_dm = dm("D-read", true);
    read_dm.extra.insert(
        "updated".to_string(),
        serde_json::json!("1710009999.000000"),
    );
    conversations.push(read_dm);
    conversations.push(dm_with_unreads("D-unread"));
    let unread = HashSet::from(["D-unread".to_string()]);

    let candidates =
        channel_history_prefetch_candidates_with_huddles(&conversations, &HashSet::new(), &unread);

    assert_eq!(candidates.first().map(String::as_str), Some("D-unread"));
    // The read DM no longer jumps the queue: it ranks by activity alone.
    assert_eq!(candidates.get(1).map(String::as_str), Some("D-read"));
}

#[test]
fn channel_history_prefetch_read_dms_compete_with_channels_by_activity() {
    let mut conversations = channels_by_activity(CHANNEL_HISTORY_PREFETCH_LIMIT);
    conversations.push(dm("D-read-stale", true));

    let candidates = channel_history_prefetch_candidates(&conversations);

    assert_eq!(candidates.len(), CHANNEL_HISTORY_PREFETCH_LIMIT);
    assert!(!candidates.contains(&"D-read-stale".to_string()));
}

#[test]
fn channel_history_prefetch_total_never_exceeds_the_limit() {
    let mut conversations = channels_by_activity(CHANNEL_HISTORY_PREFETCH_LIMIT * 2);
    let mut unread = HashSet::new();
    let mut huddles = HashSet::new();
    for index in 0..CHANNEL_HISTORY_PREFETCH_LIMIT * 2 {
        let id = format!("D{index}");
        conversations.push(dm_with_unreads(&id));
        unread.insert(id);
    }
    for index in 0..5 {
        let id = format!("H{index}");
        conversations.push(channel(&id, None));
        huddles.insert(id);
    }

    let candidates =
        channel_history_prefetch_candidates_with_huddles(&conversations, &huddles, &unread);

    assert_eq!(candidates.len(), CHANNEL_HISTORY_PREFETCH_LIMIT);
    let huddle_count = candidates.iter().filter(|id| id.starts_with('H')).count();
    assert_eq!(huddle_count, 5);
    assert!(candidates[..5].iter().all(|id| id.starts_with('H')));
    assert!(candidates[5..].iter().all(|id| id.starts_with('D')));
}

#[test]
fn conversation_unreads_prefer_the_ledger_and_fall_back_to_the_payload() {
    let payload_unread = dm_with_unreads("D1");
    assert!(conversation_has_unreads(&payload_unread, None));

    let mut payload_read = dm_with_unreads("D2");
    payload_read.extra.insert(
        "last_read".to_string(),
        serde_json::json!("1710000500.000000"),
    );
    assert!(!conversation_has_unreads(&payload_read, None));
    assert!(!conversation_has_unreads(&dm("D3", true), None));

    let read_ledger = ConversationReadState::default();
    assert!(!conversation_has_unreads(
        &payload_unread,
        Some(&read_ledger)
    ));
    let unread_ledger = ConversationReadState {
        server_has_unreads: true,
        ..Default::default()
    };
    assert!(conversation_has_unreads(
        &payload_read,
        Some(&unread_ledger)
    ));
}

#[test]
fn prefetch_skips_history_whose_cache_already_ends_at_latest() {
    let cached = vec![
        message_at("1710000100.000100"),
        message_at("1710000200.000200"),
    ];

    assert!(prefetch_cache_is_current(
        &cached,
        Some("1710000200.000200")
    ));
    assert!(!prefetch_cache_is_current(
        &cached,
        Some("1710000300.000000")
    ));
    assert!(!prefetch_cache_is_current(
        &cached,
        Some("1710000100.000100")
    ));
    // Unknown latest, or an empty cache, always fetches.
    assert!(!prefetch_cache_is_current(&cached, None));
    assert!(!prefetch_cache_is_current(&[], Some("1710000200.000200")));
}

#[test]
fn channel_history_prefetch_always_includes_huddle_metadata() {
    let mut conversations = (0..CHANNEL_HISTORY_PREFETCH_LIMIT + 3)
        .map(|index| {
            channel(
                &format!("C{index}"),
                Some(&format!("17100000{index:02}.000000")),
            )
        })
        .collect::<Vec<_>>();
    let mut huddle = channel("C-huddle", None);
    huddle.extra.insert(
        "properties".to_string(),
        serde_json::json!({"huddles": [{"id": "R123"}]}),
    );
    conversations.push(huddle);

    let candidates = channel_history_prefetch_candidates(&conversations);

    assert_eq!(candidates.first().map(String::as_str), Some("C-huddle"));
    assert!(candidates.contains(&"C-huddle".to_string()));
}

#[test]
fn channel_history_prefetch_keeps_current_huddles_after_cache_redaction() {
    let conversations = (0..CHANNEL_HISTORY_PREFETCH_LIMIT + 3)
        .map(|index| {
            channel(
                &format!("C{index}"),
                Some(&format!("17100000{index:02}.000000")),
            )
        })
        .chain([channel("C-huddle", None)])
        .collect::<Vec<_>>();
    let current_huddle_channels = HashSet::from(["C-huddle".to_string()]);

    let candidates = channel_history_prefetch_candidates_with_huddles(
        &conversations,
        &current_huddle_channels,
        &HashSet::new(),
    );

    assert_eq!(candidates.first().map(String::as_str), Some("C-huddle"));
}

#[test]
fn conversation_priority_ignores_non_finite_values() {
    for priority in ["NaN", "inf", "-inf"] {
        let mut conversation = dm("D1", false);
        conversation
            .extra
            .insert("priority".to_string(), serde_json::json!(priority));

        assert_eq!(conversation.priority_hint(), 0.0);
    }
}

#[test]
fn membership_snapshot_uses_raw_api_state_and_returns_the_coordinator_projection() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-workspace-raw-membership-{}-{nonce}",
        std::process::id()
    ));
    let store = WorkspaceStore::new(directory.clone(), "T1:U1");
    let workspace = WorkspaceReducerAdapter::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let mut cached = channel("C1", Some("10.0"));
        cached.name = Some("cached".to_string());
        cached.is_starred = Some(false);
        cached
            .extra
            .insert("topic".to_string(), serde_json::json!("Cached topic"));
        let removed = SlackConversation {
            id: "C_OLD".to_string(),
            name: Some("removed".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        workspace
            .apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![cached.clone(), removed.clone()],
                    ..Default::default()
                }),
            )
            .unwrap();
        store
            .store_conversations(&[cached.clone(), removed])
            .await
            .unwrap();
        let base_revision = workspace.revision();

        for (origin, conversation) in [
            (
                MutationOrigin::Local,
                SlackConversation {
                    id: "C_LOCAL".to_string(),
                    name: Some("joined locally".to_string()),
                    is_channel: Some(true),
                    ..Default::default()
                },
            ),
            (
                MutationOrigin::Realtime,
                SlackConversation {
                    id: "C_REALTIME".to_string(),
                    name: Some("realtime membership".to_string()),
                    is_channel: Some(true),
                    ..Default::default()
                },
            ),
        ] {
            workspace
                .apply_persisted(
                    Some(&store),
                    origin,
                    WorkspaceMutation::ConversationUpsert(conversation),
                )
                .await
                .unwrap();
        }

        let fresh = vec![
            SlackConversation {
                id: "C1".to_string(),
                name: Some("renamed".to_string()),
                is_channel: Some(true),
                is_starred: Some(false),
                ..Default::default()
            },
            SlackConversation {
                id: "C2".to_string(),
                name: Some("new".to_string()),
                is_channel: Some(true),
                is_starred: Some(true),
                ..Default::default()
            },
        ];
        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: Some(RequestId::new(7)),
            fallback: OperationContext::new(
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
            ),
            workspace_patch_send_gate: None,
        };

        let conversations = apply_conversation_membership_snapshot(
            &events,
            Some(&store),
            &workspace,
            base_revision,
            fresh,
            Some(HashSet::from(["C1".to_string(), "C_LOCAL".to_string()])),
        )
        .await
        .unwrap();

        assert_eq!(
            conversations
                .iter()
                .map(|conversation| conversation.id.as_str())
                .collect::<Vec<_>>(),
            vec!["C1", "C2", "C_LOCAL", "C_REALTIME"]
        );
        let current = conversations
            .iter()
            .find(|conversation| conversation.id == "C1")
            .unwrap();
        assert_eq!(current.name.as_deref(), Some("renamed"));
        assert!(current.is_starred());
        assert_eq!(current.extra["topic"], serde_json::json!("Cached topic"));
        assert!(conversations
            .iter()
            .find(|conversation| conversation.id == "C_LOCAL")
            .unwrap()
            .is_starred());
        assert!(!conversations
            .iter()
            .find(|conversation| conversation.id == "C2")
            .unwrap()
            .is_starred());
        let mut persisted = store.load_conversations().await.unwrap().unwrap();
        persisted.sort_by(|left, right| left.id.cmp(&right.id));
        assert_eq!(persisted, conversations);

        let event = receiver.recv().await.unwrap();
        assert_eq!(event.meta.request, None);
        assert!(matches!(
            event.kind,
            RuntimeEventKind::WorkspacePatch(ref patch)
                if patch.changes().iter().any(|change| matches!(
                    change,
                    WorkspaceChange::ConversationRemoved { channel_id }
                        if channel_id == "C_OLD"
                ))
        ));
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::ConversationsSynchronized
        ));
        assert!(receiver.try_recv().is_err());
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn suspicious_empty_raw_membership_snapshot_does_not_erase_coordinator_state() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let workspace = WorkspaceReducerAdapter::default();
        let retained = channel("C1", None);
        workspace
            .apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![retained.clone()],
                    ..Default::default()
                }),
            )
            .unwrap();
        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: None,
            fallback: OperationContext::new(
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
            ),
            workspace_patch_send_gate: None,
        };
        assert!(apply_conversation_membership_snapshot(
            &events,
            None,
            &workspace,
            workspace.revision(),
            vec![SlackConversation {
                id: "   ".to_string(),
                ..Default::default()
            }],
            Some(HashSet::new()),
        )
        .await
        .is_err());
        assert_eq!(workspace.conversations(), vec![retained]);
        assert!(receiver.try_recv().is_err());

        let empty = WorkspaceReducerAdapter::default();
        assert!(apply_conversation_membership_snapshot(
            &events,
            None,
            &empty,
            empty.revision(),
            Vec::new(),
            Some(HashSet::new()),
        )
        .await
        .unwrap()
        .is_empty());
        assert!(matches!(
            receiver.recv().await.unwrap().kind,
            RuntimeEventKind::ConversationsSynchronized
        ));
        assert!(receiver.try_recv().is_err());
    });
}

#[test]
fn unchanged_membership_snapshot_emits_only_a_state_free_completion() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let workspace = WorkspaceReducerAdapter::default();
        let conversation = SlackConversation {
            id: "C1".to_string(),
            name: Some("general".to_string()),
            is_channel: Some(true),
            ..Default::default()
        };
        workspace
            .apply(
                MutationOrigin::Cache,
                WorkspaceMutation::Hydrate(WorkspaceBootstrapData {
                    conversations: vec![conversation.clone()],
                    ..Default::default()
                }),
            )
            .unwrap();
        let (sender, mut receiver) = runtime_event_channel();
        let events = RuntimeEventSender {
            sender,
            session: SessionId::default().next(),
            request: Some(RequestId::new(3)),
            fallback: OperationContext::new(
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
            ),
            workspace_patch_send_gate: None,
        };

        assert_eq!(
            apply_conversation_membership_snapshot(
                &events,
                None,
                &workspace,
                workspace.revision(),
                vec![conversation.clone()],
                None,
            )
            .await
            .unwrap(),
            vec![conversation]
        );
        let event = receiver.recv().await.unwrap();
        assert_eq!(event.meta.request, Some(RequestId::new(3)));
        assert!(matches!(
            event.kind,
            RuntimeEventKind::ConversationsSynchronized
        ));
        assert!(receiver.try_recv().is_err());
    });
}

#[test]
fn preview_asset_cache_round_trips_raw_workspace_scoped_assets() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before Unix epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "conduit-image-cache-test-{}-{unique}",
        std::process::id()
    ));
    let cache = ImageAssetCache::new(directory.clone());
    let workspace = "T123:U123";
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        assert_eq!(
            cache
                .load(workspace, "https://files.example/image.png")
                .await
                .expect("cache load failed"),
            None
        );

        let image_bytes = b"\x89PNG\r\n\x1a\nraw-image".to_vec();
        let image = cache
            .store(
                workspace,
                "https://files.example/image.png",
                DownloadedPreviewAsset {
                    mime_type: PreviewAssetMime::Png,
                    bytes: image_bytes.clone(),
                },
            )
            .await
            .expect("cache store failed");

        let image_path = cache.path_for_key(
            workspace,
            "https://files.example/image.png",
            PreviewAssetMime::Png,
        );
        let cached = tokio::fs::read(&image_path).await.unwrap();
        assert!(!cached.starts_with(b"data:"));
        assert_eq!(cached, image_bytes);
        assert_eq!(image.path_in(&directory), image_path);
        assert_eq!(image.content_type(), "image/png");
        assert!(!image.is_video());

        assert_eq!(
            cache
                .load(workspace, "https://files.example/image.png")
                .await
                .expect("cache load failed"),
            Some(image.clone())
        );
        assert_eq!(
            cache
                .load("T999:U999", "https://files.example/image.png")
                .await
                .expect("cross-workspace cache load failed"),
            None
        );
        assert_ne!(
            image_path,
            cache.path_for_key(
                "T999:U999",
                "https://files.example/image.png",
                PreviewAssetMime::Png,
            )
        );

        let gif = cache
            .store(
                workspace,
                "https://files.example/animated.gif",
                DownloadedPreviewAsset {
                    mime_type: PreviewAssetMime::Gif,
                    bytes: b"GIF89a-animated-frames".to_vec(),
                },
            )
            .await
            .expect("cache store failed");
        assert_eq!(
            cache
                .load(workspace, "https://files.example/animated.gif")
                .await
                .expect("cache load failed"),
            Some(gif)
        );

        let video = cache
            .store(
                workspace,
                "https://files.example/video.mp4",
                DownloadedPreviewAsset {
                    mime_type: PreviewAssetMime::Mp4,
                    bytes: b"\0\0\0\x18ftypisomvideo".to_vec(),
                },
            )
            .await
            .expect("cache store failed");
        assert!(video.is_video());
        assert_eq!(
            cache
                .load(workspace, "https://files.example/video.mp4")
                .await
                .expect("cache load failed"),
            Some(video)
        );

        let oversized_path = cache.path_for_key(workspace, "oversized", PreviewAssetMime::Png);
        let oversized_file = std::fs::File::create(&oversized_path).unwrap();
        oversized_file
            .set_len(PreviewAssetMime::Png.max_bytes() as u64 + 1)
            .unwrap();
        assert!(cache.load(workspace, "oversized").await.is_err());
    });

    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn preview_asset_store_rolls_back_when_bounds_cannot_be_enforced() {
    let directory = std::env::temp_dir().join(format!(
        "conduit-image-cache-rollback-test-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let cache = ImageAssetCache::new(directory.clone());
    let workspace = "T123:U123";
    let key = "https://files.example/image.png";
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");

    runtime.block_on(async {
        assert!(cache
            .store_with_policy(
                workspace,
                key,
                DownloadedPreviewAsset {
                    mime_type: PreviewAssetMime::Png,
                    bytes: b"\x89PNG\r\n\x1a\nraw-image".to_vec(),
                },
                PreviewCachePolicy {
                    max_age: Duration::MAX,
                    max_bytes: u64::MAX,
                    max_entries: 0,
                },
            )
            .await
            .is_err());
    });

    assert!(!cache
        .path_for_key(workspace, key, PreviewAssetMime::Png)
        .exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn cached_asset_descriptor_rejects_invalid_keys_and_sizes() {
    let workspace_key = "a".repeat(64);
    let cache_key = "b".repeat(64);
    assert!(CachedAssetDescriptor::new(
        workspace_key.clone(),
        cache_key.clone(),
        PreviewAssetMime::Png,
        8,
    )
    .is_some());
    assert!(CachedAssetDescriptor::new(
        "../workspace".to_string(),
        cache_key.clone(),
        PreviewAssetMime::Png,
        8,
    )
    .is_none());
    assert!(
        CachedAssetDescriptor::new(workspace_key, "A".repeat(64), PreviewAssetMime::Png, 8,)
            .is_none()
    );
    assert!(CachedAssetDescriptor::new(
        "a".repeat(64),
        cache_key,
        PreviewAssetMime::Png,
        PreviewAssetMime::Png.max_bytes() as u64 + 1,
    )
    .is_none());
}

#[test]
fn preview_cache_prunes_legacy_files_and_evicts_to_a_byte_cap() {
    let directory = std::env::temp_dir().join(format!(
        "conduit-preview-prune-test-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let workspace = directory.join("a".repeat(64));
    std::fs::create_dir_all(&workspace).unwrap();
    let first = workspace.join(format!("{}.png", "1".repeat(64)));
    let second = workspace.join(format!("{}.png", "2".repeat(64)));
    let protected = workspace.join(format!("{}.png", "3".repeat(64)));
    let legacy = directory.join(format!("{}.data-uri", "4".repeat(64)));
    std::fs::write(&first, b"\x89PNG\r\n\x1a\n1").unwrap();
    std::fs::write(&second, b"\x89PNG\r\n\x1a\n2").unwrap();
    std::fs::write(&protected, b"\x89PNG\r\n\x1a\n3").unwrap();
    std::fs::write(&legacy, b"data:image/png;base64,legacy").unwrap();

    prune_preview_cache(
        &directory,
        Some(&protected),
        PreviewCachePolicy {
            max_age: Duration::MAX,
            max_bytes: 17,
            max_entries: 2,
        },
        SystemTime::now(),
    )
    .unwrap();

    let retained_size = std::fs::read_dir(&workspace)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum::<u64>();
    assert!(!legacy.exists());
    assert!(protected.exists());
    assert!(retained_size <= 17);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn preview_cache_evicts_to_an_entry_cap() {
    let directory = std::env::temp_dir().join(format!(
        "conduit-preview-entry-prune-test-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let workspace = directory.join("a".repeat(64));
    std::fs::create_dir_all(&workspace).unwrap();
    for marker in ['1', '2', '3'] {
        std::fs::write(
            workspace.join(format!("{}.jpg", marker.to_string().repeat(64))),
            b"\xff\xd8\xff",
        )
        .unwrap();
    }

    prune_preview_cache(
        &directory,
        None,
        PreviewCachePolicy {
            max_age: Duration::MAX,
            max_bytes: u64::MAX,
            max_entries: 2,
        },
        SystemTime::now(),
    )
    .unwrap();

    assert_eq!(std::fs::read_dir(&workspace).unwrap().count(), 2);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn preview_cache_reports_an_unenforceable_protected_bound() {
    let directory = std::env::temp_dir().join(format!(
        "conduit-preview-protected-prune-test-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let workspace = directory.join("a".repeat(64));
    std::fs::create_dir_all(&workspace).unwrap();
    let protected = workspace.join(format!("{}.jpg", "1".repeat(64)));
    std::fs::write(&protected, b"\xff\xd8\xff").unwrap();

    assert!(prune_preview_cache(
        &directory,
        Some(&protected),
        PreviewCachePolicy {
            max_age: Duration::MAX,
            max_bytes: u64::MAX,
            max_entries: 0,
        },
        SystemTime::now(),
    )
    .is_err());
    assert!(protected.exists());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn streaming_preview_eviction_is_independent_of_scan_order() {
    for (case, scan_order) in [[2, 1, 0], [0, 1, 2]].into_iter().enumerate() {
        let directory = std::env::temp_dir().join(format!(
            "conduit-preview-order-test-{}-{}-{case}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let sizes = [4_u64, 6, 6];
        let paths = sizes
            .iter()
            .enumerate()
            .map(|(index, size)| {
                let path = directory.join(format!("asset-{index}.jpg"));
                std::fs::write(&path, vec![0_u8; *size as usize]).unwrap();
                path
            })
            .collect::<Vec<_>>();
        let mut retained = BTreeMap::new();
        let mut identities = HashMap::new();
        let mut total = 0;
        let mut eviction_cutoff = None;
        for index in scan_order {
            retain_preview_cache_entry(
                &mut retained,
                &mut identities,
                &mut total,
                &mut eviction_cutoff,
                PreviewCacheEntry {
                    path: paths[index].clone(),
                    workspace_key: "workspace".to_string(),
                    cache_key: format!("key-{index}"),
                    size: sizes[index],
                    last_used: UNIX_EPOCH + Duration::from_secs(index as u64 + 1),
                },
                None,
                PreviewCachePolicy {
                    max_age: Duration::MAX,
                    max_bytes: 10,
                    max_entries: 10,
                },
            )
            .unwrap();
        }

        assert_eq!(
            retained
                .values()
                .map(|entry| entry.cache_key.as_str())
                .collect::<Vec<_>>(),
            vec!["key-2"]
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn streaming_preview_duplicates_keep_the_same_newest_winner() {
    for (case, scan_order) in [[0, 1, 2], [2, 0, 1], [1, 2, 0]].into_iter().enumerate() {
        let directory = std::env::temp_dir().join(format!(
            "conduit-preview-duplicate-test-{}-{}-{case}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let paths = ["png", "jpg", "gif"].map(|extension| {
            let path = directory.join(format!("asset.{extension}"));
            std::fs::write(&path, b"data").unwrap();
            path
        });
        let mut retained = BTreeMap::new();
        let mut identities = HashMap::new();
        let mut total = 0;
        let mut eviction_cutoff = None;
        for index in scan_order {
            retain_preview_cache_entry(
                &mut retained,
                &mut identities,
                &mut total,
                &mut eviction_cutoff,
                PreviewCacheEntry {
                    path: paths[index].clone(),
                    workspace_key: "workspace".to_string(),
                    cache_key: "same-key".to_string(),
                    size: 4,
                    last_used: UNIX_EPOCH + Duration::from_secs(index as u64 + 1),
                },
                None,
                PreviewCachePolicy {
                    max_age: Duration::MAX,
                    max_bytes: 100,
                    max_entries: 10,
                },
            )
            .unwrap();
        }

        assert_eq!(retained.len(), 1);
        assert_eq!(retained.values().next().unwrap().path, paths[2]);
        assert!(paths[2].exists());
        assert!(!paths[0].exists());
        assert!(!paths[1].exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
}

fn conversation_target(channel_id: &str) -> ReadTarget {
    ReadTarget::conversation(channel_id)
}

fn thread_target(channel_id: &str, thread_ts: &str) -> ReadTarget {
    ReadTarget::thread(channel_id, thread_ts)
}

#[test]
fn read_flusher_queue_monotonic_protection() {
    let mut queue = ReadFlusherQueue::new();
    queue.set_last_marked(&conversation_target("C1"), "100.000000");

    // Forward read target accepted
    assert!(queue.enqueue(&conversation_target("C1"), "150.000000"));
    assert_eq!(
        queue.pending_mark(&conversation_target("C1")),
        Some("150.000000")
    );

    // Backward or equal candidate rejected
    assert!(!queue.enqueue(&conversation_target("C1"), "120.000000"));
    assert!(!queue.enqueue(&conversation_target("C1"), "100.000000"));
    assert_eq!(
        queue.pending_mark(&conversation_target("C1")),
        Some("150.000000")
    );
}

#[test]
fn read_flusher_queue_deduplication() {
    let mut queue = ReadFlusherQueue::new();
    queue.set_last_marked(&conversation_target("C1"), "100.000000");

    // First forward enqueue accepted
    assert!(queue.enqueue(&conversation_target("C1"), "150.000000"));

    // Equal or older pending enqueue skipped
    assert!(!queue.enqueue(&conversation_target("C1"), "150.000000"));
    assert!(!queue.enqueue(&conversation_target("C1"), "140.000000"));

    // Newer pending enqueue accepted and collapses prior update
    assert!(queue.enqueue(&conversation_target("C1"), "200.000000"));
    assert_eq!(
        queue.pending_mark(&conversation_target("C1")),
        Some("200.000000")
    );
}

#[test]
fn read_flusher_queue_debounced_batching() {
    let mut queue = ReadFlusherQueue::new();
    queue.enqueue(&conversation_target("C1"), "100.000000");
    queue.enqueue(&conversation_target("C2"), "200.000000");
    queue.enqueue(&conversation_target("C1"), "150.000000");

    let flushed = queue.flush();
    assert_eq!(flushed.len(), 2);
    assert!(flushed.contains(&PendingReadMark {
        target: conversation_target("C1"),
        target_ts: "150.000000".to_string(),
        kind: ReadMarkKind::Read,
    }));
    assert!(flushed.contains(&PendingReadMark {
        target: conversation_target("C2"),
        target_ts: "200.000000".to_string(),
        kind: ReadMarkKind::Read,
    }));

    // last_marked only advances once Slack confirmed the mark
    assert_eq!(queue.last_marked(&conversation_target("C1")), None);
    for mark in &flushed {
        queue.mark_succeeded(mark);
    }
    assert_eq!(
        queue.last_marked(&conversation_target("C1")),
        Some("150.000000")
    );
    assert_eq!(
        queue.last_marked(&conversation_target("C2")),
        Some("200.000000")
    );
    assert!(queue.flush().is_empty());
}

#[test]
fn read_flusher_queue_immediate_channel_flush() {
    let mut queue = ReadFlusherQueue::new();
    queue.enqueue(&conversation_target("C1"), "100.000000");
    queue.enqueue(&conversation_target("C2"), "200.000000");

    let c1_mark = queue.flush_target(&conversation_target("C1"));
    assert_eq!(
        c1_mark,
        Some(PendingReadMark {
            target: conversation_target("C1"),
            target_ts: "100.000000".to_string(),
            kind: ReadMarkKind::Read,
        })
    );
    assert_eq!(queue.last_marked(&conversation_target("C1")), None);
    queue.mark_succeeded(c1_mark.as_ref().unwrap());
    assert_eq!(
        queue.last_marked(&conversation_target("C1")),
        Some("100.000000")
    );

    // C2 remains pending
    assert_eq!(
        queue.pending_mark(&conversation_target("C2")),
        Some("200.000000")
    );
}

#[test]
fn read_flusher_queue_retries_failed_mark() {
    let mut queue = ReadFlusherQueue::new();
    assert!(queue.enqueue(&conversation_target("C1"), "150.000000"));
    let flushed = queue.flush();
    assert_eq!(flushed.len(), 1);

    // A failed mark goes back into the queue and is sent again.
    queue.mark_failed(&flushed[0]);
    assert_eq!(queue.last_marked(&conversation_target("C1")), None);
    let retry = queue.flush();
    assert_eq!(retry, flushed);

    // While the retry is in flight, the same target is not enqueued again.
    assert!(!queue.enqueue(&conversation_target("C1"), "150.000000"));

    // A newer pending target supersedes the failed one.
    assert!(queue.enqueue(&conversation_target("C1"), "200.000000"));
    queue.mark_failed(&retry[0]);
    assert_eq!(
        queue.pending_mark(&conversation_target("C1")),
        Some("200.000000")
    );

    let retried = queue.flush();
    queue.mark_succeeded(&retried[0]);
    assert_eq!(
        queue.last_marked(&conversation_target("C1")),
        Some("200.000000")
    );
    assert!(!queue.enqueue(&conversation_target("C1"), "200.000000"));
}

#[test]
fn read_flusher_queue_rejects_targets_already_in_flight() {
    let mut queue = ReadFlusherQueue::new();
    assert!(queue.enqueue(&conversation_target("C1"), "150.000000"));
    let flushed = queue.flush();
    assert!(!queue.enqueue(&conversation_target("C1"), "150.000000"));
    assert!(!queue.enqueue(&conversation_target("C1"), "100.000000"));
    assert!(queue.enqueue(&conversation_target("C1"), "160.000000"));

    // A failed transient mark is requeued, then superseded by the newer one.
    queue.mark_failed(&flushed[0]);
    assert_eq!(
        queue.pending_mark(&conversation_target("C1")),
        Some("160.000000")
    );

    let flushed = queue.flush();
    queue.mark_succeeded(&flushed[0]);
    assert!(!queue.enqueue(&conversation_target("C1"), "160.000000"));

    let mut queue = ReadFlusherQueue::new();
    assert!(queue.enqueue(&thread_target("C1", "1.0"), "5.0"));
    let flushed = queue.flush();
    assert!(!queue.enqueue(&thread_target("C1", "1.0"), "5.0"));
    queue.mark_failed(&flushed[0]);
    assert_eq!(queue.pending_mark(&thread_target("C1", "1.0")), Some("5.0"));
}

#[test]
fn read_flusher_queue_drops_permanent_failures() {
    let mut queue = ReadFlusherQueue::new();
    queue.enqueue(&conversation_target("C1"), "150.000000");
    let flushed = queue.flush();
    queue.mark_dropped(&flushed[0]);
    assert_eq!(queue.pending_mark(&conversation_target("C1")), None);
    assert!(queue.flush().is_empty());
}

#[test]
fn read_mark_errors_are_classified_permanent_or_transient() {
    let api = |code: &str| {
        anyhow::Error::from(SlackError::Api {
            method: "conversations.mark".to_string(),
            code: code.to_string(),
        })
    };
    for code in [
        "not_in_channel",
        "channel_not_found",
        "is_archived",
        "invalid_auth",
        "not_authed",
        "missing_scope",
        "invalid_ts",
        "thread_not_found",
    ] {
        assert!(read_mark_error_is_permanent(&api(code)), "{code}");
    }
    for code in ["internal_error", "service_unavailable", "ratelimited"] {
        assert!(!read_mark_error_is_permanent(&api(code)), "{code}");
    }
    assert!(read_mark_error_is_permanent(&anyhow::Error::from(
        SlackError::InvalidParameters
    )));
    assert!(!read_mark_error_is_permanent(&anyhow::Error::from(
        SlackError::RateLimited {
            method: "conversations.mark".to_string()
        }
    )));
    assert!(!read_mark_error_is_permanent(&anyhow::anyhow!(
        "connection reset"
    )));
}

#[test]
fn read_flusher_queue_forced_enqueue() {
    let mut queue = ReadFlusherQueue::new();
    queue.set_last_marked(&conversation_target("C1"), "200.000000");

    // Force enqueue allows backward mark for manual overrides
    queue.enqueue_force(&conversation_target("C1"), "100.000000");
    assert_eq!(
        queue.pending_mark(&conversation_target("C1")),
        Some("100.000000")
    );
}

#[test]
fn read_flusher_queue_forced_unread_rewinds_last_marked() {
    let mut queue = ReadFlusherQueue::new();
    assert!(queue.enqueue(&conversation_target("C1"), "20.000000"));
    let read = queue.flush();
    queue.mark_succeeded(&read[0]);
    assert_eq!(
        queue.last_marked(&conversation_target("C1")),
        Some("20.000000")
    );

    // A forced unread to an earlier ts must rewind the confirmed cursor.
    queue.enqueue_force(&conversation_target("C1"), "15.000000");
    let unread = queue.flush();
    assert_eq!(unread[0].kind, ReadMarkKind::Unread);
    queue.mark_succeeded(&unread[0]);
    assert_eq!(
        queue.last_marked(&conversation_target("C1")),
        Some("15.000000")
    );

    // Reading again must reach Slack instead of being deduplicated away.
    assert!(queue.enqueue(&conversation_target("C1"), "20.000000"));
    assert_eq!(
        queue.pending_mark(&conversation_target("C1")),
        Some("20.000000")
    );
}

#[test]
fn read_flusher_queue_read_after_pending_unread_is_not_rejected() {
    let mut queue = ReadFlusherQueue::new();
    assert!(queue.enqueue(&conversation_target("C1"), "20.000000"));
    let read = queue.flush();
    queue.mark_succeeded(&read[0]);

    // Before the unread reaches Slack, a read past it is the newer intent.
    queue.enqueue_force(&conversation_target("C1"), "15.000000");
    assert!(queue.enqueue(&conversation_target("C1"), "18.000000"));
    assert_eq!(
        queue.pending_mark(&conversation_target("C1")),
        Some("18.000000")
    );
    assert_eq!(
        queue.pending_mark_kind(&conversation_target("C1")),
        Some(ReadMarkKind::Read)
    );
}

#[test]
fn read_flusher_queue_read_superseding_pending_unread_rewinds_last_marked() {
    let mut queue = ReadFlusherQueue::new();
    assert!(queue.enqueue(&conversation_target("C1"), "20.000000"));
    let read = queue.flush();
    queue.mark_succeeded(&read[0]);

    // unread:15 is replaced by read:18 before it ever reaches Slack.
    queue.enqueue_force(&conversation_target("C1"), "15.000000");
    assert!(queue.enqueue(&conversation_target("C1"), "18.000000"));
    let flushed = queue.flush();
    assert_eq!(flushed.len(), 1);
    assert_eq!(flushed[0].kind, ReadMarkKind::Read);
    queue.mark_succeeded(&flushed[0]);

    // Slack's cursor is now 18, so 19 must still be markable.
    assert_eq!(
        queue.last_marked(&conversation_target("C1")),
        Some("18.000000")
    );
    assert!(queue.enqueue(&conversation_target("C1"), "19.000000"));
}

#[test]
fn read_flusher_queue_read_after_failed_in_flight_unread_rewinds_last_marked() {
    let mut queue = ReadFlusherQueue::new();
    assert!(queue.enqueue(&conversation_target("C1"), "20.000000"));
    let read = queue.flush();
    queue.mark_succeeded(&read[0]);

    // unread:15 is in flight when the user reads up to 18.
    queue.enqueue_force(&conversation_target("C1"), "15.000000");
    let unread = queue.flush();
    assert!(queue.enqueue(&conversation_target("C1"), "18.000000"));

    // unread:15 fails; the newer read:18 wins and is not clobbered.
    queue.mark_failed(&unread[0]);
    let flushed = queue.flush();
    assert_eq!(flushed[0].target_ts, "18.000000");
    queue.mark_succeeded(&flushed[0]);

    assert_eq!(
        queue.last_marked(&conversation_target("C1")),
        Some("18.000000")
    );
    assert!(queue.enqueue(&conversation_target("C1"), "19.000000"));
}

#[test]
fn read_flusher_queue_failed_read_retry_keeps_newer_unread() {
    let mut queue = ReadFlusherQueue::new();
    assert!(queue.enqueue(&conversation_target("C1"), "20.000000"));
    let in_flight = queue.flush();

    // The user forces unread while read:20 is still in flight.
    queue.enqueue_force(&conversation_target("C1"), "15.000000");

    // read:20 fails transiently; its retry must not clobber unread:15.
    queue.mark_failed(&in_flight[0]);
    assert_eq!(
        queue.pending_mark(&conversation_target("C1")),
        Some("15.000000")
    );
    assert_eq!(
        queue.pending_mark_kind(&conversation_target("C1")),
        Some(ReadMarkKind::Unread)
    );

    let flushed = queue.flush();
    assert_eq!(
        flushed,
        vec![PendingReadMark {
            target: conversation_target("C1"),
            target_ts: "15.000000".to_string(),
            kind: ReadMarkKind::Unread,
        }]
    );
}

#[test]
fn read_flusher_queue_failed_unread_is_retried() {
    let mut queue = ReadFlusherQueue::new();
    assert!(queue.enqueue(&conversation_target("C1"), "20.000000"));
    let read = queue.flush();
    queue.mark_succeeded(&read[0]);

    queue.enqueue_force(&conversation_target("C1"), "15.000000");
    let unread = queue.flush();
    queue.mark_failed(&unread[0]);
    assert_eq!(
        queue.pending_mark(&conversation_target("C1")),
        Some("15.000000")
    );
    assert_eq!(
        queue.pending_mark_kind(&conversation_target("C1")),
        Some(ReadMarkKind::Unread)
    );
}

#[test]
fn read_flusher_queue_thread_monotonic_and_dedup() {
    let mut queue = ReadFlusherQueue::new();
    queue.set_last_marked(&thread_target("C1", "100.000000"), "110.000000");

    // Forward thread target accepted
    assert!(queue.enqueue(&thread_target("C1", "100.000000"), "120.000000"));
    assert_eq!(
        queue.pending_mark(&thread_target("C1", "100.000000")),
        Some("120.000000")
    );

    // Backward or equal candidate rejected
    assert!(!queue.enqueue(&thread_target("C1", "100.000000"), "115.000000"));
    assert!(!queue.enqueue(&thread_target("C1", "100.000000"), "120.000000"));

    // Newer target accepted and replaces pending
    assert!(queue.enqueue(&thread_target("C1", "100.000000"), "130.000000"));
    assert_eq!(
        queue.pending_mark(&thread_target("C1", "100.000000")),
        Some("130.000000")
    );
}

#[test]
fn read_flusher_queue_thread_batching() {
    let mut queue = ReadFlusherQueue::new();
    queue.enqueue(&thread_target("C1", "100.000000"), "110.000000");
    queue.enqueue(&thread_target("C1", "200.000000"), "210.000000");

    let flushed = queue.flush();
    assert_eq!(flushed.len(), 2);
    assert!(flushed.contains(&PendingReadMark {
        target: thread_target("C1", "100.000000"),
        target_ts: "110.000000".to_string(),
        kind: ReadMarkKind::Read,
    }));
    assert!(flushed.contains(&PendingReadMark {
        target: thread_target("C1", "200.000000"),
        target_ts: "210.000000".to_string(),
        kind: ReadMarkKind::Read,
    }));

    assert_eq!(queue.last_marked(&thread_target("C1", "100.000000")), None);
    for mark in &flushed {
        queue.mark_succeeded(mark);
    }
    assert_eq!(
        queue.last_marked(&thread_target("C1", "100.000000")),
        Some("110.000000")
    );
    assert!(queue.flush().is_empty());
}

#[test]
fn read_flusher_queue_thread_forced_unread_rewinds_last_marked() {
    let mut queue = ReadFlusherQueue::new();
    let target = thread_target("C1", "1.000000");
    assert!(queue.enqueue(&target, "20.000000"));
    let read = queue.flush();
    queue.mark_succeeded(&read[0]);

    // A thread unread shares the conversation path, so it rewinds too.
    queue.enqueue_force(&target, "15.000000");
    let unread = queue.flush();
    assert_eq!(unread[0].kind, ReadMarkKind::Unread);
    queue.mark_succeeded(&unread[0]);
    assert_eq!(queue.last_marked(&target), Some("15.000000"));

    assert!(queue.enqueue(&target, "20.000000"));
}

#[test]
fn read_flusher_queue_thread_failed_read_retry_keeps_newer_unread() {
    let mut queue = ReadFlusherQueue::new();
    let target = thread_target("C1", "1.000000");
    assert!(queue.enqueue(&target, "20.000000"));
    let in_flight = queue.flush();

    queue.enqueue_force(&target, "15.000000");
    queue.mark_failed(&in_flight[0]);
    assert_eq!(queue.pending_mark(&target), Some("15.000000"));
    assert_eq!(queue.pending_mark_kind(&target), Some(ReadMarkKind::Unread));
}

#[test]
fn read_flusher_queue_keeps_conversation_and_thread_cursors_apart() {
    let mut queue = ReadFlusherQueue::new();
    let thread = thread_target("C1", "1.000000");
    assert!(queue.enqueue(&thread, "30.000000"));
    assert!(queue.enqueue(&conversation_target("C1"), "20.000000"));

    // Conversations flush before their threads, in a stable order.
    let flushed = queue.flush();
    assert_eq!(flushed[0].target, conversation_target("C1"));
    assert_eq!(flushed[1].target, thread);
    for mark in &flushed {
        queue.mark_succeeded(mark);
    }
    assert_eq!(
        queue.last_marked(&conversation_target("C1")),
        Some("20.000000")
    );
    assert_eq!(queue.last_marked(&thread), Some("30.000000"));
}

#[test]
fn read_flusher_loop_flushes_conversations_and_threads() {
    admission_test_runtime().block_on(async {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let server_addr = server.server_addr().to_ip().unwrap();

        let server_thread = std::thread::spawn(move || {
            // First request: conversations.mark
            let mut req = server.recv().unwrap();
            assert_eq!(req.url(), "/api/conversations.mark");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            assert!(body.contains("channel=C1"));
            assert!(body.contains("ts=150.000000"));
            req.respond(
                tiny_http::Response::from_string(r#"{"ok":true}"#).with_header(
                    tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                ),
            )
            .unwrap();

            // Second request: subscriptions.thread.mark
            let mut req = server.recv().unwrap();
            assert_eq!(req.url(), "/api/subscriptions.thread.mark");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            assert!(body.contains("channel=C1"));
            assert!(body.contains("thread_ts=100.000000"));
            assert!(body.contains("ts=120.000000"));
            req.respond(
                tiny_http::Response::from_string(r#"{"ok":true}"#).with_header(
                    tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                ),
            )
            .unwrap();
        });

        let mut slack = SlackApi::new(StoredToken {
            access_token: "test-token".to_string(),
            ..Default::default()
        });
        slack.api_base_url = format!("http://{server_addr}/api");

        let handle = ReadFlusherHandle::new(
            Arc::new(Mutex::new(ReadFlusherQueue::new())),
            Arc::new(Notify::new()),
        );
        handle.enqueue(&conversation_target("C1"), "150.000000");
        handle.enqueue(&thread_target("C1", "100.000000"), "120.000000");

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let flusher_task = tokio::spawn(run_read_flusher_loop(
            handle.clone(),
            slack,
            Duration::from_millis(10),
            shutdown_rx,
        ));

        // Sleep to allow debounce + flush
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = shutdown_tx.send(());
        flusher_task.await.unwrap();
        server_thread.join().unwrap();
    });
}

#[test]
fn read_flusher_supervisor_lifecycle() {
    admission_test_runtime().block_on(async {
        let handle = ReadFlusherHandle::new(
            Arc::new(Mutex::new(ReadFlusherQueue::new())),
            Arc::new(Notify::new()),
        );
        let slack = SlackApi::new(StoredToken {
            access_token: "test-token".to_string(),
            ..Default::default()
        });
        let session = SessionId(42);
        let mut supervisor =
            ReadFlusherSessionSupervisor::spawn(session, handle, slack, Duration::from_millis(10));
        assert!(supervisor.start());
        supervisor.shutdown().await;
    });
}

#[test]
fn realtime_marked_events_reduce_to_workspace_patches() {
    let workspace = WorkspaceReducerAdapter::default();
    let key = crate::thread_catalog::ThreadKey::new("C1", "100.0").unwrap();
    let mut record = crate::thread_catalog::ThreadRecord::placeholder(key);
    record.reply_count = 1;
    record.latest_reply = Some("110.0".to_string());
    workspace.apply(
        MutationOrigin::WebApi,
        WorkspaceMutation::ThreadCatalogChanged(vec![record]),
    );
    workspace.apply(
        MutationOrigin::WebApi,
        WorkspaceMutation::CountsSnapshot(vec![crate::unread_ledger::ServerReadCounts {
            channel_id: "C1".to_string(),
            last_read: Some("100.0".to_string()),
            latest: Some("150.0".to_string()),
            mention_count: 0,
            unread_count: 1,
            has_unreads: true,
        }]),
    );

    let conv_marked_event = SocketModeEvent::ConversationMarked {
        channel_id: "C1".to_string(),
        ts: "150.0".to_string(),
    };
    let reduction = reduce_realtime_workspace_event(&workspace, &conv_marked_event)
        .expect("conversation marked reduction expected");
    assert!(reduction.patch().changes().iter().any(|c| matches!(
        c,
        crate::workspace_pipeline::WorkspaceChange::ReadStatesChanged(_)
    )));

    let thread_marked_event = SocketModeEvent::ThreadMarked {
        channel_id: "C1".to_string(),
        thread_ts: "100.0".to_string(),
        last_read: None,
    };
    let reduction = reduce_realtime_workspace_event(&workspace, &thread_marked_event)
        .expect("thread marked reduction expected");
    assert!(reduction.patch().changes().iter().any(|c| matches!(
        c,
        crate::workspace_pipeline::WorkspaceChange::ThreadCatalogChanged(_)
    )));
}

fn test_runtime_connection() -> RuntimeConnection {
    let (huddles, _huddle_rx) = huddle_actor_channel();
    let read_flusher = ReadFlusherHandle::new(
        Arc::new(Mutex::new(ReadFlusherQueue::new())),
        Arc::new(Notify::new()),
    );
    RuntimeConnection {
        slack: SlackApi::new(StoredToken {
            access_token: "test-token".to_string(),
            ..Default::default()
        }),
        workspace_url: None,
        workspace_store: None,
        image_cache_scope: "test-scope".to_string(),
        workspace: WorkspaceReducerAdapter::default().with_read_flusher(read_flusher.clone()),
        current_user_id: Some("U_SELF".to_string()),
        user_cache: Arc::new(Mutex::new(HashMap::new())),
        message_handoffs: Arc::new(Mutex::new(MessageHandoffResolver::new(256))),
        conversation_star_sync: ConversationStarSyncGate::default(),
        user_status_sync: UserStatusSync::default(),
        team_id: Some("T1".to_string()),
        huddles,
        read_flusher,
        scheduler: Arc::new(Mutex::new(SyncScheduler::new(
            SchedulerConfig::new(256, 8, 5).unwrap(),
        ))),
        pending_jobs: Arc::new(Mutex::new(HashMap::new())),
        next_job_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        cached_bootstrap_load_gate: None,
    }
}

#[test]
fn read_flusher_queue_caps_marked_targets_at_max_capacity() {
    let mut queue = ReadFlusherQueue::new();
    for index in 0..(READ_FLUSHER_MAX_MARKED_TARGETS + 32) {
        let thread_ts = format!("{}.000000", 100 + index);
        queue.enqueue(&thread_target("C1", &thread_ts), "200.000000");
        let drained = queue.flush();
        assert_eq!(drained.len(), 1);
        queue.mark_succeeded(&drained[0]);
    }

    assert_eq!(queue.last_marked_len(), READ_FLUSHER_MAX_MARKED_TARGETS);
    let newest = format!("{}.000000", 100 + READ_FLUSHER_MAX_MARKED_TARGETS + 31);
    assert!(!queue.enqueue(&thread_target("C1", &newest), "200.000000"));
    assert!(queue.enqueue(&thread_target("C1", "100.000000"), "200.000000"));
}

#[test]
fn schedule_job_internal_removes_coalesced_job_from_pending_jobs() {
    let connection = test_runtime_connection();
    let policy = || SyncJobPolicy {
        priority: SyncPriority::Maintenance,
        durability: SyncDurability::Ephemeral,
        freshness: FreshnessPolicy::Always,
        replacement: ReplacementClass::Refresh(RefreshClass::UserDirectory),
        retry: RetryPolicy::Never,
    };

    let first_id = schedule_job_internal(
        &connection,
        SyncJobPayload::MembershipSync {
            channel_id: "user_directory".to_string(),
        },
        policy(),
        1_000,
    )
    .expect("first job admitted");
    let second_id = schedule_job_internal(
        &connection,
        SyncJobPayload::MembershipSync {
            channel_id: "user_directory".to_string(),
        },
        policy(),
        1_001,
    )
    .expect("second job admitted and coalesced");

    assert_ne!(first_id, second_id);
    let pending = connection.pending_jobs.lock().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(!pending.contains_key(&first_id));
    assert!(pending.contains_key(&second_id));
}

#[test]
fn drive_scheduler_retries_transient_slack_errors_and_cleans_pending_jobs_on_failure() {
    admission_test_runtime().block_on(async {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let server_addr = server.server_addr().to_ip().unwrap();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server_attempts = Arc::clone(&attempts);

        let server_thread = std::thread::spawn(move || {
            for attempt in 0..2 {
                let req = server.recv().unwrap();
                assert!(req.url().starts_with("/api/users.list"));
                server_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = if attempt == 0 {
                    tiny_http::Response::from_string(
                        r#"{"ok":false,"error":"ratelimited","members":[]}"#,
                    )
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                    )
                } else {
                    tiny_http::Response::from_string(
                        r#"{"ok":false,"error":"invalid_auth","members":[]}"#,
                    )
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                    )
                };
                req.respond(response).unwrap();
            }
        });

        let mut connection = test_runtime_connection();
        connection.slack.api_base_url = format!("http://{server_addr}/api");

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let job_id = schedule_job_internal(
            &connection,
            SyncJobPayload::MembershipSync {
                channel_id: "user_directory".to_string(),
            },
            SyncJobPolicy {
                priority: SyncPriority::Maintenance,
                durability: SyncDurability::Ephemeral,
                freshness: FreshnessPolicy::Always,
                replacement: ReplacementClass::Refresh(RefreshClass::UserDirectory),
                retry: RetryPolicy::fixed(2, 10).unwrap(),
            },
            now_ms,
        )
        .expect("job admitted");

        let session = SessionId(1);
        let state = Arc::new(Mutex::new(RuntimeState::new(session)));
        let identity = RuntimeIdentity {
            session,
            request: RequestId::new(1),
        };
        let limits = RuntimeTaskLimits::new(1, 1, 1, 1, 1);
        let (event_tx, _event_rx) = runtime_event_channel();
        let events = RuntimeEventSender::new(
            event_tx,
            identity,
            OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace),
        );

        drive_scheduler(&state, identity, connection.clone(), limits.clone(), events);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let done = connection.pending_jobs.lock().unwrap().is_empty()
                    && attempts.load(std::sync::atomic::Ordering::SeqCst) == 2;
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scheduler retry and permanent failure cleanup timed out");

        let counters = connection.scheduler.lock().unwrap().counters();
        assert_eq!(counters.retried(), 1);
        assert_eq!(counters.failed(), 1);
        assert!(!connection
            .pending_jobs
            .lock()
            .unwrap()
            .contains_key(&job_id));
        server_thread.join().unwrap();
    });
}
