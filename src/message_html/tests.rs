use super::*;

#[test]
fn normalizes_posix_locales_to_escaped_bcp_47_language_tags() {
    assert_eq!(normalize_language_tag("nl_NL.UTF-8"), Some("nl-NL".into()));
    assert_eq!(
        normalize_language_tag("sr_RS@latin"),
        Some("sr-Latn-RS".into())
    );
    assert_eq!(
        normalize_language_tag("sr_RS@cyrillic"),
        Some("sr-Cyrl-RS".into())
    );
    assert_eq!(
        normalize_language_tag("ks_IN@devanagari"),
        Some("ks-Deva-IN".into())
    );
    assert_eq!(
        normalize_language_tag("tt_RU@iqtelif"),
        Some("tt-Latn-RU".into())
    );
    assert_eq!(
        normalize_language_tag("ca_ES@valencia"),
        Some("ca-ES-valencia".into())
    );
    assert_eq!(
        normalize_language_tag("sr_RS@ijekavianlatin"),
        Some("sr-Latn-RS-ijekavsk".into())
    );
    assert_eq!(
        normalize_language_tag("sr_Latn_RS@latin"),
        Some("sr-Latn-RS".into())
    );
    assert_eq!(
        normalize_language_tag("sr_Latn_RS@cyrillic"),
        Some("sr-Latn-RS".into())
    );
    assert_eq!(normalize_language_tag("C"), None);
    assert_eq!(normalize_language_tag("c.UTF-8"), None);
    assert_eq!(normalize_language_tag("POSIX"), None);
    assert_eq!(normalize_language_tag("posix.utf8"), None);
    assert_eq!(normalize_language_tag("en\"><script>"), None);
    assert_eq!(
        normalize_time_format_locale(Some(b"nl_NL.UTF-8")),
        Some("nl-NL".into())
    );
    assert_eq!(normalize_time_format_locale(Some(&[0xff, 0xfe])), None);
    assert_eq!(
        preferred_time_locale([None, Some("nl_NL.UTF-8"), Some("en_US.UTF-8")]),
        Some("nl-NL".into())
    );
    assert_eq!(
        preferred_time_locale([Some(""), Some("nl_NL.UTF-8"), Some("en_US.UTF-8")]),
        Some("nl-NL".into())
    );
    assert_eq!(
        preferred_time_locale([Some("C.UTF-8"), Some("nl_NL.UTF-8")]),
        None
    );
}

#[test]
fn timestamp_tooltip_includes_the_timezone_once() {
    assert_eq!(
        full_timestamp_with_timezone("do 02 jul 2026 10:42:16 CEST", "CEST"),
        "do 02 jul 2026 10:42:16 CEST"
    );
    assert_eq!(
        full_timestamp_with_timezone("do 02 jul 2026 10:42:16", "CEST"),
        "do 02 jul 2026 10:42:16 CEST"
    );
    assert_eq!(
        full_timestamp_with_timezone("do 02 jul 2026 10:42:16", ""),
        "do 02 jul 2026 10:42:16"
    );
}

#[test]
fn formats_timestamp_text_for_the_active_locale_and_keeps_iso_machine_time() {
    let (machine, full, short) = localized_timestamp_parts("1710000000.000100").unwrap();

    assert!(machine.contains('T'));
    assert!(!full.trim().is_empty());
    assert!(!short.trim().is_empty());
    assert!(localized_timestamp_parts("invalid").is_none());
}

#[test]
fn compact_timestamp_uses_relative_days_then_dates_and_years() {
    let timezone = gtk::glib::TimeZone::local();
    let now = gtk::glib::DateTime::new(&timezone, 2026, 7, 15, 12, 0, 0.0).unwrap();
    let at = |year, month, day| {
        gtk::glib::DateTime::new(&timezone, year, month, day, 9, 30, 0.0).unwrap()
    };

    let today = at(2026, 7, 15);
    assert_eq!(
        compact_timestamp_text(&today, &now).as_deref(),
        today.format(&gettext("%H:%M")).ok().as_deref()
    );

    let yesterday = at(2026, 7, 14);
    assert!(compact_timestamp_text(&yesterday, &now)
        .unwrap()
        .starts_with("Yesterday, "));

    let five_days_ago = at(2026, 7, 10);
    let weekday = capitalize_first_letter(&five_days_ago.format("%A").unwrap());
    assert!(compact_timestamp_text(&five_days_ago, &now)
        .unwrap()
        .contains(&weekday));

    let six_days_ago = at(2026, 7, 9);
    let weekday_six = capitalize_first_letter(&six_days_ago.format("%A").unwrap());
    assert!(compact_timestamp_text(&six_days_ago, &now)
        .unwrap()
        .contains(&weekday_six));

    let seven_days_ago = at(2026, 7, 8);
    let seven_days_label = compact_timestamp_text(&seven_days_ago, &now).unwrap();
    assert!(seven_days_label.contains("8 Jul"));
    assert!(!seven_days_label.contains("2026"));

    let recent_previous_year_now = at(2026, 1, 10);
    let recent_previous_year = at(2025, 12, 31);
    assert!(
        compact_timestamp_text(&recent_previous_year, &recent_previous_year_now)
            .unwrap()
            .contains("31 Dec 2025")
    );

    let old_previous_year = at(2025, 12, 31);
    assert!(compact_timestamp_text(&old_previous_year, &now)
        .unwrap()
        .contains("2025"));
}

#[test]
fn quick_reactions_rank_frequency_within_latest_twenty_uses() {
    let mut history = [
        "heart", "eyes", "thumbsup", "fire", "heart", "eyes", "thumbsup", "fire", "heart", "eyes",
        "thumbsup", "fire", "heart", "eyes", "thumbsup", "heart", "eyes", "heart", "heart",
        "rocket",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    history.extend((0..20).map(|_| "rocket".to_string()));
    let context = MessageHtmlContext {
        recent_reactions: history,
        ..Default::default()
    };

    let names = recent_reactions(&context)
        .into_iter()
        .map(|emoji| emoji.name)
        .collect::<Vec<_>>();

    assert_eq!(names, ["heart", "eyes", "+1", "fire"]);

    let tie_context = MessageHtmlContext {
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
    let context = MessageHtmlContext {
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
    let context = MessageHtmlContext {
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
    let context = MessageHtmlContext::default();
    let input = "Cats & Dogs <Birds> \"Fish\" 'Frogs'";
    let output = mrkdwn_to_pango(input, &context);
    assert_eq!(
        output,
        "Cats &amp; Dogs &lt;Birds&gt; &quot;Fish&quot; &apos;Frogs&apos;"
    );
}

#[test]
fn test_mrkdwn_to_pango_basic_formatting() {
    let context = MessageHtmlContext::default();
    let input = "*bold* _italic_ ~strike~ `inline code`";
    let output = mrkdwn_to_pango(input, &context);
    assert_eq!(
        output,
        "<b>bold</b> <i>italic</i> <s>strike</s> <tt>inline code</tt>"
    );
}

#[test]
fn test_mrkdwn_to_pango_links() {
    let context = MessageHtmlContext::default();
    let input = "<https://example.com|Example> and <https://conduit.app>";
    let output = mrkdwn_to_pango(input, &context);
    assert_eq!(
        output,
        "<a href=\"https://example.com\">Example</a> and <a href=\"https://conduit.app\">https://conduit.app</a>"
    );
}

#[test]
fn test_mrkdwn_to_pango_mentions() {
    let mut context = MessageHtmlContext::default();
    let mut user_names = HashMap::new();
    user_names.insert("U123".to_string(), "Alice".to_string());
    context.user_names = Arc::new(user_names);

    context
        .conversation_titles
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
    let mut context = MessageHtmlContext::default();
    context
        .conversation_titles
        .insert("C123".to_string(), "#secret-room".to_string());
    context.private_conversation_ids.insert("C123".to_string());

    let output = mrkdwn_to_pango("<#C123|secret-room>", &context);
    assert_eq!(
        output,
        "<span background=\"#D6ECFF\" foreground=\"#1264A3\"> 🔒 secret-room </span>"
    );
}

#[test]
fn test_mrkdwn_to_pango_emoji() {
    let context = MessageHtmlContext::default();
    let input = ":smile: and :unknown_emoji:";
    let output = mrkdwn_to_pango(input, &context);
    assert_eq!(output, "😄 and :unknown_emoji:");

    let mut emojis = HashMap::new();
    emojis.insert(
        "heart-sparkle".to_string(),
        "https://emoji.slack-edge.com/heart-sparkle.gif".to_string(),
    );
    let custom_context = MessageHtmlContext {
        custom_emojis: std::sync::Arc::new(emojis),
        ..MessageHtmlContext::default()
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
    let context = MessageHtmlContext::default();
    let input = "*bold with `code` and _italic_*";
    let output = mrkdwn_to_pango(input, &context);
    assert_eq!(output, "<b>bold with <tt>code</tt> and <i>italic</i></b>");
}
