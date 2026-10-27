use super::*;

#[test]
fn quick_reactions_rank_frequency_within_history_limit() {
    let mut history = [
        "heart", "eyes", "thumbsup", "fire", "heart", "eyes", "thumbsup", "fire", "heart", "eyes",
        "thumbsup", "fire", "heart", "eyes", "thumbsup", "heart", "eyes", "heart", "heart",
        "rocket",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    // Unresolvable once-used names fill the window without changing the top
    // four; the rockets after it fall outside the history limit.
    let filler = config::RECENT_REACTION_HISTORY_LIMIT - history.len();
    history.extend((0..filler).map(|index| format!("unknown_{index}")));
    history.extend((0..config::RECENT_REACTION_HISTORY_LIMIT).map(|_| "rocket".to_string()));
    let context = MessageRenderContext {
        recent_reactions: history,
        ..Default::default()
    };

    let names = recent_reactions(&context)
        .into_iter()
        .map(|emoji| emoji.name)
        .collect::<Vec<_>>();

    assert_eq!(names, ["heart", "eyes", "+1", "fire"]);

    let tie_context = MessageRenderContext {
        recent_reactions: [
            "eyes", "heart", "thumbsup", "fire", "fire", "thumbsup", "heart", "eyes",
        ]
        .into_iter()
        .map(str::to_string)
        .collect(),
        ..Default::default()
    };
    let tied_names = recent_reactions(&tie_context)
        .into_iter()
        .map(|emoji| emoji.name)
        .collect::<Vec<_>>();

    assert_eq!(tied_names, ["eyes", "heart", "+1", "fire"]);
}

#[test]
fn quick_reactions_canonicalize_and_deduplicate_aliases() {
    let custom_emojis = Arc::new(HashMap::from([
        ("ohyou".to_string(), "alias:awesome".to_string()),
        (
            "awesome".to_string(),
            "https://emoji.example/awesome.png".to_string(),
        ),
    ]));
    let context = MessageRenderContext {
        custom_emojis,
        recent_reactions: vec![
            "thumbsup".to_string(),
            "+1".to_string(),
            "ohyou".to_string(),
            "awesome".to_string(),
        ],
        ..Default::default()
    };

    let names = recent_reactions(&context)
        .into_iter()
        .map(|emoji| emoji.name)
        .collect::<Vec<_>>();

    assert_eq!(names, ["+1", "awesome", "smile", "white_check_mark"]);
}

#[test]
fn cloned_render_context_shares_workspace_catalogs() {
    let context = MessageRenderContext {
        user_names: Arc::new(HashMap::from([("U123".into(), "Ada".into())])),
        user_full_names: Arc::new(HashMap::from([("U123".into(), "Ada Lovelace".into())])),
        user_avatar_urls: Arc::new(HashMap::from([(
            "U123".into(),
            "https://avatars.example/ada.png".into(),
        )])),
        user_statuses: Arc::new(HashMap::from([("U123".into(), SlackUserStatus::default())])),
        user_group_names: Arc::new(HashMap::from([("S123".into(), "platform".into())])),
        user_group_members: Arc::new(HashMap::from([("S123".into(), vec!["Ada".into()])])),
        custom_emojis: Arc::new(HashMap::from([(
            "party_parrot".into(),
            "https://emoji.example/party.gif".into(),
        )])),
        ..Default::default()
    };

    let cloned = context.clone();

    assert!(Arc::ptr_eq(&context.user_names, &cloned.user_names));
    assert!(Arc::ptr_eq(
        &context.user_full_names,
        &cloned.user_full_names
    ));
    assert!(Arc::ptr_eq(
        &context.user_avatar_urls,
        &cloned.user_avatar_urls
    ));
    assert!(Arc::ptr_eq(&context.user_statuses, &cloned.user_statuses));
    assert!(Arc::ptr_eq(
        &context.user_group_names,
        &cloned.user_group_names
    ));
    assert!(Arc::ptr_eq(
        &context.user_group_members,
        &cloned.user_group_members
    ));
    assert!(Arc::ptr_eq(&context.custom_emojis, &cloned.custom_emojis));
}

#[test]
fn test_mrkdwn_to_pango_plain_text_escaping() {
    let context = MessageRenderContext::default();
    let input = "Cats & Dogs <Birds> \"Fish\" 'Frogs'";
    let output = mrkdwn_to_pango(input, &context);
    assert_eq!(
        output,
        "Cats &amp; Dogs &lt;Birds&gt; &quot;Fish&quot; &apos;Frogs&apos;"
    );
}

#[test]
fn test_mrkdwn_to_pango_basic_formatting() {
    let context = MessageRenderContext::default();
    let input = "*bold* _italic_ ~strike~ `inline code`";
    let output = mrkdwn_to_pango(input, &context);
    assert_eq!(
        output,
        "<b>bold</b> <i>italic</i> <s>strike</s> <tt>inline code</tt>"
    );
}

#[test]
fn test_mrkdwn_to_pango_links() {
    let context = MessageRenderContext::default();
    let input = "<https://example.com|Example> and <https://conduit.app>";
    let output = mrkdwn_to_pango(input, &context);
    assert_eq!(
        output,
        "<a href=\"https://example.com\">Example</a> and <a href=\"https://conduit.app\">https://conduit.app</a>"
    );
}

#[test]
fn test_mrkdwn_to_pango_mentions() {
    let mut context = MessageRenderContext::default();
    let mut user_names = HashMap::new();
    user_names.insert("U123".to_string(), "Alice".to_string());
    context.user_names = Arc::new(user_names);

    Arc::make_mut(&mut context.conversation_titles)
        .insert("C123".to_string(), "general".to_string());

    let input = "<@U123> in <#C123|general> and <#C999>";
    let output = mrkdwn_to_pango(input, &context);
    assert_eq!(
        output,
        "<a href=\"conduit-user://U123\"><span background=\"#D6ECFF\" foreground=\"#1264A3\"> @Alice </span></a> in <span background=\"#D6ECFF\" foreground=\"#1264A3\"> #general </span> and <span background=\"#D6ECFF\" foreground=\"#1264A3\"> #C999 </span>"
    );
}

#[test]
fn test_mrkdwn_to_pango_private_channel_mention() {
    let mut context = MessageRenderContext::default();
    Arc::make_mut(&mut context.conversation_titles)
        .insert("C123".to_string(), "#secret-room".to_string());
    Arc::make_mut(&mut context.private_conversation_ids).insert("C123".to_string());

    let output = mrkdwn_to_pango("<#C123|secret-room>", &context);
    assert_eq!(
        output,
        "<span background=\"#D6ECFF\" foreground=\"#1264A3\"> 🔒 secret-room </span>"
    );
}

#[test]
fn test_mrkdwn_to_pango_emoji() {
    let context = MessageRenderContext::default();
    let input = ":smile: and :unknown_emoji:";
    let output = mrkdwn_to_pango(input, &context);
    assert_eq!(
        output,
        "<span font_desc=\"20px\">😄</span> and :unknown_emoji:"
    );

    let mut emojis = HashMap::new();
    emojis.insert(
        "heart-sparkle".to_string(),
        "https://emoji.slack-edge.com/heart-sparkle.gif".to_string(),
    );
    let custom_context = MessageRenderContext {
        custom_emojis: std::sync::Arc::new(emojis),
        ..MessageRenderContext::default()
    };
    let custom_input = "Hello :heart-sparkle: world";
    let custom_output = mrkdwn_to_pango(custom_input, &custom_context);
    assert_eq!(
        custom_output,
        "Hello <conduit-custom-emoji name=\"heart-sparkle\" url=\"https://emoji.slack-edge.com/heart-sparkle.gif\"/> world"
    );
}

#[test]
fn test_mrkdwn_to_pango_nested_formatting() {
    let context = MessageRenderContext::default();
    let input = "*bold with `code` and _italic_*";
    let output = mrkdwn_to_pango(input, &context);
    assert_eq!(output, "<b>bold with <tt>code</tt> and <i>italic</i></b>");
}

#[test]
fn render_context_equality_tracks_shared_directories_and_per_render_fields() {
    let mut titles = HashMap::new();
    titles.insert("C1".to_string(), "#general".to_string());
    let base = MessageRenderContext {
        conversation_titles: Arc::new(titles),
        ..MessageRenderContext::default()
    };

    // A re-render reusing the same snapshots is unchanged.
    let rerender = base.clone();
    assert!(Arc::ptr_eq(
        &base.conversation_titles,
        &rerender.conversation_titles
    ));
    assert_eq!(base, rerender);

    // A rebuilt snapshot with equal contents still compares equal.
    let rebuilt = MessageRenderContext {
        conversation_titles: Arc::new(base.conversation_titles.as_ref().clone()),
        ..base.clone()
    };
    assert_eq!(base, rebuilt);

    // A rebuilt snapshot with different contents is a change.
    let mut renamed = base.clone();
    Arc::make_mut(&mut renamed.conversation_titles)
        .insert("C1".to_string(), "#announcements".to_string());
    assert!(!Arc::ptr_eq(
        &base.conversation_titles,
        &renamed.conversation_titles
    ));
    assert_ne!(base, renamed);

    // Per-render fields are a change even with shared snapshots.
    let unread = MessageRenderContext {
        unread_separator_ts: Some("1.0".to_string()),
        ..base.clone()
    };
    assert_ne!(base, unread);
    let thread = MessageRenderContext {
        thread_ts: Some("1.0".to_string()),
        ..base.clone()
    };
    assert_ne!(base, thread);
}

#[test]
fn context_delta_reaches_only_rows_showing_the_change() {
    let previous = MessageRenderContext {
        user_names: Arc::new(HashMap::from([
            ("U1".to_string(), "Ada".to_string()),
            ("U2".to_string(), "Bob".to_string()),
        ])),
        user_avatar_urls: Arc::new(HashMap::from([(
            "U2".to_string(),
            "https://avatars.test/bob.png".to_string(),
        )])),
        last_read: Some("1700000000.000100".to_string()),
        ..Default::default()
    };
    assert!(previous.delta_from(&previous.clone()).is_empty());

    let read_on = MessageRenderContext {
        last_read: Some("1700000000.000200".to_string()),
        timeline_scroll: TimelineScrollBehavior::Bottom,
        ..previous.clone()
    };
    assert!(read_on.delta_from(&previous).is_empty(), "view state only");

    let reacted = MessageRenderContext {
        recent_reactions: vec!["tada".to_string()],
        ..previous.clone()
    };
    assert!(
        reacted.delta_from(&previous).is_empty(),
        "quick bar reads recent reactions on hover"
    );

    let mut renamed = previous.clone();
    Arc::make_mut(&mut renamed.user_names).insert("U1".to_string(), "Ada L.".to_string());
    renamed.image_assets.insert(
        "https://avatars.test/bob.png".to_string(),
        PathBuf::from("/tmp/bob.png"),
    );
    renamed.unread_separator_ts = Some("1700000000.000300".to_string());
    let delta = renamed.delta_from(&previous);
    assert!(!delta.all);
    assert_eq!(
        delta.user_ids,
        HashSet::from(["U1".to_string(), "U2".to_string()])
    );
    assert_eq!(
        delta.message_ts,
        HashSet::from(["1700000000.000300".to_string()])
    );

    let by = |user: &str, ts: &str| SlackMessage {
        ts: ts.to_string(),
        user: Some(user.to_string()),
        ..Default::default()
    };
    let custom_emojis = HashMap::new();
    assert!(delta.affects(&by("U1", "1700000000.000100"), &custom_emojis));
    assert!(delta.affects(&by("U3", "1700000000.000300"), &custom_emojis));
    assert!(!delta.affects(&by("U3", "1700000000.000100"), &custom_emojis));
}

#[test]
fn context_delta_rebuilds_everything_for_shared_catalogs() {
    let previous = MessageRenderContext::default();
    let current = MessageRenderContext {
        custom_emojis: Arc::new(HashMap::from([(
            "parrot".to_string(),
            "https://emoji.test/parrot.gif".to_string(),
        )])),
        ..Default::default()
    };
    assert!(current.delta_from(&previous).all);
}
