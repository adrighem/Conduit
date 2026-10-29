use super::*;

#[test]
fn font_family_is_extracted_from_pango_descriptions() {
    assert_eq!(
        font_family_from_description("Monospace 10").as_deref(),
        Some("Monospace")
    );
    assert_eq!(
        font_family_from_description("JetBrains Mono Bold 11").as_deref(),
        Some("JetBrains Mono")
    );
    assert_eq!(font_family_from_description("").as_deref(), None);
}

#[test]
fn executable_message_controls_require_fresh_callback_and_publisher_metadata() {
    let mut message: SlackMessage = serde_json::from_value(serde_json::json!({
        "ts": "1710000000.000100",
        "user": "U_BOT",
        "bot_id": "B_APP",
        "app_id": "A_APP",
        "blocks": [{
            "type": "actions",
            "block_id": "request-actions",
            "elements": [{
                "type": "button",
                "action_id": "approve",
                "value": "opaque",
                "text": {"type": "plain_text", "text": "Approve"}
            }]
        }]
    }))
    .expect("synthetic app message should deserialize");
    message.refresh_canonical_content();

    assert_eq!(message_action_control_keys(&message, true).len(), 1);
    assert!(message_action_control_keys(&message, false).is_empty());
    let cached: SlackMessage =
        serde_json::from_str(&serde_json::to_string(&message).expect("message should serialize"))
            .expect("cached message should deserialize");
    assert!(message_action_control_keys(&cached, true).is_empty());

    message.author = crate::rich_message::MessageAuthor::Unknown {
        display_name: "App".to_string(),
    };
    assert!(message_action_control_keys(&message, true).is_empty());
}

#[test]
fn composer_completion_description_announces_person_and_position() {
    let entry = ComposerCompletionEntry::Mention(MentionCandidate {
        user_id: "U1".to_string(),
        display_name: "Ada".to_string(),
        full_name: Some("Ada Lovelace".to_string()),
        username: Some("ada.dev".to_string()),
        search_aliases: Vec::new(),
    });

    assert_eq!(
        composer_completion_description(&entry, 1, 4),
        "Person suggestion 2 of 4: Ada, Ada Lovelace, @ada.dev"
    );
}

#[test]
fn composer_completion_description_announces_group_mentions() {
    let entry = ComposerCompletionEntry::Mention(MentionCandidate {
        user_id: "S1".to_string(),
        display_name: "devs".to_string(),
        full_name: Some("Developers".to_string()),
        username: None,
        search_aliases: Vec::new(),
    });

    assert_eq!(
        composer_completion_description(&entry, 0, 2),
        "Group suggestion 1 of 2: devs, Developers"
    );
}

#[test]
fn navigation_history_keeps_distinct_bounded_visits() {
    let mut history = Vec::new();
    let threads = MainNavigationTarget::Threads;
    remember_navigation(
        &mut history,
        MainNavigationTarget::Conversation("C1".into()),
        &threads,
    );
    remember_navigation(
        &mut history,
        MainNavigationTarget::Conversation("C1".into()),
        &threads,
    );
    assert_eq!(
        history,
        vec![MainNavigationTarget::Conversation("C1".into())]
    );

    for index in 0..=MAX_NAVIGATION_HISTORY {
        remember_navigation(
            &mut history,
            MainNavigationTarget::Conversation(format!("C{index}")),
            &threads,
        );
    }
    assert_eq!(history.len(), MAX_NAVIGATION_HISTORY);
    assert_eq!(
        history.last(),
        Some(&MainNavigationTarget::Conversation("C100".into()))
    );
}

#[test]
fn channel_names_follow_slack_creation_rules() {
    assert!(valid_channel_name("project_alpha-2"));
    assert!(valid_channel_name("  general  "));
    assert!(!valid_channel_name(""));
    assert!(!valid_channel_name("Project Alpha"));
    assert!(!valid_channel_name("127.0.0.1"));
    assert!(!valid_channel_name(&"a".repeat(81)));
}

#[test]
fn lifecycle_presentation_owns_connection_surface_and_status() {
    let cases = [
        (
            WorkspaceLifecycle::Disconnected,
            WorkspaceLifecycleSurface::Connect,
            "Choose a workspace to continue",
            false,
            false,
        ),
        (
            WorkspaceLifecycle::Connecting,
            WorkspaceLifecycleSurface::Loading,
            "Connecting to Slack…",
            false,
            false,
        ),
        (
            WorkspaceLifecycle::Syncing,
            WorkspaceLifecycleSurface::Loading,
            "Syncing workspace…",
            false,
            false,
        ),
        (
            WorkspaceLifecycle::Ready,
            WorkspaceLifecycleSurface::Workspace,
            "",
            false,
            true,
        ),
        (
            WorkspaceLifecycle::Degraded,
            WorkspaceLifecycleSurface::Workspace,
            "Connection interrupted. Retrying…",
            true,
            true,
        ),
        (
            WorkspaceLifecycle::AuthenticationRequired,
            WorkspaceLifecycleSurface::Connect,
            "Slack authentication failed. Sign in again.",
            false,
            false,
        ),
        (
            WorkspaceLifecycle::StartupFailed,
            WorkspaceLifecycleSurface::Connect,
            "Conduit could not start.",
            false,
            false,
        ),
    ];

    for (lifecycle, surface, status, initial_sync_complete, workspace_interactive) in cases {
        assert_eq!(
            workspace_lifecycle_presentation(lifecycle, true, initial_sync_complete),
            WorkspaceLifecyclePresentation {
                surface,
                status,
                workspace_interactive,
            }
        );
    }

    assert_eq!(
        workspace_lifecycle_presentation(WorkspaceLifecycle::Degraded, false, false).surface,
        WorkspaceLifecycleSurface::Connect
    );
}

#[test]
fn initial_sync_uses_movable_loading_surface_but_recovery_keeps_workspace() {
    let initial_sync = workspace_lifecycle_presentation(WorkspaceLifecycle::Syncing, true, false);
    assert_eq!(initial_sync.surface, WorkspaceLifecycleSurface::Loading);
    assert!(!initial_sync.workspace_interactive);

    let ready = workspace_lifecycle_presentation(WorkspaceLifecycle::Ready, true, false);
    assert!(ready.workspace_interactive);

    let recovery = workspace_lifecycle_presentation(WorkspaceLifecycle::Syncing, true, true);
    assert!(recovery.workspace_interactive);
    let degraded = workspace_lifecycle_presentation(WorkspaceLifecycle::Degraded, true, true);
    assert!(degraded.workspace_interactive);

    let initial_failure =
        workspace_lifecycle_presentation(WorkspaceLifecycle::Degraded, true, false);
    assert_eq!(initial_failure.surface, WorkspaceLifecycleSurface::Loading);
    assert!(!initial_failure.workspace_interactive);
}

#[test]
fn initial_sync_completion_is_latched_only_for_the_active_session() {
    assert!(!initial_sync_completion(false, WorkspaceLifecycle::Syncing));
    assert!(initial_sync_completion(false, WorkspaceLifecycle::Ready));
    assert!(initial_sync_completion(true, WorkspaceLifecycle::Syncing));
    assert!(initial_sync_completion(true, WorkspaceLifecycle::Degraded));
    assert!(!initial_sync_completion(
        true,
        WorkspaceLifecycle::AuthenticationRequired
    ));
    assert!(!initial_sync_completion(
        true,
        WorkspaceLifecycle::Disconnected
    ));
}

#[test]
fn repeated_ui_invalidations_require_only_one_scheduled_flush() {
    let mut pending = UiInvalidations::default();
    let schedules = (0..100)
        .filter(|_| pending.insert(UiInvalidations::MAIN | UiInvalidations::THREAD))
        .count();

    assert_eq!(schedules, 1);
    assert!(pending.contains(UiInvalidations::MAIN));
    assert!(pending.contains(UiInvalidations::THREAD));
}

#[test]
fn perf_guard_full_sidebar_renders_only_come_from_the_frame_flush() {
    // A full sidebar render rebuilds every conversation row (40-95 ms with
    // ~1500 conversations). Callers queue `UiInvalidations::SIDEBAR` so a
    // burst of changes costs one render per frame.
    let source = include_str!("../window.rs");
    assert_eq!(
        source.matches("self.render_conversations()").count(),
        1,
        "only flush_ui_invalidations may render the sidebar directly; queue \
         UiInvalidations::SIDEBAR instead"
    );
}

#[test]
fn perf_guard_timeline_renders_register_no_message_control_handles() {
    // Fresh per-render handle tokens made every render context differ, so
    // every re-render rebound every timeline row. Handles are registered on
    // click instead.
    let source = include_str!("../window.rs");
    assert!(!source.contains("replace_message_control_handles("));
    assert!(!source.contains("message_control_handles:"));
}

#[test]
fn perf_guard_synchronous_sidebar_render_clears_only_the_pending_sidebar_flag() {
    let mut pending = UiInvalidations::default();
    pending.insert(UiInvalidations::SIDEBAR | UiInvalidations::MAIN);

    pending.remove(UiInvalidations::SIDEBAR);

    assert!(!pending.contains(UiInvalidations::SIDEBAR));
    assert!(pending.contains(UiInvalidations::MAIN));
    pending.remove(UiInvalidations::MAIN);
    assert!(pending.is_empty());
    pending.remove(UiInvalidations::SIDEBAR);
    assert!(pending.is_empty());
}

#[test]
fn coalesced_ui_invalidation_flush_drains_each_surface_once() {
    let mut pending = UiInvalidations::default();
    assert!(pending.insert(UiInvalidations::SIDEBAR));
    assert!(!pending.insert(UiInvalidations::MAIN | UiInvalidations::PICKER));
    assert!(!pending.insert(UiInvalidations::SIDEBAR | UiInvalidations::TITLE));

    let drained = pending.take();
    for surface in [
        UiInvalidations::SIDEBAR,
        UiInvalidations::MAIN,
        UiInvalidations::TITLE,
        UiInvalidations::PICKER,
    ] {
        assert!(drained.contains(surface));
    }
    assert!(!drained.contains(UiInvalidations::THREAD));
    assert_eq!(pending, UiInvalidations::default());
    assert!(pending.insert(UiInvalidations::THREAD));
}

#[test]
fn media_zoom_scales_below_fit_size_without_distorting_aspect_ratio() {
    assert_eq!(media_zoom_size((1600, 900), (800, 600), 1.0), (800, 450));
    assert_eq!(media_zoom_size((1600, 900), (800, 600), 0.5), (400, 225));
    assert_eq!(media_zoom_size((400, 200), (800, 600), 0.25), (100, 50));
}
use crate::runtime::{RuntimeOperation, RuntimeTarget};
use crate::sidebar::ConversationKind;

#[test]
fn image_asset_key_sets_bound_count_and_bytes() {
    let mut keys = BoundedImageAssetKeys::default();
    for index in 0..=IMAGE_ASSET_KEY_SET_MAX_ENTRIES {
        assert_eq!(
            keys.try_insert(format!("asset-{index}")),
            index < IMAGE_ASSET_KEY_SET_MAX_ENTRIES
        );
    }
    assert_eq!(keys.len(), IMAGE_ASSET_KEY_SET_MAX_ENTRIES);

    let mut oversized = BoundedImageAssetKeys::default();
    assert!(!oversized.try_insert("x".repeat(IMAGE_ASSET_SOURCE_MAX_BYTES + 1)));

    let mut evicting = BoundedImageAssetKeys::default();
    for index in 0..=IMAGE_ASSET_KEY_SET_MAX_ENTRIES {
        assert!(evicting.insert_evicting(format!("asset-{index}")));
    }
    assert_eq!(evicting.len(), IMAGE_ASSET_KEY_SET_MAX_ENTRIES);
    assert!(!evicting.contains("asset-0"));
    assert!(evicting.contains(&format!("asset-{}", IMAGE_ASSET_KEY_SET_MAX_ENTRIES)));

    let mut byte_bounded = BoundedImageAssetKeys::default();
    let entries_by_bytes = IMAGE_ASSET_KEY_SET_MAX_BYTES / IMAGE_ASSET_SOURCE_MAX_BYTES;
    for index in 0..=entries_by_bytes {
        let key = format!(
            "{}{:020}",
            "x".repeat(IMAGE_ASSET_SOURCE_MAX_BYTES - 20),
            index
        );
        assert!(byte_bounded.insert_evicting(key));
    }
    assert_eq!(byte_bounded.len(), entries_by_bytes);
}

fn sidebar_row(id: &str, title: &str) -> SidebarRowModel {
    SidebarRowModel {
        id: id.to_string(),
        title: title.to_string(),
        kind: ConversationKind::DirectMessage,
        unread: false,
        mention_count: 0,
        selected: false,
        starred: false,
        private: true,
        muted: false,
        external: false,
        huddle_active: false,
        user_deleted: false,
        search_aliases: Vec::new(),
        status: None,
        avatar_url: None,
        latest_ts: None,
        has_draft: false,
    }
}

#[test]
fn incremental_sidebar_membership_falls_back_at_the_recent_dm_boundary() {
    let mut direct = sidebar_row("D1", "Ada");
    assert!(!sidebar_row_has_stable_incremental_membership(&direct));

    direct.selected = true;
    assert!(sidebar_row_has_stable_incremental_membership(&direct));
    direct.selected = false;
    direct.starred = true;
    assert!(sidebar_row_has_stable_incremental_membership(&direct));

    direct.starred = false;
    direct.kind = ConversationKind::PublicChannel;
    assert!(sidebar_row_has_stable_incremental_membership(&direct));
}

fn picker_item(id: &str, title: &str) -> ConversationPickerItem {
    ConversationPickerItem {
        row: sidebar_row(id, title),
        action: ConversationPickerAction::OpenConversation,
    }
}

fn keyed_sidebar_row(id: &str, title: &str, selected: bool) -> KeyedSidebarItem {
    let mut row = sidebar_row(id, title);
    row.selected = selected;
    KeyedSidebarItem {
        key: SidebarItemKey::Conversation {
            section: None,
            id: id.to_string(),
        },
        model: SidebarItemModel::Conversation(row),
    }
}

#[test]
fn sidebar_list_store_replaces_only_updated_projection_items() {
    let store = sidebar_list_store();
    let mut projection = SidebarProjection::default();
    let initial = (0..1_430)
        .map(|index| {
            keyed_sidebar_row(
                &format!("C{index}"),
                &format!("Channel {index}"),
                index == 700,
            )
        })
        .collect::<Vec<_>>();
    let operations = projection.reconcile(&initial);
    apply_sidebar_store_operations(&store, projection.items(), &operations);
    let retained_before = store.item(699).unwrap();
    let updated_before = store.item(700).unwrap();

    let mut changed = sidebar_row("C700", "Channel 700");
    changed.selected = true;
    changed.muted = true;
    let operations = projection
        .update_conversation_rows(&[changed])
        .expect("presentation-only update should preserve projection structure");
    apply_sidebar_store_operations(&store, projection.items(), &operations);

    assert_eq!(store.n_items(), 1_430);
    assert_eq!(store.item(699).unwrap(), retained_before);
    assert_ne!(store.item(700).unwrap(), updated_before);
    assert_eq!(sidebar_selected_position(projection.items()), 700);
}

#[test]
fn thread_summary_label_text_pluralizes_responses_and_unread_independently() {
    assert_eq!(
        crate::sidebar::thread_summary_label_text(1, 1),
        "Thread (1 response) (1 unread)"
    );
    assert_eq!(
        crate::sidebar::thread_summary_label_text(1, 0),
        "Thread (1 response) (0 unread)"
    );
    assert_eq!(
        crate::sidebar::thread_summary_label_text(5, 2),
        "Thread (5 responses) (2 unread)"
    );
    assert_eq!(
        crate::sidebar::thread_summary_label_text(0, 0),
        "Thread (0 responses) (0 unread)"
    );
}

#[test]
fn sidebar_list_store_splice_preserves_selection_by_model_identity() {
    let store = sidebar_list_store();
    let mut projection = SidebarProjection::default();
    let initial = vec![
        keyed_sidebar_row("C1", "One", false),
        keyed_sidebar_row("C2", "Two", true),
        keyed_sidebar_row("C3", "Three", false),
    ];
    let operations = projection.reconcile(&initial);
    apply_sidebar_store_operations(&store, projection.items(), &operations);
    let selected_before = store.item(1).unwrap();
    let suffix_before = store.item(2).unwrap();

    let next = vec![
        keyed_sidebar_row("C0", "Zero", false),
        keyed_sidebar_row("C1", "One", false),
        keyed_sidebar_row("C2", "Two", true),
        keyed_sidebar_row("C3", "Three", false),
    ];
    let operations = projection.reconcile(&next);
    apply_sidebar_store_operations(&store, projection.items(), &operations);

    assert_eq!(store.n_items(), 4);
    assert_eq!(store.item(2).unwrap(), selected_before);
    assert_eq!(store.item(3).unwrap(), suffix_before);
    assert_eq!(sidebar_selected_position(projection.items()), 2);
}

#[test]
fn picker_population_flattens_sections_and_preserves_bounded_order() {
    let sections = ConversationPickerSections {
        conversations: (0..30)
            .map(|index| picker_item(&format!("C{index}"), &format!("Channel {index}")))
            .collect(),
        channels: vec![picker_item("C_JOIN", "Join me")],
        people: vec![picker_item("U1", "Ada")],
        search_results: None,
    };
    let entries = conversation_picker_population_entries(&sections);
    assert!(matches!(
        entries.front(),
        Some(ConversationPickerListEntry::Header(title)) if title == "Conversations"
    ));
    assert!(entries.iter().any(
        |entry| matches!(entry, ConversationPickerListEntry::Header(title) if title == "Channels you can join")
    ));
    assert!(entries.iter().any(
        |entry| matches!(entry, ConversationPickerListEntry::Header(title) if title == "People")
    ));

    let expected = entries.iter().cloned().collect::<Vec<_>>();
    let mut population = ConversationPickerPopulation::new(7, entries);
    let mut actual = Vec::new();
    while let Some(batch) = population.next_batch(7) {
        assert!(batch.len() <= PICKER_POPULATION_BATCH_SIZE);
        actual.extend(batch);
    }
    assert_eq!(actual, expected);
}

#[test]
fn picker_population_rejects_stale_generation_without_appending() {
    let sections = ConversationPickerSections {
        search_results: Some(vec![picker_item("C1", "General")]),
        ..Default::default()
    };
    let entries = conversation_picker_population_entries(&sections);
    assert!(entries
        .iter()
        .all(|entry| matches!(entry, ConversationPickerListEntry::Item(_))));

    let mut population = ConversationPickerPopulation::new(4, entries);
    assert_eq!(population.next_batch(5), None);
    assert!(population.is_empty());
}

#[test]
fn request_coordinator_rejects_superseded_and_previous_session_responses() {
    let mut coordinator = RequestCoordinator::default();
    let first = coordinator.begin_session(&RuntimeCommand::SearchMessages {
        query: "first".to_string(),
    });
    let second = coordinator.issue(&RuntimeCommand::SearchMessages {
        query: "second".to_string(),
    });
    let context = OperationContext::new(RuntimeOperation::Search, RuntimeTarget::Workspace);

    assert!(!coordinator.accepts(&RuntimeEventMeta::new(first, context.clone())));
    assert!(coordinator.accepts(&RuntimeEventMeta::new(second, context.clone())));

    let signed_out = coordinator.begin_session(&RuntimeCommand::SignOut);
    assert!(!coordinator.accepts(&RuntimeEventMeta::new(second, context)));
    assert!(!coordinator.accepts(&RuntimeEventMeta {
        session: second.session,
        request: None,
        context: OperationContext::new(RuntimeOperation::SocketMode, RuntimeTarget::Workspace,),
    }));
    assert!(coordinator.accepts(&RuntimeEventMeta::new(
        signed_out,
        OperationContext::new(RuntimeOperation::SignOut, RuntimeTarget::Workspace),
    )));
}

#[test]
fn request_coordinator_checkpoint_restores_rejected_admission() {
    let mut coordinator = RequestCoordinator::default();
    let accepted = coordinator.begin_session(&RuntimeCommand::SearchMessages {
        query: "accepted".to_string(),
    });
    let context = OperationContext::new(RuntimeOperation::Search, RuntimeTarget::Workspace);

    let before_replacement = coordinator.clone();
    let replacement = coordinator.issue(&RuntimeCommand::SearchMessages {
        query: "rejected".to_string(),
    });
    assert!(!coordinator.accepts(&RuntimeEventMeta::new(accepted, context.clone())));
    assert!(coordinator.accepts(&RuntimeEventMeta::new(replacement, context.clone())));
    coordinator = before_replacement;
    assert!(coordinator.accepts(&RuntimeEventMeta::new(accepted, context.clone())));

    let before_session = coordinator.clone();
    let _rejected_session = coordinator.begin_session(&RuntimeCommand::Disconnect);
    assert!(!coordinator.accepts(&RuntimeEventMeta::new(accepted, context.clone())));
    coordinator = before_session;
    assert!(coordinator.accepts(&RuntimeEventMeta::new(accepted, context)));
}

#[test]
fn request_coordinator_accepts_cached_and_fresh_events_for_current_request() {
    let mut coordinator = RequestCoordinator::default();
    let identity = coordinator.begin_session(&RuntimeCommand::LoadHistory {
        channel_id: "C123".to_string(),
    });
    let context = OperationContext::new(
        RuntimeOperation::History,
        RuntimeTarget::Channel("C123".to_string()),
    );

    let cached = RuntimeEventMeta::new(identity, context.clone());
    let fresh = RuntimeEventMeta::new(identity, context);

    assert!(coordinator.accepts(&cached));
    assert!(coordinator.accepts(&fresh));
}

#[test]
fn request_coordinator_accepts_a_session_scoped_recovered_workspace_patch() {
    let mut coordinator = RequestCoordinator::default();
    let first = coordinator.begin_session(&RuntimeCommand::RefreshConversations);
    let second = coordinator.issue(&RuntimeCommand::RefreshConversations);
    let patch = crate::workspace_pipeline::WorkspacePatch::new(
        crate::workspace_pipeline::WorkspaceRevision::INITIAL.successor(),
        vec![
            crate::workspace_pipeline::WorkspaceChange::ConversationUpsert(SlackConversation {
                id: "C1".to_string(),
                ..Default::default()
            }),
        ],
    )
    .unwrap();
    let recovered = RuntimeEvent {
        meta: RuntimeEventMeta {
            session: first.session,
            request: None,
            context: OperationContext::new(
                RuntimeOperation::Conversations,
                RuntimeTarget::Workspace,
            ),
        },
        kind: RuntimeEventKind::WorkspacePatch(patch),
    };

    assert!(!coordinator.accepts(&RuntimeEventMeta::new(
        first,
        OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace)
    )));
    assert!(coordinator.accepts(&RuntimeEventMeta::new(
        second,
        OperationContext::new(RuntimeOperation::Conversations, RuntimeTarget::Workspace)
    )));
    assert!(coordinator.accepts(&recovered.meta));
    let state = WorkspaceSessionState::default();
    let RuntimeEventKind::WorkspacePatch(patch) = recovered.kind else {
        unreachable!();
    };
    state.apply_workspace_patch(&patch).unwrap();
    assert!(state.conversations.borrow().get("C1").is_some());
}

#[test]
fn typed_workspace_patch_applies_on_its_owning_main_context() {
    let context = glib::MainContext::new();
    context.block_on(async {
        assert!(context.is_owner());
        let state = WorkspaceSessionState::default();
        let patch = crate::workspace_pipeline::WorkspacePatch::new(
            crate::workspace_pipeline::WorkspaceRevision::INITIAL.successor(),
            vec![
                crate::workspace_pipeline::WorkspaceChange::ConversationUpsert(SlackConversation {
                    id: "C1".to_string(),
                    name: Some("general".to_string()),
                    ..Default::default()
                }),
            ],
        )
        .unwrap();
        let event = RuntimeEventKind::WorkspacePatch(patch);
        let RuntimeEventKind::WorkspacePatch(patch) = event else {
            unreachable!();
        };

        assert!(state
            .apply_workspace_patch(&patch)
            .unwrap()
            .conversation_changed());
        assert_eq!(
            state
                .conversations
                .borrow()
                .get("C1")
                .and_then(|conversation| conversation.name.as_deref()),
            Some("general")
        );
    });
}

#[test]
fn conversation_sync_completion_initializes_an_empty_workspace_once() {
    assert!(conversation_sync_completion_needs_catalog_sync(false));
    assert!(!conversation_sync_completion_needs_catalog_sync(true));
}

#[test]
fn conversation_catalog_updates_after_typed_patch_adoption() {
    let state = WorkspaceSessionState::default();
    let patch = crate::workspace_pipeline::WorkspacePatch::new(
        crate::workspace_pipeline::WorkspaceRevision::INITIAL.successor(),
        vec![
            crate::workspace_pipeline::WorkspaceChange::ConversationUpsert(SlackConversation {
                id: "C1".to_string(),
                name: Some("authoritative".to_string()),
                ..Default::default()
            }),
        ],
    )
    .unwrap();
    state.apply_workspace_patch(&patch).unwrap();
    state.view.borrow_mut().select_conversation("C1");
    assert_eq!(
        state
            .conversations
            .borrow()
            .get("C1")
            .and_then(|conversation| conversation.name.as_deref()),
        Some("authoritative")
    );
    assert_eq!(state.view.borrow().visible_channel_id(), Some("C1"));
}

#[test]
fn typed_private_conversation_removal_preserves_discovery_cleanup() {
    let state = WorkspaceSessionState::default();
    let private = SlackConversation {
        id: "C_PRIVATE".to_string(),
        is_channel: Some(true),
        is_private: Some(true),
        ..Default::default()
    };
    let public = SlackConversation {
        id: "C_PUBLIC".to_string(),
        is_channel: Some(true),
        ..Default::default()
    };
    let first_revision = crate::workspace_pipeline::WorkspaceRevision::INITIAL.successor();
    state
        .apply_workspace_patch(
            &crate::workspace_pipeline::WorkspacePatch::new(
                first_revision,
                vec![
                    crate::workspace_pipeline::WorkspaceChange::ConversationsReset(vec![
                        private.clone(),
                        public.clone(),
                    ]),
                ],
            )
            .unwrap(),
        )
        .unwrap();
    let removal = state
        .apply_workspace_patch(
            &crate::workspace_pipeline::WorkspacePatch::new(
                first_revision.successor(),
                vec![
                    crate::workspace_pipeline::WorkspaceChange::ConversationRemoved {
                        channel_id: private.id.clone(),
                    },
                ],
            )
            .unwrap(),
        )
        .unwrap();
    let mut discovered = vec![private, public];

    remove_patch_departures_from_discovery(&mut discovered, removal.removals());

    assert_eq!(
        discovered
            .iter()
            .map(|conversation| conversation.id.as_str())
            .collect::<Vec<_>>(),
        vec!["C_PUBLIC"]
    );
}

#[test]
fn request_coordinator_accepts_all_mutation_completions() {
    let mut coordinator = RequestCoordinator::default();
    let first = coordinator.begin_session(&RuntimeCommand::SetSaved {
        channel_id: "C123".to_string(),
        ts: "1.0".to_string(),
        add: true,
        thread_ts: None,
    });
    let second = coordinator.issue(&RuntimeCommand::SetSaved {
        channel_id: "C123".to_string(),
        ts: "2.0".to_string(),
        add: true,
        thread_ts: None,
    });
    let context = OperationContext::new(
        RuntimeOperation::Saved,
        RuntimeTarget::Message {
            channel_id: "C123".to_string(),
            thread_ts: None,
        },
    );

    assert!(coordinator.accepts(&RuntimeEventMeta::new(first, context.clone())));
    assert!(coordinator.accepts(&RuntimeEventMeta::new(second, context)));
}

#[test]
fn startup_runtime_error_is_terminal_for_event_delivery() {
    let event = RuntimeEvent {
        meta: RuntimeEventMeta::new(
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(1),
            },
            OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace),
        ),
        kind: RuntimeEventKind::RuntimeStartFailed(RuntimeFailure::validation(
            "runtime construction failed",
        )),
    };

    assert!(runtime_event_is_start_failure(&event));

    let ordinary_error = RuntimeEvent {
        meta: RuntimeEventMeta::new(
            RuntimeIdentity {
                session: SessionId::default().next(),
                request: RequestId::new(2),
            },
            OperationContext::new(RuntimeOperation::Startup, RuntimeTarget::Workspace),
        ),
        kind: RuntimeEventKind::Error(RuntimeFailure::validation("stored token failed")),
    };
    assert!(!runtime_event_is_start_failure(&ordinary_error));
}

#[test]
fn authentication_failures_always_recover_at_the_session_surface() {
    let failure = RuntimeFailure {
        category: RuntimeFailureCategory::Authentication,
        message: "Sign in again".to_string(),
    };
    let context = OperationContext::new(
        RuntimeOperation::History,
        RuntimeTarget::Channel("C123".to_string()),
    );

    assert_eq!(
        runtime_failure_recovery_for_failure(&context, &failure),
        RuntimeFailureRecovery::Session
    );
}

#[test]
fn runtime_failure_policy_maps_operations_and_targets_to_local_recovery() {
    let channel = RuntimeTarget::Channel("C123".to_string());
    let thread = RuntimeTarget::Thread {
        channel_id: "C123".to_string(),
        thread_ts: "1.0".to_string(),
    };
    let main_message = RuntimeTarget::Message {
        channel_id: "C123".to_string(),
        thread_ts: None,
    };
    let thread_message = RuntimeTarget::Message {
        channel_id: "C123".to_string(),
        thread_ts: Some("1.0".to_string()),
    };
    let exact_message = RuntimeTarget::ExactMessage {
        channel_id: "C123".to_string(),
        ts: "2.0".to_string(),
    };

    let cases = [
        (
            RuntimeOperation::Startup,
            RuntimeTarget::Workspace,
            RuntimeFailureRecovery::Session,
        ),
        (
            RuntimeOperation::Authenticate,
            RuntimeTarget::Workspace,
            RuntimeFailureRecovery::Session,
        ),
        (
            RuntimeOperation::SignOut,
            RuntimeTarget::Workspace,
            RuntimeFailureRecovery::Session,
        ),
        (
            RuntimeOperation::Disconnect,
            RuntimeTarget::Workspace,
            RuntimeFailureRecovery::Session,
        ),
        (
            RuntimeOperation::Conversations,
            RuntimeTarget::Workspace,
            RuntimeFailureRecovery::Sidebar,
        ),
        (
            RuntimeOperation::History,
            channel.clone(),
            RuntimeFailureRecovery::History("C123".to_string()),
        ),
        (
            RuntimeOperation::OlderHistory,
            channel,
            RuntimeFailureRecovery::History("C123".to_string()),
        ),
        (
            RuntimeOperation::Thread,
            thread.clone(),
            RuntimeFailureRecovery::Thread {
                channel_id: "C123".to_string(),
                thread_ts: "1.0".to_string(),
            },
        ),
        (
            RuntimeOperation::OlderThread,
            thread,
            RuntimeFailureRecovery::Thread {
                channel_id: "C123".to_string(),
                thread_ts: "1.0".to_string(),
            },
        ),
        (
            RuntimeOperation::Search,
            RuntimeTarget::Workspace,
            RuntimeFailureRecovery::Search,
        ),
        (
            RuntimeOperation::Files,
            RuntimeTarget::Workspace,
            RuntimeFailureRecovery::Files,
        ),
        (
            RuntimeOperation::Files,
            RuntimeTarget::File("F123".to_string()),
            RuntimeFailureRecovery::Files,
        ),
        (
            RuntimeOperation::SavedItems,
            RuntimeTarget::Workspace,
            RuntimeFailureRecovery::SavedItems,
        ),
        (
            RuntimeOperation::FollowedThreads,
            RuntimeTarget::Workspace,
            RuntimeFailureRecovery::FollowedThreads,
        ),
        (
            RuntimeOperation::User,
            RuntimeTarget::User("U123".to_string()),
            RuntimeFailureRecovery::User("U123".to_string()),
        ),
        (
            RuntimeOperation::ConversationDetails,
            RuntimeTarget::Channel("C123".to_string()),
            RuntimeFailureRecovery::ConversationDetails("C123".to_string()),
        ),
        (
            RuntimeOperation::ConversationEdit,
            RuntimeTarget::Channel("C123".to_string()),
            RuntimeFailureRecovery::ConversationDetails("C123".to_string()),
        ),
        (
            RuntimeOperation::ImageAsset,
            RuntimeTarget::Image("asset".to_string()),
            RuntimeFailureRecovery::Image("asset".to_string()),
        ),
        (
            RuntimeOperation::AttachmentDownload,
            RuntimeTarget::Attachment("https://files.slack.com/file.pdf".to_string()),
            RuntimeFailureRecovery::Attachment,
        ),
        (
            RuntimeOperation::PostMessage,
            main_message.clone(),
            RuntimeFailureRecovery::PostMessage {
                channel_id: "C123".to_string(),
                thread_ts: None,
            },
        ),
        (
            RuntimeOperation::PostMessage,
            thread_message.clone(),
            RuntimeFailureRecovery::PostMessage {
                channel_id: "C123".to_string(),
                thread_ts: Some("1.0".to_string()),
            },
        ),
        (
            RuntimeOperation::ExecuteSlashCommand,
            main_message.clone(),
            RuntimeFailureRecovery::PostMessage {
                channel_id: "C123".to_string(),
                thread_ts: None,
            },
        ),
        (
            RuntimeOperation::ExecuteSlashCommand,
            thread_message.clone(),
            RuntimeFailureRecovery::PostMessage {
                channel_id: "C123".to_string(),
                thread_ts: Some("1.0".to_string()),
            },
        ),
        (
            RuntimeOperation::UpdateMessage,
            exact_message,
            RuntimeFailureRecovery::UpdateMessage {
                channel_id: "C123".to_string(),
                message_ts: "2.0".to_string(),
            },
        ),
        (
            RuntimeOperation::Reaction,
            main_message.clone(),
            RuntimeFailureRecovery::Reaction {
                channel_id: "C123".to_string(),
                thread_ts: None,
            },
        ),
        (
            RuntimeOperation::Saved,
            main_message,
            RuntimeFailureRecovery::Saved {
                channel_id: "C123".to_string(),
                thread_ts: None,
            },
        ),
        (
            RuntimeOperation::ConversationStar,
            RuntimeTarget::Channel("C123".to_string()),
            RuntimeFailureRecovery::ConversationStar,
        ),
        (
            RuntimeOperation::UserStatus,
            RuntimeTarget::Workspace,
            RuntimeFailureRecovery::UserStatus,
        ),
        (
            RuntimeOperation::FileUpload,
            RuntimeTarget::Upload {
                channel_id: "C123".to_string(),
                thread_ts: Some("1.0".to_string()),
            },
            RuntimeFailureRecovery::Upload {
                channel_id: "C123".to_string(),
                thread_ts: Some("1.0".to_string()),
            },
        ),
        (
            RuntimeOperation::SocketMode,
            RuntimeTarget::Workspace,
            RuntimeFailureRecovery::NonDisruptive,
        ),
    ];

    for (operation, target, expected) in cases {
        let context = OperationContext::new(operation, target);
        assert_eq!(runtime_failure_recovery(&context), expected);
    }

    assert_eq!(
        runtime_failure_recovery(&OperationContext::new(
            RuntimeOperation::User,
            RuntimeTarget::Workspace,
        )),
        RuntimeFailureRecovery::NonDisruptive
    );
}

#[test]
fn message_title_opens_details_only_for_channels_and_group_dms() {
    let conversation = |json: serde_json::Value| -> SlackConversation {
        serde_json::from_value(json).expect("conversation fixture")
    };
    let channel = conversation(serde_json::json!({"id": "C1", "is_channel": true}));
    let group = conversation(serde_json::json!({"id": "G1", "is_mpim": true}));
    let direct = conversation(serde_json::json!({"id": "D1", "is_im": true, "user": "U1"}));
    let view = MainMessageView::Conversation;
    assert!(message_title_opens_details(view, false, Some(&channel)));
    assert!(message_title_opens_details(view, false, Some(&group)));
    assert!(!message_title_opens_details(view, false, Some(&direct)));
    assert!(!message_title_opens_details(view, true, Some(&channel)));
    assert!(!message_title_opens_details(view, false, None));
}

#[test]
fn status_permission_failure_explains_profile_reauthorization() {
    let failure = RuntimeFailure {
        category: RuntimeFailureCategory::Validation,
        message: "Slack does not allow this action for this conversation.".to_string(),
    };

    let message = current_user_status_error_message(&failure);
    assert!(message.contains("change your status"));
    assert!(message.contains("profile access"));
    assert!(!message.contains("conversation"));
}

#[test]
fn internal_status_failure_does_not_assume_reauthorization_is_needed() {
    let failure = RuntimeFailure {
        category: RuntimeFailureCategory::Internal,
        message: "unexpected response".to_string(),
    };

    let message = current_user_status_error_message(&failure);
    assert!(message.contains("Try again"));
    assert!(!message.contains("OAuth"));
    assert!(!message.contains("Reconnect"));
}

#[test]
fn mutation_target_unreads_requires_the_channel_and_optional_thread() {
    assert!(mutation_target_is_active(
        Some("C1"),
        Some("T1"),
        "C1",
        None
    ));
    assert!(mutation_target_is_active(
        Some("C1"),
        Some("T1"),
        "C1",
        Some("T1")
    ));
    assert!(!mutation_target_is_active(
        Some("C2"),
        Some("T1"),
        "C1",
        None
    ));
    assert!(!mutation_target_is_active(
        Some("C1"),
        Some("T2"),
        "C1",
        Some("T1")
    ));
    assert!(!mutation_target_is_active(
        Some("C1"),
        None,
        "C1",
        Some("T1")
    ));
}

fn message(ts: &str, text: &str) -> SlackMessage {
    SlackMessage {
        ts: ts.to_string(),
        text: Some(text.to_string()),
        ..Default::default()
    }
}

#[test]
fn earliest_unread_ts_finds_first_message_after_last_read() {
    let messages = [
        message("1710000000.000100", "old"),
        message("1710000003.000100", "newest"),
        message("1710000001.000100", "first unread"),
        message("1710000002.000100", "second unread"),
    ];
    assert_eq!(
        earliest_unread_ts(Some("1710000000.000100"), &messages),
        Some("1710000001.000100".to_string())
    );
}

#[test]
fn earliest_unread_ts_is_none_without_last_read() {
    let messages = [message("1710000000.000100", "hi")];
    assert_eq!(earliest_unread_ts(None, &messages), None);
}

#[test]
fn earliest_unread_ts_is_none_when_everything_is_read() {
    let messages = [message("1710000000.000100", "hi")];
    assert_eq!(
        earliest_unread_ts(Some("1710000005.000100"), &messages),
        None
    );
}

#[test]
fn earliest_unread_ts_is_none_for_empty_messages() {
    assert_eq!(earliest_unread_ts(Some("1710000000.000100"), &[]), None);
}

#[test]
fn sidebar_row_action_lookup_ignores_unregistered_rows() {
    let action = SidebarRowAction {
        channel_id: "C123".to_string(),
        title: "#general".to_string(),
        action: ConversationPickerAction::OpenConversation,
    };
    let mut actions = HashMap::new();
    actions.insert(4, action.clone());

    assert_eq!(sidebar_row_action_for_index(&actions, 3), None);
    assert_eq!(sidebar_row_action_for_index(&actions, 4), Some(action));
}

#[test]
fn sidebar_section_state_toggles_independently() {
    let mut collapsed = HashSet::from([SidebarSectionKind::Channels]);

    toggle_sidebar_section_state(&mut collapsed, SidebarSectionKind::DirectMessages);
    assert!(collapsed.contains(&SidebarSectionKind::Channels));
    assert!(collapsed.contains(&SidebarSectionKind::DirectMessages));

    toggle_sidebar_section_state(&mut collapsed, SidebarSectionKind::Channels);
    assert!(!collapsed.contains(&SidebarSectionKind::Channels));
    assert!(collapsed.contains(&SidebarSectionKind::DirectMessages));
}

#[test]
fn sidebar_section_accessibility_describes_the_available_action() {
    assert_eq!(
        sidebar_section_accessible_label("Channels", false),
        "Collapse Channels"
    );
    assert_eq!(
        sidebar_section_accessible_label("Channels", true),
        "Expand Channels"
    );
}

#[test]
fn sidebar_error_change_preserves_populated_list() {
    assert!(sidebar_error_change_needs_render(false));
    assert!(!sidebar_error_change_needs_render(true));
}

#[test]
fn status_expiration_scheduler_selects_nearest_future_expiration() {
    let statuses = HashMap::from([
        (
            "expired".to_string(),
            SlackUserStatus {
                expiration: 90,
                ..Default::default()
            },
        ),
        (
            "later".to_string(),
            SlackUserStatus {
                expiration: 200,
                ..Default::default()
            },
        ),
        (
            "next".to_string(),
            SlackUserStatus {
                expiration: 150,
                ..Default::default()
            },
        ),
    ]);

    assert_eq!(nearest_status_expiration(&statuses, 100), Some(150));
}

#[test]
fn status_snapshots_replace_stale_values_without_overwriting_newer_users() {
    let current_status = SlackUserStatus {
        text: "Current".to_string(),
        ..Default::default()
    };
    let stale_status = SlackUserStatus {
        text: "Stale".to_string(),
        ..Default::default()
    };
    let new_status = SlackUserStatus {
        text: "New".to_string(),
        ..Default::default()
    };
    let mut statuses = HashMap::from([
        ("U_CHANGED".to_string(), current_status.clone()),
        ("U_REMOVED".to_string(), stale_status.clone()),
    ]);

    let changed = apply_user_status_snapshot(
        &mut statuses,
        HashMap::from([
            ("U_CHANGED".to_string(), stale_status.clone()),
            ("U_NEW".to_string(), new_status.clone()),
        ]),
        true,
        &HashSet::from(["U_CHANGED".to_string()]),
    );

    assert_eq!(statuses.get("U_CHANGED"), Some(&current_status));
    assert_eq!(statuses.get("U_NEW"), Some(&new_status));
    assert!(!statuses.contains_key("U_REMOVED"));
    assert!(!changed.contains(&"U_CHANGED".to_string()));
    assert!(changed.contains(&"U_NEW".to_string()));
    assert!(changed.contains(&"U_REMOVED".to_string()));

    statuses.remove("U_CHANGED");
    apply_user_status_snapshot(
        &mut statuses,
        HashMap::from([
            ("U_CHANGED".to_string(), stale_status),
            ("U_NEW".to_string(), new_status.clone()),
        ]),
        true,
        &HashSet::from(["U_CHANGED".to_string()]),
    );
    assert!(!statuses.contains_key("U_CHANGED"));

    apply_user_status_snapshot(
        &mut statuses,
        HashMap::from([
            ("U_NEW".to_string(), current_status),
            ("U_CACHED".to_string(), new_status.clone()),
        ]),
        false,
        &HashSet::new(),
    );
    assert_eq!(statuses.get("U_NEW"), Some(&new_status));
    assert_eq!(statuses.get("U_CACHED"), Some(&new_status));
}

#[test]
fn current_user_header_prefers_display_name_and_falls_back_to_workspace() {
    let names = HashMap::from([("U123".to_string(), "Vincent".to_string())]);

    assert_eq!(
        current_user_header_title(Some("U123"), &names, Some("Signicat")),
        "Vincent"
    );
    assert_eq!(
        current_user_header_title(Some("U999"), &names, Some("Signicat")),
        "Signicat"
    );
    assert_eq!(
        current_user_header_title(None, &names, None),
        gettext("Workspace")
    );
}

#[test]
fn localized_placeholder_error_templates_are_complete_per_surface() {
    for (surface, title, expected) in [
        (
            PlaceholderSurface::Messages,
            "Messages",
            "Could not load messages. Try again. token <expired>",
        ),
        (
            PlaceholderSurface::SearchResults,
            "Search results",
            "Could not load search results. Try again. token <expired>",
        ),
        (
            PlaceholderSurface::Files,
            "Files",
            "Could not load files. Try again. token <expired>",
        ),
        (
            PlaceholderSurface::SavedItems,
            "Later",
            "Could not load saved items. Try again. token <expired>",
        ),
        (
            PlaceholderSurface::Threads,
            "Threads",
            "Could not load threads. Try again. token <expired>",
        ),
    ] {
        assert_eq!(surface.title(), title);
        assert_eq!(surface.error_message("token <expired>"), expected);
    }

    assert_eq!(
        localized_replies_error("request failed"),
        "Could not load replies. Try again. request failed"
    );
}

#[test]
fn user_projection_invalidations_target_only_affected_surfaces() {
    let dm = SlackConversation {
        id: "D123".to_string(),
        user: Some("U123".to_string()),
        is_im: Some(true),
        ..Default::default()
    };
    let group_dm: SlackConversation = serde_json::from_value(serde_json::json!({
        "id": "G123",
        "is_mpim": true,
        "members": ["U456", "U789"]
    }))
    .expect("failed to parse group direct message");
    let channel = SlackConversation {
        id: "C123".to_string(),
        name: Some("general".to_string()),
        is_channel: Some(true),
        ..Default::default()
    };
    let conversations = vec![dm, group_dm, channel];

    let dm_flags = user_projection_invalidations(
        &conversations,
        &["U123".to_string()],
        Some("U999"),
        Some("D123"),
        false,
    );
    assert!(dm_flags.contains(UiInvalidations::SIDEBAR));
    assert!(dm_flags.contains(UiInvalidations::TITLE));
    assert!(!dm_flags.contains(UiInvalidations::PICKER));

    let group_flags = user_projection_invalidations(
        &conversations,
        &["U456".to_string()],
        None,
        Some("C123"),
        false,
    );
    assert!(group_flags.contains(UiInvalidations::SIDEBAR));
    assert!(!group_flags.contains(UiInvalidations::TITLE));

    let current_user_flags = user_projection_invalidations(
        &conversations,
        &["U999".to_string()],
        Some("U999"),
        Some("C123"),
        false,
    );
    assert_eq!(current_user_flags, UiInvalidations::TITLE);

    let picker_flags =
        user_projection_invalidations(&conversations, &["U000".to_string()], None, None, true);
    assert_eq!(picker_flags, UiInvalidations::PICKER);
}

#[test]
fn browser_session_input_requires_both_tokens() {
    assert_eq!(
        browser_session_input("xoxc-token", "").unwrap_err(),
        "Enter XOXC and XOXD tokens"
    );
    assert_eq!(
        browser_session_input("", "xoxd-token").unwrap_err(),
        "Enter XOXC and XOXD tokens"
    );
}

#[test]
fn browser_session_input_trims_token_values() {
    assert_eq!(
        browser_session_input(" xoxc-token ", " xoxd-token ").unwrap(),
        ("xoxc-token".to_string(), "xoxd-token".to_string())
    );
}

#[test]
fn attention_notification_last_mile_blocks_active_and_muted_conversations() {
    assert!(attention_notification_should_deliver(
        false,
        Some("C123"),
        "C123",
        false
    ));
    assert!(attention_notification_should_deliver(
        true,
        Some("C999"),
        "C123",
        false
    ));
    assert!(!attention_notification_should_deliver(
        true,
        Some("C123"),
        "C123",
        false
    ));
    assert!(!attention_notification_should_deliver(
        false, None, "C123", true
    ));
}

#[test]
fn notification_conversation_waits_for_direct_message_names() {
    let direct_message = SlackConversation {
        id: "D123".into(),
        user: Some("U456".into()),
        is_im: Some(true),
        ..Default::default()
    };
    assert_eq!(
        message_notification_conversation(
            Some(&direct_message),
            &HashMap::new(),
            &HashMap::new(),
            Some("U123"),
        ),
        None
    );
    let resolved = message_notification_conversation(
        Some(&direct_message),
        &HashMap::from([("U456".into(), "Ada".into())]),
        &HashMap::new(),
        Some("U123"),
    )
    .unwrap();
    assert_eq!(resolved, ("Ada".into(), false));
    assert!(!resolved.0.contains("U456"));
}

#[test]
fn notification_conversation_waits_for_every_group_dm_name() {
    let group_message: SlackConversation = serde_json::from_value(serde_json::json!({
        "id": "G123",
        "is_mpim": true,
        "users": ["U123", "U456", "U789"]
    }))
    .unwrap();
    assert!(message_notification_conversation(
        Some(&group_message),
        &HashMap::from([("U456".into(), "Ada".into())]),
        &HashMap::new(),
        Some("U123"),
    )
    .is_none());
    assert_eq!(
        message_notification_conversation(
            Some(&group_message),
            &HashMap::from([
                ("U456".into(), "Ada".into()),
                ("U789".into(), "Grace".into()),
            ]),
            &HashMap::new(),
            Some("U123"),
        ),
        Some(("Ada, Grace".into(), false))
    );
}

#[test]
fn notification_body_uses_fallback_for_empty_message_text() {
    assert_eq!(
        message_notification_body(None, &HashMap::new(), &HashMap::new()),
        Some("New message".into())
    );
    assert_eq!(
        message_notification_body(
            Some(&SlackMessage {
                ts: "1710000100.000000".to_string(),
                text: Some("   ".to_string()),
                ..Default::default()
            }),
            &HashMap::new(),
            &HashMap::new(),
        ),
        Some("New message".into())
    );
    assert_eq!(
        message_notification_body(
            Some(&message("1710000200.000000", "Hello")),
            &HashMap::new(),
            &HashMap::new(),
        ),
        Some("Hello".into())
    );
}

#[test]
fn notification_body_uses_attachment_text() {
    let message = SlackMessage {
        attachments: Some(vec![crate::models::SlackAttachment {
            text: Some("Review with <@U123>".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };

    assert_eq!(
        message_notification_body(
            Some(&message),
            &HashMap::from([("U123".to_string(), "Ada".to_string())]),
            &HashMap::new(),
        )
        .as_deref(),
        Some("Review with @Ada")
    );
}

#[test]
fn channel_notification_content_waits_for_and_includes_sender_display_name() {
    let mut incoming = message("1710000200.000000", "Hello <@U789>");
    incoming.user = Some("U456".into());
    assert_eq!(
        message_notification_content("general", true, &incoming, &HashMap::new(), &HashMap::new()),
        None
    );
    assert_eq!(
        message_notification_content(
            "general",
            true,
            &incoming,
            &HashMap::from([("U456".into(), "Ada".into())]),
            &HashMap::new(),
        ),
        None
    );
    assert_eq!(
        message_notification_content(
            "general",
            true,
            &incoming,
            &HashMap::from([
                ("U456".into(), "Ada".into()),
                ("U789".into(), "Grace".into()),
            ]),
            &HashMap::new(),
        ),
        Some(("general".into(), "Ada: Hello @Grace".into()))
    );
    assert_eq!(
        message_notification_content(
            "general",
            true,
            &incoming,
            &HashMap::from([
                ("U456".into(), "ada".into()),
                ("U789".into(), "grace".into()),
            ]),
            &HashMap::from([
                ("U456".into(), "Ada Lovelace".into()),
                ("U789".into(), "Grace Hopper".into()),
            ]),
        ),
        Some(("general".into(), "Ada Lovelace: Hello @Grace Hopper".into()))
    );
    assert_eq!(
        message_notification_content(
            "Ada",
            false,
            &incoming,
            &HashMap::from([("U789".into(), "Grace".into())]),
            &HashMap::new(),
        ),
        Some(("Ada".into(), "Hello @Grace".into()))
    );
}

#[test]
fn notification_targets_wait_for_the_workspace_and_conversations() {
    let target = NotificationTarget {
        workspace_id: "T123".into(),
        channel_id: "C123".into(),
        thread_ts: Some("1710000000.000100".into()),
    };

    assert_eq!(
        notification_target_resolution(None, false, &target),
        NotificationTargetResolution::Wait
    );
    assert_eq!(
        notification_target_resolution(Some("T123"), false, &target),
        NotificationTargetResolution::Wait
    );
    assert_eq!(
        notification_target_resolution(Some("T123"), true, &target),
        NotificationTargetResolution::Open
    );
    assert_eq!(
        notification_target_resolution(Some("T999"), true, &target),
        NotificationTargetResolution::RejectWorkspace
    );
}

#[test]
fn conversation_targets_select_known_channels_and_open_prospective_dms() {
    let conversations = [
        SlackConversation {
            id: "D999".into(),
            user: Some("U123".into()),
            is_im: Some(true),
            ..Default::default()
        },
        SlackConversation {
            id: "D123".into(),
            user: Some("U123".into()),
            is_im: Some(true),
            ..Default::default()
        },
    ];

    assert_eq!(
        conversation_target_action("C123", &conversations),
        ConversationTargetAction::SelectConversation("C123".into())
    );
    assert_eq!(
        conversation_target_action("U123", &conversations),
        ConversationTargetAction::SelectConversation("D123".into())
    );
    assert_eq!(
        conversation_target_action("U456", &conversations),
        ConversationTargetAction::OpenDirectMessage("U456".into())
    );
}

#[test]
fn workspace_identity_prefers_stable_team_id_with_fallbacks() {
    assert_eq!(
        workspace_identity(&AuthInfo {
            team_id: Some(" T123 ".into()),
            user_id: Some(" U123 ".into()),
            url: Some("https://workspace.slack.com".into()),
            team: Some("Workspace".into()),
            ..Default::default()
        }),
        Some("T123:U123".into())
    );
    assert_eq!(
        workspace_identity(&AuthInfo {
            url: Some("https://workspace.slack.com".into()),
            ..Default::default()
        }),
        Some("https://workspace.slack.com".into())
    );
    assert_eq!(workspace_identity(&AuthInfo::default()), None);
}

#[test]
fn latest_editable_message_skips_newer_foreign_and_ineligible_messages() {
    let messages = vec![
        SlackMessage {
            user: Some("UOTHER".into()),
            text: Some("newer foreign".into()),
            ts: "5.0".into(),
            ..Default::default()
        },
        SlackMessage {
            user: Some("USELF".into()),
            text: Some("thread reply".into()),
            ts: "4.0".into(),
            thread_ts: Some("1.0".into()),
            ..Default::default()
        },
        SlackMessage {
            user: Some("USELF".into()),
            text: Some("unsupported subtype".into()),
            ts: "3.0".into(),
            subtype: Some("channel_join".into()),
            ..Default::default()
        },
        SlackMessage {
            user: Some("USELF".into()),
            text: Some("last editable".into()),
            ts: "2.0".into(),
            ..Default::default()
        },
        SlackMessage {
            user: Some("USELF".into()),
            text: Some("older".into()),
            ts: "1.0".into(),
            ..Default::default()
        },
    ];

    let selected =
        latest_editable_message(ComposerTarget::Message, &messages, Some("USELF")).unwrap();
    assert_eq!(selected.ts, "2.0");
    assert_eq!(
        latest_editable_message(ComposerTarget::Message, &messages, None),
        None
    );

    let selected_thread =
        latest_editable_message(ComposerTarget::Thread, &messages, Some("USELF")).unwrap();
    assert_eq!(selected_thread.ts, "4.0");
    assert_eq!(
        latest_editable_message(ComposerTarget::Thread, &messages, None),
        None
    );
}

#[test]
fn sent_drafts_clear_only_while_the_submitted_text_is_unchanged() {
    assert!(submitted_draft_matches(
        Some(" hello \n"),
        Some("hello"),
        "hello"
    ));
    assert!(submitted_draft_matches(None, Some("hello"), "hello"));
    assert!(!submitted_draft_matches(
        Some("hello, edited"),
        Some("hello"),
        "hello"
    ));
    assert!(!submitted_draft_matches(None, None, "hello"));
}

#[test]
fn completed_upload_removes_sent_attachments_but_keeps_new_draft_edits() {
    let sent = ComposerAttachmentDraft {
        path: PathBuf::from("sent.png"),
        remove_after_upload: true,
    };
    let added_later = ComposerAttachmentDraft {
        path: PathBuf::from("later.mp4"),
        remove_after_upload: false,
    };
    let submitted = RichComposerDraft {
        text: "first message".to_string(),
        attachments: vec![sent.clone()],
        ..Default::default()
    };
    let mut current = RichComposerDraft {
        text: "next message".to_string(),
        attachments: vec![sent, added_later.clone()],
        ..Default::default()
    };

    assert!(remove_submitted_attachments(&mut current, &submitted));
    assert_eq!(current.text, "next message");
    assert_eq!(current.attachments, vec![added_later]);
    assert!(!remove_submitted_attachments(&mut current, &submitted));
}

#[test]
fn pending_draft_deletion_forces_shutdown_persistence() {
    assert!(draft_persist_required(false, true));
    assert!(draft_persist_required(true, false));
    assert!(!draft_persist_required(false, false));
}

#[test]
fn last_conversation_restore_handles_cache_authority_and_navigation_precedence() {
    let conversations = vec![SlackConversation {
        id: "C_RESTORED".to_string(),
        name: Some("restored".to_string()),
        is_channel: Some(true),
        ..Default::default()
    }];
    let restored = LastConversation::new("T_TEST:U_TEST", "C_RESTORED").unwrap();
    let missing = LastConversation::new("T_TEST:U_TEST", "C_MISSING").unwrap();

    assert_eq!(
        last_conversation_restore_decision(
            &restored,
            Some("T_TEST:U_TEST"),
            None,
            false,
            false,
            &conversations,
        ),
        LastConversationRestoreDecision::Select("C_RESTORED".to_string())
    );
    assert_eq!(
        last_conversation_restore_decision(
            &missing,
            Some("T_TEST:U_TEST"),
            None,
            false,
            false,
            &conversations,
        ),
        LastConversationRestoreDecision::Wait
    );
    assert_eq!(
        last_conversation_restore_decision(
            &missing,
            Some("T_TEST:U_TEST"),
            None,
            true,
            false,
            &conversations,
        ),
        LastConversationRestoreDecision::Discard
    );
    assert_eq!(
        last_conversation_restore_decision(
            &restored,
            Some("T_TEST:U_TEST"),
            Some("C_NEW"),
            false,
            false,
            &conversations,
        ),
        LastConversationRestoreDecision::Superseded
    );
    assert_eq!(
        last_conversation_restore_decision(
            &restored,
            Some("T_TEST:U_TEST"),
            None,
            false,
            true,
            &conversations,
        ),
        LastConversationRestoreDecision::Superseded
    );
    assert_eq!(
        last_conversation_restore_decision(
            &restored,
            Some("T_OTHER:U_TEST"),
            None,
            false,
            false,
            &conversations,
        ),
        LastConversationRestoreDecision::Discard
    );
    assert_eq!(
        restored.to_settings_value(),
        ("T_TEST:U_TEST".to_string(), "C_RESTORED".to_string())
    );
    assert_eq!(
        LastConversation::new(" ", "C_RESTORED"),
        None,
        "empty workspace identities are never persisted"
    );
    assert_eq!(
        LastConversation::new("T_TEST:U_TEST", " "),
        None,
        "empty conversation IDs are never persisted"
    );
}

#[test]
fn only_one_submission_can_be_in_flight_for_each_draft() {
    let key = DraftKey::new("T123:U123", "C123", None);
    let other = DraftKey::new("T123:U123", "C999", None);
    let mut pending = HashMap::new();

    assert!(record_draft_submission(&mut pending, key.clone(), "first"));
    assert!(!record_draft_submission(&mut pending, key, "duplicate"));
    assert!(record_draft_submission(&mut pending, other, "parallel"));
    assert_eq!(pending.len(), 2);

    let mut uploads = HashMap::new();
    assert!(record_upload_submission(
        &mut uploads,
        DraftKey::new("T123:U123", "C123", None),
        Some("comment".into())
    ));
    assert!(!record_upload_submission(
        &mut uploads,
        DraftKey::new("T123:U123", "C123", None),
        Some("replacement".into())
    ));
    assert!(record_upload_submission(
        &mut uploads,
        DraftKey::new("T123:U123", "C123", Some("1.0")),
        None
    ));
    assert!(record_upload_submission(
        &mut uploads,
        DraftKey::new("T123:U123", "C999", None),
        None
    ));
}

#[test]
fn clipboard_image_detection_does_not_intercept_text_paste() {
    assert!(clipboard_mime_type_is_image("image/png"));
    assert!(clipboard_mime_type_is_image("image/jpeg; charset=binary"));
    assert!(!clipboard_mime_type_is_image("text/plain"));
    assert!(!clipboard_mime_type_is_image("application/pdf"));
}

#[test]
fn composer_attachment_previews_distinguish_images_videos_and_files() {
    assert_eq!(
        composer_attachment_preview_kind(Path::new("diagram.PNG")),
        ComposerAttachmentPreviewKind::Image
    );
    assert_eq!(
        composer_attachment_preview_kind(Path::new("demo.webm")),
        ComposerAttachmentPreviewKind::Video
    );
    assert_eq!(
        composer_attachment_preview_kind(Path::new("notes.pdf")),
        ComposerAttachmentPreviewKind::File
    );
}

#[test]
fn composer_attachment_previews_skip_images_over_the_decode_budget() {
    let image = Path::new("bomb.gif");
    assert_eq!(
        composer_attachment_preview(image, |_| true),
        ComposerAttachmentPreviewKind::Image
    );
    assert_eq!(
        composer_attachment_preview(image, |_| false),
        ComposerAttachmentPreviewKind::File
    );
    // Only images are probed: other kinds never decode.
    let never_probed = |path: &Path| panic!("probed {}", path.display());
    assert_eq!(
        composer_attachment_preview(Path::new("demo.webm"), never_probed),
        ComposerAttachmentPreviewKind::Video
    );
    assert_eq!(
        composer_attachment_preview(Path::new("notes.pdf"), never_probed),
        ComposerAttachmentPreviewKind::File
    );
}

#[test]
fn stale_staging_cleanup_preserves_files_referenced_by_drafts() {
    let directory = PathBuf::from("/tmp/conduit-staging");
    let retained = directory.join("retained.png");
    let draft = RichComposerDraft {
        attachments: vec![
            ComposerAttachmentDraft {
                path: retained.clone(),
                remove_after_upload: true,
            },
            ComposerAttachmentDraft {
                path: directory.join("source.png"),
                remove_after_upload: false,
            },
            ComposerAttachmentDraft {
                path: PathBuf::from("/tmp/external.png"),
                remove_after_upload: true,
            },
        ],
        ..Default::default()
    };
    let mut drafts = Drafts::default();
    drafts.upsert(
        DraftKey::new("T1:U1", "C1", None),
        &encode_rich_composer_draft(&draft),
    );

    assert_eq!(
        referenced_staged_uploads(&drafts, &directory),
        HashSet::from([retained])
    );
}

#[test]
fn sidebar_leave_action_is_only_available_for_active_channels() {
    let public_channel = SlackConversation {
        is_channel: Some(true),
        ..Default::default()
    };
    let private_channel = SlackConversation {
        is_private: Some(true),
        ..Default::default()
    };
    let direct_message = SlackConversation {
        is_im: Some(true),
        is_private: Some(true),
        ..Default::default()
    };
    let group_direct_message = SlackConversation {
        is_mpim: Some(true),
        is_group: Some(true),
        ..Default::default()
    };
    let archived_channel = SlackConversation {
        is_channel: Some(true),
        is_archived: Some(true),
        ..Default::default()
    };

    assert!(sidebar_conversation_can_leave(&public_channel));
    assert!(sidebar_conversation_can_leave(&private_channel));
    assert!(!sidebar_conversation_can_leave(&direct_message));
    assert!(!sidebar_conversation_can_leave(&group_direct_message));
    assert!(!sidebar_conversation_can_leave(&archived_channel));
    assert!(!sidebar_conversation_leave_requires_confirmation(
        &public_channel
    ));
    assert!(sidebar_conversation_leave_requires_confirmation(
        &private_channel
    ));
}

#[test]
fn sidebar_star_action_toggles_supported_conversations() {
    let public_channel = SlackConversation {
        is_channel: Some(true),
        ..Default::default()
    };
    let starred_direct_message = SlackConversation {
        is_im: Some(true),
        is_starred: Some(true),
        ..Default::default()
    };
    let unsupported = SlackConversation::default();

    let star = sidebar_conversation_star_action(&public_channel).unwrap();
    assert_eq!(star.label(), "Star");
    assert!(star.starred);

    let unstar = sidebar_conversation_star_action(&starred_direct_message).unwrap();
    assert_eq!(unstar.label(), "Unstar");
    assert!(!unstar.starred);

    assert_eq!(sidebar_conversation_star_action(&unsupported), None);
}

#[test]
fn profile_action_targets_only_one_to_one_dm_people() {
    let direct_message = SlackConversation {
        is_im: Some(true),
        user: Some("U123".into()),
        ..Default::default()
    };
    let group_direct_message = SlackConversation {
        is_mpim: Some(true),
        user: Some("U123".into()),
        ..Default::default()
    };
    let channel = SlackConversation {
        is_channel: Some(true),
        user: Some("U123".into()),
        ..Default::default()
    };
    let missing_user = SlackConversation {
        is_im: Some(true),
        ..Default::default()
    };
    let blank_user = SlackConversation {
        is_im: Some(true),
        user: Some("  ".into()),
        ..Default::default()
    };

    let action = conversation_profile_action(&direct_message).unwrap();
    assert_eq!(action.label(), "Profile");
    assert_eq!(action.user_id, "U123");
    assert_eq!(conversation_profile_action(&group_direct_message), None);
    assert_eq!(conversation_profile_action(&channel), None);
    assert_eq!(conversation_profile_action(&missing_user), None);
    assert_eq!(conversation_profile_action(&blank_user), None);

    let title_action = message_title_profile_action(
        MainMessageView::Conversation,
        false,
        false,
        Some(&direct_message),
    )
    .unwrap();
    assert_eq!(title_action.user_id, "U123");
    assert_eq!(message_title_profile_label("Ada"), "Open profile for Ada");
    let status = message_title_profile_status_description("Working remotely");
    assert_eq!(status, "Status: Working remotely");
    assert_eq!(
        message_title_profile_tooltip("Open profile for Ada", Some(&status)),
        "Open profile for Ada\nStatus: Working remotely"
    );
    assert_eq!(
        message_title_profile_tooltip("Open profile for Ada", None),
        "Open profile for Ada"
    );
    assert_eq!(
        message_title_profile_action(
            MainMessageView::Conversation,
            true,
            false,
            Some(&direct_message),
        ),
        None
    );
    assert_eq!(
        message_title_profile_action(
            MainMessageView::Conversation,
            false,
            true,
            Some(&direct_message),
        ),
        None
    );
    for view in [
        MainMessageView::Placeholder,
        MainMessageView::Threads,
        MainMessageView::Search,
        MainMessageView::Files,
        MainMessageView::Saved,
    ] {
        assert_eq!(
            message_title_profile_action(view, false, false, Some(&direct_message)),
            None
        );
    }
}

#[test]
fn profile_completion_requires_visible_matching_request() {
    assert!(accepts_user_profile_completion(true, Some("U123"), "U123"));
    assert!(!accepts_user_profile_completion(
        false,
        Some("U123"),
        "U123"
    ));
    assert!(!accepts_user_profile_completion(true, None, "U123"));
    assert!(!accepts_user_profile_completion(true, Some("U456"), "U123"));
}

#[test]
fn sidebar_context_menu_opens_from_standard_keyboard_shortcuts() {
    assert!(sidebar_context_menu_key(
        gtk::gdk::Key::Menu,
        gtk::gdk::ModifierType::empty(),
    ));
    assert!(sidebar_context_menu_key(
        gtk::gdk::Key::F10,
        gtk::gdk::ModifierType::SHIFT_MASK,
    ));
    assert!(!sidebar_context_menu_key(
        gtk::gdk::Key::F10,
        gtk::gdk::ModifierType::empty(),
    ));
}

#[test]
fn conversation_pane_image_paste_targets_the_originating_pane() {
    let control = gtk::gdk::ModifierType::CONTROL_MASK;
    assert_eq!(
        conversation_pane_image_paste_target(
            ConversationPanePasteFocus::MainPane,
            true,
            gtk::gdk::Key::v,
            control,
        ),
        Some(ComposerTarget::Message)
    );
    assert_eq!(
        conversation_pane_image_paste_target(
            ConversationPanePasteFocus::ThreadPane,
            true,
            gtk::gdk::Key::v,
            control,
        ),
        Some(ComposerTarget::Thread)
    );
}

#[test]
fn conversation_pane_image_paste_excludes_inputs_and_unrelated_widgets() {
    let control = gtk::gdk::ModifierType::CONTROL_MASK;
    for focus in [
        ConversationPanePasteFocus::Composer,
        ConversationPanePasteFocus::TextInput,
        ConversationPanePasteFocus::Outside,
    ] {
        assert_eq!(
            conversation_pane_image_paste_target(focus, true, gtk::gdk::Key::v, control),
            None
        );
    }
}

#[test]
fn conversation_pane_image_paste_preserves_normal_paste_shortcuts() {
    let control = gtk::gdk::ModifierType::CONTROL_MASK;
    let main = ConversationPanePasteFocus::MainPane;
    assert_eq!(
        conversation_pane_image_paste_target(main, false, gtk::gdk::Key::v, control),
        None
    );
    assert_eq!(
        conversation_pane_image_paste_target(
            main,
            true,
            gtk::gdk::Key::v,
            control | gtk::gdk::ModifierType::SHIFT_MASK,
        ),
        None
    );
    assert_eq!(
        conversation_pane_image_paste_target(main, true, gtk::gdk::Key::c, control,),
        None
    );
}

#[test]
fn screenshot_staging_names_are_safe_png_files() {
    let first = screenshot_filename();

    assert!(first.starts_with("Screenshot-"));
    assert!(first.ends_with(".png"));
    assert!(!first.contains('/'));
}

#[test]
fn workspace_navigation_selection_follows_authoritative_main_view() {
    assert_eq!(
        workspace_navigation_selection(MainMessageView::Conversation),
        Some(WorkspaceNavigationSelection::Messages)
    );
    assert_eq!(
        workspace_navigation_selection(MainMessageView::Threads),
        Some(WorkspaceNavigationSelection::Threads)
    );
    assert_eq!(
        workspace_navigation_selection(MainMessageView::Files),
        Some(WorkspaceNavigationSelection::Files)
    );
    assert_eq!(
        workspace_navigation_selection(MainMessageView::Saved),
        Some(WorkspaceNavigationSelection::Saved)
    );
    assert_eq!(
        workspace_navigation_selection(MainMessageView::Placeholder),
        None
    );
    assert_eq!(
        workspace_navigation_selection(MainMessageView::Search),
        None
    );
}

#[test]
fn composer_is_only_visible_for_conversations() {
    assert!(workspace_composer_visible(MainMessageView::Conversation));
    for view in [
        MainMessageView::Placeholder,
        MainMessageView::Threads,
        MainMessageView::Search,
        MainMessageView::Files,
        MainMessageView::Saved,
    ] {
        assert!(!workspace_composer_visible(view));
    }
}

#[test]
fn window_template_preserves_adaptive_and_accessible_boundaries() {
    let template = include_str!("../window.ui");

    for required in [
        "AdwNavigationSplitView\" id=\"workspace_split",
        "AdwOverlaySplitView\" id=\"thread_split",
        "AdwPasswordEntryRow\" id=\"xoxc_token_entry",
        "AdwPasswordEntryRow\" id=\"xoxd_token_entry",
        "AdwToggleGroup\" id=\"navigation_toggle_group",
        "<property name=\"name\">messages</property>",
        "<property name=\"name\">threads</property>",
        "<property name=\"name\">files</property>",
        "<property name=\"name\">saved</property>",
        "GtkSwitch\" id=\"sidebar_unread_filter_switch",
        "Show Only Unread Conversations",
        "GtkLabel\" id=\"message_status_label",
    ] {
        assert!(
            template.contains(required),
            "missing window contract {required}"
        );
    }

    let message_status = template
        .split_once("GtkLabel\" id=\"message_status_label\"")
        .and_then(|(_, rest)| rest.split_once("</object>"))
        .map(|(object, _)| object)
        .expect("message status label should be a complete template object");
    assert!(message_status.contains("<property name=\"accessible-role\">status</property>"));

    let message_title_stack = template
        .find("GtkStack\" id=\"message_title_stack\"")
        .expect("message title should have passive and profile-action modes");
    let message_title = template
        .find("AdwWindowTitle\" id=\"message_title\"")
        .expect("message title should keep a passive presentation");
    let message_title_button = template
        .find("GtkButton\" id=\"message_title_button\"")
        .expect("DM title should have a profile button");
    let message_title_action = template
        .find("AdwWindowTitle\" id=\"message_title_action\"")
        .expect("profile button should preserve title and subtitle layout");
    assert!(message_title_stack < message_title);
    assert!(message_title < message_title_button);
    assert!(message_title_button < message_title_action);
    let title_button_contract = &template[message_title_button..message_title_action];
    assert!(title_button_contract.contains("<class name=\"flat\"/>"));
    assert!(!title_button_contract.contains("accessible-role"));
    let title_action_contract = template[message_title_action..]
        .split_once("</object>")
        .map(|(object, _)| object)
        .expect("profile button title should be a complete template object");
    assert!(title_action_contract.starts_with("AdwWindowTitle\" id=\"message_title_action\""));
    assert!(title_action_contract
        .contains("<property name=\"accessible-role\">presentation</property>"));

    let status_action = template
        .find("<attribute name=\"action\">win.change-status</attribute>")
        .expect("workspace menu should expose the status dialog");
    let new_message_action = template
        .find("<attribute name=\"action\">win.new-message</attribute>")
        .expect("workspace menu should expose new message");
    assert!(status_action < new_message_action);
}

#[test]
fn both_composers_host_the_complete_rich_formatting_bar() {
    let template = include_str!("../window.ui");
    for host in ["message_format_toolbar", "thread_format_toolbar"] {
        assert!(template.contains(host), "missing composer toolbar {host}");
    }
    for breakpoint in ["message_format_breakpoint", "thread_format_breakpoint"] {
        assert!(
            template.contains(&format!("AdwBreakpointBin\" id=\"{breakpoint}")),
            "missing responsive composer toolbar {breakpoint}"
        );
    }
    for overflow in ["message_format_overflow", "thread_format_overflow"] {
        assert!(
            template.contains(overflow),
            "missing toolbar overflow {overflow}"
        );
    }
    assert!(!template.contains("GtkFlowBox\" id=\"message_format_toolbar"));
    assert!(!template.contains("GtkFlowBox\" id=\"thread_format_toolbar"));

    assert_eq!(
        composer_format_controls()
            .iter()
            .map(|control| control.accessible_name)
            .collect::<Vec<_>>(),
        vec![
            "Bold",
            "Italic",
            "Underline",
            "Strikethrough",
            "Inline code",
            "Bulleted list",
            "Numbered list",
            "Quote",
            "Code block",
        ]
    );

    let source = include_str!("../window.rs");
    let setup = source
        .split_once("fn setup_composer_toolbar")
        .and_then(|(_, source)| source.split_once("fn insert_composer_emoji"))
        .map(|(source, _)| source)
        .expect("composer toolbar setup should be bounded");
    assert!(setup.contains("connect_changed"));
    assert!(!setup.contains("idle_add_local_once"));
    assert!(setup.contains("configure_composer_format_control"));
}

#[test]
fn composer_tools_follow_submission_edit_and_draft_state() {
    let source = include_str!("../window.rs");
    let body = source
        .split_once("fn refresh_composer_tool_sensitivity")
        .and_then(|(_, rest)| rest.split_once("fn refresh_composer_placeholders"))
        .map(|(body, _)| body)
        .expect("tool sensitivity should be bounded");
    assert!(body.contains("slash.set_sensitive(idle && !editing && empty)"));
    for control in ["toggle", "emoji", "mention"] {
        assert!(body.contains(&format!("{control}.set_sensitive(idle)")));
    }
    let submission = source
        .split_once("fn set_message_edit_submission_state")
        .and_then(|(_, rest)| rest.split_once("fn sync_message_edit_presentation"))
        .map(|(body, _)| body)
        .expect("submission state should be bounded");
    assert!(submission.contains("refresh_composer_tool_sensitivity(target)"));
    assert!(source.contains("connect_preedit_changed"));
    assert!(source.contains("surface.connect_layout"));
}

#[test]
fn composer_format_controls_are_square_and_overflow_at_exact_width() {
    assert_eq!(COMPOSER_FORMAT_CONTROL_SIZE, 34);
    assert_eq!(composer_format_toolbar_width(0), 0);
    assert_eq!(composer_format_toolbar_width(1), 34);
    assert_eq!(composer_format_toolbar_width(6), 214);
    assert_eq!(composer_format_toolbar_width(10), 358);

    let source = include_str!("../window.rs");
    let sizing = source
        .split_once("fn configure_composer_format_control")
        .and_then(|(_, source)| source.split_once("fn configure_composer_format_menu_control"))
        .map(|(source, _)| source)
        .expect("composer control sizing should be bounded");
    assert!(sizing.contains("set_size_request("));
    assert_eq!(sizing.matches("COMPOSER_FORMAT_CONTROL_SIZE").count(), 2);
    assert!(sizing.contains("composer-format-control"));

    let css = source
        .split_once("fn ensure_composer_format_control_css")
        .and_then(|(_, source)| source.split_once("fn setup_composer_toolbar"))
        .map(|(source, _)| source)
        .expect("composer control CSS setup should be bounded");
    assert!(css.contains("COMPOSER_FORMAT_CONTROL_CSS"));
    assert!(css.contains("style_context_add_provider_for_display"));

    let setup = source
        .split_once("fn setup_composer_toolbar")
        .and_then(|(_, source)| source.split_once("fn insert_composer_emoji"))
        .map(|(source, _)| source)
        .expect("composer toolbar setup should be bounded");
    assert!(setup.contains("composer_format_toolbar_width(compact_control_count)"));
    assert!(setup.contains("composer_format_toolbar_width(full_control_count)"));
    assert!(setup.contains("adw::LengthUnit::Px"));
}

#[test]
fn composer_attachments_stage_in_both_panes_and_submit_as_one_batch() {
    let template = include_str!("../window.ui");
    for required in [
        "message_attachment_previews",
        "thread_attachment_previews",
        "thread_upload_button",
        "thread_upload_progress",
    ] {
        assert!(
            template.contains(required),
            "missing attachment UI {required}"
        );
    }

    let message_composer = template
        .find("GtkBox\" id=\"message_composer")
        .expect("message composer should exist");
    let message_previews = template
        .find("GtkFlowBox\" id=\"message_attachment_previews")
        .expect("message attachment previews should exist");
    let message_entry = template
        .find("GtkTextView\" id=\"message_entry")
        .expect("message entry should exist");
    assert!(message_composer < message_previews && message_previews < message_entry);

    let thread_pane = template
        .find("GtkBox\" id=\"thread_pane")
        .expect("thread pane should exist");
    let thread_previews = template
        .find("GtkFlowBox\" id=\"thread_attachment_previews")
        .expect("thread attachment previews should exist");
    let thread_entry = template
        .find("GtkTextView\" id=\"thread_entry")
        .expect("thread entry should exist");
    assert!(thread_pane < thread_previews && thread_previews < thread_entry);

    let source = include_str!("../window.rs");
    let chooser = source
        .split_once("fn choose_file_for_upload")
        .and_then(|(_, source)| source.split_once("fn connect_image_paste"))
        .map(|(source, _)| source)
        .expect("attachment chooser should be bounded");
    assert!(chooser.contains("stage_composer_attachment"));
    assert!(!chooser.contains("RuntimeCommand::UploadFile"));

    let submit = source
        .split_once("fn post_current_message")
        .and_then(|(_, source)| source.split_once("fn choose_file_for_upload"))
        .map(|(source, _)| source)
        .expect("composer submission should be bounded");
    assert!(submit.contains("RuntimeCommand::UploadFiles"));
}

#[test]
fn composer_success_state_starts_only_after_runtime_admission() {
    let source = include_str!("../window.rs");
    let submit = source
        .split_once("fn submit_composer")
        .and_then(|(_, source)| source.split_once("fn choose_file_for_upload"))
        .map(|(source, _)| source)
        .expect("composer submission should be bounded");

    let slash_admission = submit
        .find("if !self.send_command(RuntimeCommand::ExecuteSlashCommand")
        .expect("slash command execution should check runtime admission");
    let post_admission = submit
        .find("if !self.send_command(RuntimeCommand::PostMessage")
        .expect("message posting should check runtime admission");
    let upload_admission = submit
        .find("if !self.send_command(RuntimeCommand::UploadFiles")
        .expect("file uploading should check runtime admission");
    let upload_progress = submit
        .find("progress.set_visible(true)")
        .expect("admitted upload should reveal progress");

    let slash_branch = &submit[slash_admission..post_admission];
    assert!(slash_branch.contains("self.set_composer_submission_sensitive(target, false)"));

    let post_branch = &submit[post_admission..upload_admission];
    assert!(post_branch.contains("self.set_composer_submission_sensitive(target, false)"));

    assert!(upload_admission < upload_progress);
}

#[test]
fn completed_upload_hides_and_resets_progress() {
    let source = include_str!("../window.rs");
    let completed = source
        .split_once("RuntimeEventKind::FileUploaded")
        .and_then(|(_, source)| source.split_once("log_performance"))
        .map(|(source, _)| source)
        .expect("completed upload handler should be bounded");

    assert!(completed.contains("upload_progress.set_visible(false)"));
    assert!(completed.contains("upload_progress.set_fraction(0.0)"));
    assert!(!completed.contains("set_text(Some(\"Upload complete\"))"));
}

#[test]
fn composer_is_a_card_with_auto_height_and_no_resize_handle() {
    let template = include_str!("../window.ui");
    for required in [
        "GtkRevealer\" id=\"message_format_revealer",
        "GtkRevealer\" id=\"thread_format_revealer",
        "GtkToggleButton\" id=\"message_format_toggle",
        "GtkToggleButton\" id=\"thread_format_toggle",
        "GtkButton\" id=\"message_emoji_button",
        "GtkButton\" id=\"message_mention_button",
        "GtkButton\" id=\"message_slash_button",
        "GtkButton\" id=\"thread_emoji_button",
        "GtkButton\" id=\"thread_mention_button",
        "GtkButton\" id=\"thread_slash_button",
        "GtkLabel\" id=\"message_placeholder",
        "GtkLabel\" id=\"thread_placeholder",
        "GtkScrolledWindow\" id=\"message_scrolled_window",
        "GtkScrolledWindow\" id=\"thread_scrolled_window",
        "class name=\"composer-card\"",
        "property name=\"propagate-natural-height\">True",
    ] {
        assert!(
            template.contains(required),
            "missing composer card UI {required}"
        );
    }
    assert!(!template.contains("composer_resize_handle"));
    assert!(!template.contains("composer-resize-handle"));

    let revealer = template
        .find("id=\"message_format_revealer")
        .expect("format revealer should exist");
    let toolbar = template
        .find("id=\"message_format_toolbar")
        .expect("format toolbar should exist");
    let entry = template
        .find("id=\"message_entry")
        .expect("message entry should exist");
    assert!(revealer < toolbar && toolbar < entry);

    assert_eq!(COMPOSER_MIN_CONTENT_HEIGHT, 34);
    assert_eq!(COMPOSER_FALLBACK_MAX_HEIGHT, 132);
    assert!(COMPOSER_FORMAT_CONTROL_CSS.contains(".composer-card:focus-within"));
    assert!(!COMPOSER_FORMAT_CONTROL_CSS.contains("composer-resize-handle"));

    let source = include_str!("../window.rs");
    assert!(!source.contains(concat!("fn setup_single", "_composer_resizing")));
    assert!(!source.contains(concat!("fn reset_all", "_composer_heights")));

    let cap = source
        .split_once("fn update_composer_max_heights")
        .and_then(|(_, source)| source.split_once("fn composer_attachments"))
        .map(|(source, _)| source)
        .expect("composer height cap should be bounded");
    assert!(cap.contains("* 2 / 3"));
    assert!(cap.contains("max_content_height() != cap"));
}

#[test]
fn submit_composer_guards_against_unsupported_slash_commands() {
    let source = include_str!("../window.rs");
    let submit = source
        .split_once("fn submit_composer")
        .and_then(|(_, source)| source.split_once("fn choose_file_for_upload"))
        .map(|(source, _)| source)
        .expect("composer submission should be bounded");

    let slash_check = submit
        .find("validate_slash_command(&draft.text)")
        .expect("composer submission should validate slash commands");
    let post_admission = submit
        .find("if !self.send_command(RuntimeCommand::PostMessage")
        .expect("message posting should check runtime admission");

    assert!(slash_check < post_admission);
    assert!(submit.contains("Unsupported slash command: /"));
}

#[test]
fn submit_composer_dispatches_supported_slash_commands_via_runtime() {
    let source = include_str!("../window.rs");
    let submit = source
        .split_once("fn submit_composer")
        .and_then(|(_, source)| source.split_once("fn choose_file_for_upload"))
        .map(|(source, _)| source)
        .expect("composer submission should be bounded");

    let slash_dispatch = submit
        .find("if !self.send_command(RuntimeCommand::ExecuteSlashCommand")
        .expect("slash command execution should check runtime admission");
    let post_admission = submit
        .find("if !self.send_command(RuntimeCommand::PostMessage")
        .expect("message posting should check runtime admission");

    assert!(slash_dispatch < post_admission);
    assert!(submit.contains("Executing /"));

    let event_handling = source
        .split_once("RuntimeEventKind::SlashCommandExecuted")
        .and_then(|(_, source)| source.split_once("RuntimeEventKind::MessageUpdateCompleted"))
        .map(|(source, _)| source)
        .expect("slash command event handler should be bounded");

    assert!(event_handling.contains("complete_submitted_draft"));
    assert!(event_handling.contains("set_composer_submission_sensitive"));
}

#[test]
fn thread_sidebar_resize_follows_end_edge_and_clamps() {
    assert_eq!(
        resized_end_sidebar_fraction(400.0, -100.0, 1_000.0),
        Some(0.5)
    );
    assert_eq!(
        resized_end_sidebar_fraction(400.0, 100.0, 1_000.0),
        Some(0.3)
    );
    assert_eq!(
        resized_end_sidebar_fraction(400.0, -1_000.0, 1_000.0),
        Some(THREAD_PANE_MAX_FRACTION)
    );
    assert_eq!(
        resized_end_sidebar_fraction(400.0, 1_000.0, 1_000.0),
        Some(0.2)
    );
    assert_eq!(
        resized_end_sidebar_fraction(THREAD_PANE_MAX_FRACTION * 1_000.0, 0.0, 1_000.0,),
        Some(THREAD_PANE_MAX_FRACTION)
    );
    assert_eq!(resized_end_sidebar_fraction(400.0, 0.0, 0.0), None);
}

#[test]
fn recent_reaction_history_keeps_duplicates_and_latest_uses_up_to_limit() {
    let limit = config::RECENT_REACTION_HISTORY_LIMIT;
    let existing = (0..limit)
        .map(|index| format!("emoji_{index}"))
        .collect::<Vec<_>>();
    let history =
        promoted_recent_reactions(existing.iter().map(String::as_str), existing[0].as_str());
    let mut expected = vec![existing[0].clone()];
    expected.extend(existing[..limit - 1].iter().cloned());

    assert_eq!(history, expected);
    assert_eq!(history.len(), limit);
}

#[test]
fn message_image_requests_include_each_cached_avatar_once() {
    let messages = [
        SlackMessage {
            user: Some("U123".to_string()),
            ..Default::default()
        },
        SlackMessage {
            user: Some("U123".to_string()),
            ..Default::default()
        },
    ];
    let avatar_url = "https://avatars.slack-edge.com/ada.png".to_string();
    let requests = message_image_asset_requests(
        &messages,
        &HashMap::from([("U123".to_string(), avatar_url.clone())]),
        &HashMap::new(),
    );

    assert_eq!(requests, vec![(avatar_url.clone(), avatar_url)]);
}

#[test]
fn message_image_requests_reject_oversized_source_keys() {
    let messages = [SlackMessage {
        user: Some("U123".to_string()),
        ..Default::default()
    }];
    let oversized_url = format!(
        "https://avatars.slack-edge.com/{}",
        "x".repeat(IMAGE_ASSET_SOURCE_MAX_BYTES)
    );

    assert!(message_image_asset_requests(
        &messages,
        &HashMap::from([("U123".to_string(), oversized_url)]),
        &HashMap::new(),
    )
    .is_empty());
}

#[test]
fn image_asset_request_collection_is_entry_bounded() {
    let mut requests = HashMap::new();
    for index in 0..=CONDUIT_ASSET_REGISTRY_MAX_ENTRIES {
        let key = format!("asset-{index}");
        retain_image_asset_request(&mut requests, Some((key.clone(), key)));
    }

    assert_eq!(requests.len(), CONDUIT_ASSET_REGISTRY_MAX_ENTRIES);
    assert!(!requests.contains_key(&format!("asset-{}", CONDUIT_ASSET_REGISTRY_MAX_ENTRIES)));
}

#[test]
fn message_image_requests_include_bot_and_attachment_images() {
    let bot_avatar = "https://avatars.slack-edge.com/bot.png".to_string();
    let attachment_image = "https://files.slack.com/request.png".to_string();
    let messages = [SlackMessage {
        bot_profile: Some(crate::models::SlackBotProfile {
            icons: Some(crate::models::SlackIcons {
                image_72: Some(bot_avatar.clone()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        attachments: Some(vec![crate::models::SlackAttachment {
            image_url: Some(attachment_image.clone()),
            ..Default::default()
        }]),
        ..Default::default()
    }];

    assert_eq!(
        message_image_asset_requests(&messages, &HashMap::new(), &HashMap::new()),
        vec![
            (bot_avatar.clone(), bot_avatar),
            (attachment_image.clone(), attachment_image),
        ]
    );
}

#[test]
fn external_attachment_images_are_requested_through_the_image_proxy() {
    let attachment = |url: &str| SlackMessage {
        attachments: Some(vec![crate::models::SlackAttachment {
            image_url: Some(url.to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let external = "https://images.example.test/card.png";
    let requests =
        message_image_asset_requests(&[attachment(external)], &HashMap::new(), &HashMap::new());
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, external);
    assert!(
        requests[0]
            .1
            .starts_with("https://slack-imgs.com/?c=1&o1=ro&url="),
        "{}",
        requests[0].1
    );
    for private in [
        "http://localhost/a.png",
        "https://10.0.0.1/a.png",
        "http://192.168.1.2/a.png",
    ] {
        assert!(
            message_image_asset_requests(&[attachment(private)], &HashMap::new(), &HashMap::new())
                .is_empty(),
            "{private}"
        );
    }
}

#[test]
fn canonical_link_unfurl_images_remain_native_asset_requests() {
    let image_url = "https://files.slack.com/files-pri/T123-F123/card.png".to_string();
    let message = crate::slack_message_wire::SlackMessageWire::from_value(serde_json::json!({
        "ts": "1710000000.000200",
        "attachments": [{
            "title": "Private preview",
            "image_url": image_url
        }]
    }))
    .into_message()
    .expect("message should normalize");
    let message = crate::slack_message_wire::normalize_cached_message(message);

    assert!(message.attachments.is_none());
    assert_eq!(
        message_image_asset_requests(&[message], &HashMap::new(), &HashMap::new()),
        vec![(image_url.clone(), image_url)]
    );
}

/// The native renderer shows a download placeholder only for URLs it
/// expects to land; every such URL must actually be requested here.
#[test]
fn document_images_are_requested_exactly_when_the_renderer_waits_for_them() {
    let message = crate::slack_message_wire::normalize_cached_message(
        crate::slack_message_wire::SlackMessageWire::from_value(serde_json::json!({
            "ts": "1710000003.000100",
            "text": "mixed media",
            "blocks": [
                {"type": "image", "alt_text": "chart",
                 "image_url": "https://cdn.example.test/chart.png"},
                {"type": "image", "alt_text": "shot",
                 "image_url": "https://files.slack.com/files-pri/T1-F1/shot.png"},
                {"type": "context", "elements": [
                    {"type": "image", "alt_text": "icon",
                     "image_url": "https://a.slack-edge.com/icon.png"},
                    {"type": "image", "alt_text": "ext icon",
                     "image_url": "https://example.test/icon.png"},
                    {"type": "image", "alt_text": "local",
                     "image_url": "http://localhost/icon.png"}
                ]}
            ],
            "attachments": [
                {"title": "Site", "image_url": "https://example.test/og.png",
                 "footer_icon": "https://example.test/favicon.ico"},
                {"title": "GIF", "image_url": "https://media1.giphy.com/media/a/giphy.gif"}
            ]
        }))
        .into_message()
        .expect("message should normalize"),
    );
    let requests = message_image_asset_requests([&message], &HashMap::new(), &HashMap::new());
    let requested: HashSet<String> = requests.iter().map(|(key, _)| key.clone()).collect();
    let urls: Vec<&str> = message.document.image_urls().collect();
    assert_eq!(urls.len(), 8);

    // External images keep their original URL as cache key but are
    // fetched through the proxy; allowlisted ones are fetched directly.
    let fetch_for = |key: &str| {
        requests
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, fetch)| fetch.as_str())
    };
    assert_eq!(
        fetch_for("https://files.slack.com/files-pri/T1-F1/shot.png"),
        Some("https://files.slack.com/files-pri/T1-F1/shot.png")
    );
    let proxied = fetch_for("https://example.test/og.png").expect("og image requested");
    assert!(
        proxied.starts_with("https://slack-imgs.com/?c=1&o1=ro&url="),
        "{proxied}"
    );
    assert_eq!(fetch_for("http://localhost/icon.png"), None);

    for url in urls {
        let image = crate::rich_message::MessageImage::new(Some(url.to_string()), "", None);
        assert_eq!(
            crate::timeline_media::fetchable_url(&image).is_some(),
            requested.contains(url),
            "{url}"
        );
    }
}

#[test]
fn gif_file_requests_animated_thumbnail_instead_of_static_preview() {
    let animated_url = "https://files.slack.com/files-tmb/F1/animated-480.gif";
    let message: SlackMessage = serde_json::from_value(serde_json::json!({
        "ts": "1710000001.000200",
        "thread_ts": "1710000000.000100",
        "text": "shared a GIF",
        "files": [{
            "id": "F1",
            "mimetype": "image/gif",
            "url_private": "https://files.slack.com/files-pri/F1/original.gif",
            "thumb_480": "https://files.slack.com/files-tmb/F1/static-480.png",
            "thumb_480_gif": animated_url
        }]
    }))
    .unwrap();

    assert_eq!(
        message_image_asset_requests(&[message], &HashMap::new(), &HashMap::new()),
        vec![(animated_url.to_string(), animated_url.to_string())]
    );
}

#[test]
fn custom_emoji_reactions_are_requested() {
    let emoji_url = "https://emoji.example.com/pepe-robot.png".to_string();
    let custom_emojis = HashMap::from([("pepe-robot".to_string(), emoji_url.clone())]);
    let message = SlackMessage {
        ts: "1710000002.000100".to_string(),
        reactions: Some(vec![crate::models::SlackReaction {
            name: Some(":pepe-robot:".to_string()),
            count: Some(1),
            ..Default::default()
        }]),
        ..Default::default()
    };

    assert_eq!(
        message_image_asset_requests(&[message], &HashMap::new(), &custom_emojis),
        vec![(emoji_url.clone(), emoji_url)]
    );
}

#[test]
fn cached_canonical_image_remains_eligible_for_dom_asset_patch() {
    let image_url = "https://files.slack.com/files-pri/F1/animated.gif";
    let message = crate::slack_message_wire::normalize_cached_message(
        crate::slack_message_wire::SlackMessageWire::from_value(serde_json::json!({
            "ts": "1710000001.000200",
            "blocks": [{
                "type": "image",
                "slack_file": {"url": image_url},
                "alt_text": "shared a GIF"
            }]
        }))
        .into_message()
        .expect("GIF block should normalize"),
    );

    assert!(messages_use_image_asset(
        &[message],
        &HashMap::new(),
        image_url
    ));
}

#[test]
fn message_user_ids_cover_author_mentions_and_reactions() {
    let message = SlackMessage {
        ts: "1710000000.000100".to_string(),
        user: Some("U1".to_string()),
        text: Some("hi <@U2|bob>".to_string()),
        reactions: Some(vec![crate::models::SlackReaction {
            name: Some("eyes".to_string()),
            users: Some(vec!["U3".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut ids = HashSet::new();
    collect_message_user_ids(&[message], &mut ids);
    let expected = ["U1", "U2", "U3"].map(String::from).into_iter().collect();
    assert_eq!(ids, expected);
}

#[test]
fn thread_root_user_ids_include_reply_users() {
    let root = SlackMessage {
        ts: "1710000000.000100".to_string(),
        user: Some("U1".to_string()),
        reply_users: Some(vec!["U4".to_string()]),
        ..Default::default()
    };
    let mut ids = HashSet::new();
    collect_thread_root_user_ids(&root, &mut ids);
    assert!(ids.contains("U1") && ids.contains("U4"));
}

#[test]
fn search_result_user_ids_include_author_and_mentions() {
    let result = SearchMatch {
        user: Some("U1".to_string()),
        text: Some("ping <@U9> and <@U8|al>".to_string()),
        ..Default::default()
    };
    let mut ids = HashSet::new();
    collect_search_result_user_ids(&result, &mut ids);
    let expected = ["U1", "U8", "U9"].map(String::from).into_iter().collect();
    assert_eq!(ids, expected);
}

#[test]
fn messages_showing_image_asset_lists_only_rows_using_the_key() {
    let emoji_url = "https://emoji.slack-edge.com/T1/party/abc.png".to_string();
    let custom_emojis = HashMap::from([("party".to_string(), emoji_url.clone())]);
    let message = |ts: &str, user: &str, text: &str| SlackMessage {
        ts: ts.to_string(),
        user: Some(user.to_string()),
        text: Some(text.to_string()),
        ..Default::default()
    };
    let messages = [
        message("1.0", "U1", "plain"),
        message("2.0", "U2", "party :party: time"),
        message("3.0", "U3", "plain"),
    ];
    let showing = |key: &str, users: &[&str]| {
        let users = users.iter().map(ToString::to_string).collect::<Vec<_>>();
        messages_showing_image_asset(&messages, &custom_emojis, key, &users)
    };

    assert_eq!(showing(&emoji_url, &[]), HashSet::from(["2.0".to_string()]));
    assert_eq!(
        showing("https://avatars.example/u3.png", &["U3"]),
        HashSet::from(["3.0".to_string()])
    );
    assert!(showing("https://example.com/unused.png", &[]).is_empty());
}

#[test]
fn forward_message_payload_uses_permalink_when_visible() {
    let public_channel = SlackConversation {
        id: "C123".to_string(),
        name: Some("general".to_string()),
        is_channel: Some(true),
        is_private: Some(false),
        ..Default::default()
    };
    let msg = SlackMessage {
        ts: "1710000000.000100".to_string(),
        text: Some("Hello everyone".to_string()),
        ..Default::default()
    };

    // Forward from public channel to another conversation
    let (text, attachments) = build_forward_message_payload(ForwardMessageContext {
        source_channel_id: "C123",
        target_channel_id: "D456",
        message_ts: "1710000000.000100",
        source_message: Some(&msg),
        source_conversation: Some(&public_channel),
        workspace_url: Some("https://workspace.slack.com"),
        user_display_name: Some("Scott"),
        user_avatar_url: None,
    });

    assert_eq!(
        text,
        "https://workspace.slack.com/archives/C123/p1710000000000100"
    );
    assert!(attachments.is_none());

    // Forward to same channel
    let (same_text, same_attachments) = build_forward_message_payload(ForwardMessageContext {
        source_channel_id: "D789",
        target_channel_id: "D789",
        message_ts: "1710000000.000100",
        source_message: Some(&msg),
        source_conversation: None,
        workspace_url: Some("https://workspace.slack.com"),
        user_display_name: Some("Scott"),
        user_avatar_url: None,
    });
    assert_eq!(
        same_text,
        "https://workspace.slack.com/archives/D789/p1710000000000100"
    );
    assert!(same_attachments.is_none());
}

#[test]
fn forward_message_payload_formats_quote_attachment_when_not_visible() {
    let private_dm = SlackConversation {
        id: "D111".to_string(),
        name: Some("secret-chat".to_string()),
        is_im: Some(true),
        is_private: Some(true),
        ..Default::default()
    };
    let msg = SlackMessage {
        ts: "1710000000.000100".to_string(),
        text: Some("Private proposal details".to_string()),
        ..Default::default()
    };

    let (text, attachments) = build_forward_message_payload(ForwardMessageContext {
        source_channel_id: "D111",
        target_channel_id: "D222",
        message_ts: "1710000000.000100",
        source_message: Some(&msg),
        source_conversation: Some(&private_dm),
        workspace_url: Some("https://workspace.slack.com"),
        user_display_name: Some("Scott"),
        user_avatar_url: Some("https://avatar.url/pic.png"),
    });

    assert_eq!(
        text,
        "[Forwarded message from Scott]: Private proposal details"
    );
    assert!(attachments.is_some());
    let attachments_json: serde_json::Value =
        serde_json::from_str(&attachments.unwrap()).expect("valid JSON attachments");
    assert_eq!(
        attachments_json[0]["author_name"],
        serde_json::json!("Scott")
    );
    assert_eq!(
        attachments_json[0]["author_icon"],
        serde_json::json!("https://avatar.url/pic.png")
    );
    assert_eq!(
        attachments_json[0]["text"],
        serde_json::json!("Private proposal details")
    );
    assert_eq!(
        attachments_json[0]["footer"],
        serde_json::json!("Slack conversation in #secret-chat")
    );
    assert_eq!(
        attachments_json[0]["is_msg_unfurl"],
        serde_json::json!(true)
    );
}

#[test]
fn test_hex_to_rgba_conversion() {
    assert_eq!(
        hex_to_rgba("#1264A3", 0.15),
        Some("rgba(18, 100, 163, 0.15)".to_string())
    );
    assert_eq!(
        hex_to_rgba("#ffffff", 0.5),
        Some("rgba(255, 255, 255, 0.50)".to_string())
    );
    assert_eq!(hex_to_rgba("invalid", 0.15), None);
    assert_eq!(hex_to_rgba("#123", 0.15), None);
}

#[test]
fn test_cached_theme_json_roundtrip() {
    let theme = crate::slack::SidebarTheme {
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
    let serialized = serde_json::to_string(&theme).expect("serialize theme");
    let deserialized: crate::slack::SidebarTheme =
        serde_json::from_str(&serialized).expect("deserialize theme");
    assert_eq!(theme, deserialized);
}
