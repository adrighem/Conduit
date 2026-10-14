use std::collections::HashMap;
use std::thread;

use super::*;
use crate::rich_message::SensitiveValue;
use tiny_http::{Header, Response, Server};

fn iso_base_media_fixture(major_brand: &[u8; 4], compatible_brands: &[&[u8; 4]]) -> Vec<u8> {
    let size = 16 + compatible_brands.len() * 4;
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(&(size as u32).to_be_bytes());
    bytes.extend_from_slice(b"ftyp");
    bytes.extend_from_slice(major_brand);
    bytes.extend_from_slice(&[0; 4]);
    for brand in compatible_brands {
        bytes.extend_from_slice(*brand);
    }
    bytes
}

#[test]
fn ia_theme_uses_custom_hexes_and_light_sidebar_by_default() {
    let raw = r##"{"primary":{"palette":"aubergine","custom":"#2A0062"},"highlight1":{"palette":"aubergine","custom":"#190147"},"highlight2":{"palette":"jade","custom":"#41FFD1"},"important":{"palette":"rose","custom":"#FF8F8F"},"brightness":6,"windowBackgroundGradients":true,"useCustomHex":true}"##;
    let theme = parse_ia_theme(&Value::String(raw.into())).expect("theme parses");
    assert_eq!(theme.top_nav_bg.as_deref(), Some("#2A0062"));
    assert_eq!(theme.top_nav_text.as_deref(), Some("#FFFFFF"));
    assert_eq!(theme.column_bg, "#EAE6EF");
    assert_eq!(theme.text_color, "#150031");
    assert_eq!(theme.active_item, "#190147");
    assert_eq!(theme.active_item_text, "#FFFFFF");
    assert_eq!(theme.active_presence, "#41FFD1");
    assert_eq!(theme.mention_badge.as_deref(), Some("#FF8F8F"));
}

#[test]
fn ia_theme_darker_sidebars_tint_primary_with_white_text() {
    let theme = parse_ia_theme(&serde_json::json!({
        "primary": {"palette": "aubergine", "custom": "#2A0062"},
        "useCustomHex": true,
        "sidebarInverted": true,
    }))
    .expect("theme parses");
    assert_eq!(theme.column_bg, "#3F1A72");
    assert_eq!(theme.text_color, "#FFFFFF");
}

#[test]
fn ia_theme_prefers_palette_unless_custom_hex_is_on() {
    let theme = parse_ia_theme(&serde_json::json!({
        "primary": {"palette": "aubergine", "custom": "#2A0062"},
        "useCustomHex": false,
    }))
    .expect("theme parses");
    assert_eq!(theme.top_nav_bg.as_deref(), Some("#4A154B"));
}

#[test]
fn user_boot_prefs_fall_back_to_legacy_custom_values() {
    let prefs: ClientUserBootPrefs = serde_json::from_value(serde_json::json!({
        "sidebar_theme": "eggplant_theme",
        "sidebar_theme_custom_values": "{\"column_bg\":\"#111111\",\"menu_bg\":\"#222222\",\"active_item\":\"#333333\",\"active_item_text\":\"#FFFFFF\",\"hover_item\":\"#444444\",\"text_color\":\"#FFFFFF\",\"active_presence\":\"#00FF00\"}",
    }))
    .unwrap();
    assert_eq!(prefs.sidebar_theme().unwrap().column_bg, "#111111");
}

#[test]
fn parse_sidebar_theme_csv_accepts_eight_value_theme() {
    let csv = "#350d36,#350d36,#1264a3,#ffffff,#350d36,#ffffff,#2bac76,#e8912d";
    let theme = parse_sidebar_theme_csv(csv).expect("theme should parse");
    assert_eq!(theme.column_bg, "#350d36");
    assert_eq!(theme.menu_bg_hover, "#350d36");
    assert_eq!(theme.active_item, "#1264a3");
    assert_eq!(theme.active_item_text, "#ffffff");
    assert_eq!(theme.hover_item, "#350d36");
    assert_eq!(theme.text_color, "#ffffff");
    assert_eq!(theme.active_presence, "#2bac76");
    assert_eq!(theme.mention_badge, Some("#e8912d".to_string()));
    assert_eq!(theme.top_nav_bg, None);
    assert_eq!(theme.top_nav_text, None);
}

#[test]
fn parse_sidebar_theme_csv_rejects_non_custom_preset_value() {
    assert_eq!(parse_sidebar_theme_csv("default"), None);
}

#[test]
fn parse_sidebar_theme_csv_rejects_too_short_theme() {
    let csv = "#350d36,#350d36,#1264a3,#ffffff,#350d36,#ffffff";
    assert_eq!(parse_sidebar_theme_csv(csv), None);
}

#[test]
fn preset_sidebar_theme_resolves_known_presets() {
    let aubergine = preset_sidebar_theme("aubergine").expect("aubergine preset exists");
    assert_eq!(aubergine.column_bg, "#3F0E40");
    assert_eq!(preset_sidebar_theme("default"), Some(aubergine.clone()));

    let goth = preset_sidebar_theme("goth").expect("goth preset exists");
    assert_eq!(goth.column_bg, "#1A1D21");
    assert_eq!(preset_sidebar_theme("dark"), Some(goth));

    assert!(preset_sidebar_theme("nonexistent").is_none());
}

#[test]
fn parse_sidebar_theme_value_resolves_json_object() {
    let json: serde_json::Value = serde_json::json!({
        "column_bg": "#112233",
        "menu_bg_hover": "#223344",
        "active_item": "#334455",
        "active_item_text": "#445566",
        "hover_item": "#556677",
        "text_color": "#667788",
        "active_presence": "#778899",
        "top_nav_bg": "#8899aa",
        "top_nav_text": "#99aabb"
    });
    let theme = parse_sidebar_theme_value(&json).expect("valid json theme");
    assert_eq!(theme.column_bg, "#112233");
    assert_eq!(theme.top_nav_bg.as_deref(), Some("#8899aa"));
}

#[test]
fn parse_sidebar_theme_str_handles_preset_and_json_string() {
    let theme = parse_sidebar_theme_str("chocolate").expect("chocolate preset");
    assert_eq!(theme.column_bg, "#54453B");

    let json_str = r##"{"column_bg":"#112233","menu_bg":"#223344","active_item":"#334455","active_item_text":"#445566","hover_item":"#556677","text_color":"#667788","presence":"#778899"}"##;
    let theme2 = parse_sidebar_theme_str(json_str).expect("json string theme");
    assert_eq!(theme2.column_bg, "#112233");
}

#[test]
fn default_aubergine_is_valid() {
    let theme = SidebarTheme::default_aubergine();
    assert_eq!(theme.column_bg, "#3F0E40");
}

fn browser_test_token(browser_cookie_d: Option<&str>) -> StoredToken {
    StoredToken {
        access_token: "browser-access-value".to_string(),
        token_type: Some("browser_session".to_string()),
        scope: None,
        refresh_token: None,
        expires_in: None,
        expires_at: None,
        team_id: None,
        team_name: None,
        user_id: None,
        client_id: None,
        browser_cookie_d: browser_cookie_d.map(str::to_string),
        user_agent: Some("Conduit Browser Test Agent".to_string()),
    }
}

fn multipart_field_value<'a>(body: &'a str, field: &str) -> Option<&'a str> {
    let marker = format!("name=\"{field}\"");
    let field_start = body.find(&marker)? + marker.len();
    let value_start = body[field_start..].find("\r\n\r\n")? + field_start + 4;
    let value_end = body[value_start..].find("\r\n")? + value_start;
    Some(&body[value_start..value_end])
}

fn user_test_token() -> StoredToken {
    StoredToken {
        access_token: "xoxp-test-token".to_string(),
        token_type: None,
        scope: Some("stars:read,stars:write".to_string()),
        refresh_token: None,
        expires_in: None,
        expires_at: None,
        team_id: None,
        team_name: None,
        user_id: None,
        client_id: None,
        browser_cookie_d: None,
        user_agent: None,
    }
}

#[test]
fn generated_client_message_ids_are_unique_uuid_values() {
    let first = next_client_message_id();
    let second = next_client_message_id();

    assert_ne!(first, second);
    assert_eq!(first.len(), 36);
    assert_eq!(first.as_bytes()[14], b'4');
    assert!(matches!(first.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    assert_eq!(
        first.chars().filter(|character| *character == '-').count(),
        4
    );
}

/// Dispatches `request` against a mock Slack and returns path and body.
fn dispatch_message_action(request: &SlackMessageActionRequest) -> (String, String) {
    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(r#"{"ok":true}"#).with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        (path, body)
    });
    let mut api = SlackApi::new(browser_test_token(Some("browser-cookie-value")));
    api.api_base_url = format!("http://{address}/api");
    tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.execute_message_action("https://example.slack.com/", "T123", request))
        .expect("message action should dispatch");
    received.join().expect("mock Slack server should finish")
}

fn block_action_request(
    attachment_id: Option<u64>,
    is_ephemeral: bool,
) -> SlackMessageActionRequest {
    SlackMessageActionRequest {
        channel_id: "C123".to_string(),
        message_ts: "1710000000.000100".to_string(),
        thread_ts: None,
        service_id: "B123".to_string(),
        app_id: Some("A123".to_string()),
        bot_user_id: Some("U123".to_string()),
        is_ephemeral,
        action: SlackControlAction::Block {
            action: SensitiveValue::new(
                r#"{"type":"button","block_id":"block","action_id":"approve","value":"opaque","text":{"type":"plain_text","text":"Approve"}}"#,
            ),
            attachment_id,
        },
    }
}

#[test]
fn block_message_action_uses_private_browser_dispatch_shape() {
    let (path, body) = dispatch_message_action(&block_action_request(None, false));

    assert_eq!(path, "/api/blocks.actions");
    assert_eq!(multipart_field_value(&body, "service_id"), Some("B123"));
    assert_eq!(multipart_field_value(&body, "app_id"), Some("A123"));
    assert_eq!(
        multipart_field_value(&body, "service_team_id"),
        Some("T123")
    );
    assert!(
        multipart_field_value(&body, "client_token").is_some_and(|value| value.starts_with("web-"))
    );
    assert_eq!(
        multipart_field_value(&body, "_x_reason"),
        Some("dispatch_action_to_developer")
    );
    assert_eq!(
        multipart_field_value(&body, "state"),
        Some(r#"{"values":{}}"#)
    );
    let actions: Value = serde_json::from_str(
        multipart_field_value(&body, "actions").expect("actions field should exist"),
    )
    .expect("actions should be JSON");
    assert_eq!(actions[0]["action_id"], "approve");
    let container: Value = serde_json::from_str(
        multipart_field_value(&body, "container").expect("container field should exist"),
    )
    .expect("container should be JSON");
    assert_eq!(container["type"], "message");
    assert_eq!(container["channel_id"], "C123");
    assert!(container.get("attachment_id").is_none());
    assert!(multipart_field_value(&body, "action_ts").is_none());
}

/// Matches the Slack web client's /giphy Shuffle request: block buttons
/// inside attachment 1 of an ephemeral message.
#[test]
fn attachment_block_action_targets_the_attachment_container() {
    let (path, body) = dispatch_message_action(&block_action_request(Some(1), true));

    assert_eq!(path, "/api/blocks.actions");
    let container: Value = serde_json::from_str(
        multipart_field_value(&body, "container").expect("container field should exist"),
    )
    .expect("container should be JSON");
    assert_eq!(container["type"], "message_attachment");
    assert_eq!(container["attachment_id"], 1);
    assert_eq!(container["message_ts"], "1710000000.000100");
    assert_eq!(container["channel_id"], "C123");
    assert_eq!(container["is_ephemeral"], true);
}

#[test]
fn legacy_message_action_uses_attachment_dispatch_shape() {
    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(r#"{"ok":true,"replaced":true}"#).with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        (path, body)
    });
    let mut api = SlackApi::new(browser_test_token(Some("browser-cookie-value")));
    api.api_base_url = format!("http://{address}/api");
    let request = SlackMessageActionRequest {
        channel_id: "C123".to_string(),
        message_ts: "1710000000.000100".to_string(),
        thread_ts: Some("1710000000.000000".to_string()),
        service_id: "B123".to_string(),
        app_id: None,
        bot_user_id: Some("U123".to_string()),
        is_ephemeral: false,
        action: SlackControlAction::LegacyAttachment {
            attachment_id: 2,
            callback_id: SensitiveValue::new("callback"),
            action: SensitiveValue::new(
                r#"{"type":"button","name":"response","text":"Yes","value":"yes"}"#,
            ),
        },
    };

    tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.execute_message_action("https://example.slack.com/", "T123", &request))
        .expect("message action should dispatch");
    let (path, body) = received.join().expect("mock Slack server should finish");

    assert_eq!(path, "/api/chat.attachmentAction");
    assert_eq!(multipart_field_value(&body, "service_id"), Some("B123"));
    assert_eq!(multipart_field_value(&body, "bot_user_id"), Some("U123"));
    assert_eq!(
        multipart_field_value(&body, "_x_reason"),
        Some("user_attachment_action_dispatch")
    );
    let payload: Value = serde_json::from_str(
        multipart_field_value(&body, "payload").expect("payload field should exist"),
    )
    .expect("payload should be JSON");
    assert_eq!(payload["attachment_id"], 2);
    assert_eq!(payload["callback_id"], "callback");
    assert_eq!(payload["actions"][0]["value"], "yes");
    assert_eq!(payload["thread_ts"], "1710000000.000000");
    assert_eq!(payload["prompt_app_install"], false);
}

#[test]
fn ephemeral_attachment_action_sets_is_ephemeral_flag() {
    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(r#"{"ok":true,"replaced":true}"#).with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        (path, body)
    });
    let mut api = SlackApi::new(browser_test_token(Some("browser-cookie-value")));
    api.api_base_url = format!("http://{address}/api");
    let request = SlackMessageActionRequest {
        channel_id: "C123".to_string(),
        message_ts: "1710000000.000100".to_string(),
        thread_ts: None,
        service_id: "B123".to_string(),
        app_id: None,
        bot_user_id: Some("U123".to_string()),
        is_ephemeral: true,
        action: SlackControlAction::LegacyAttachment {
            attachment_id: 1,
            callback_id: SensitiveValue::new("giphy_send"),
            action: SensitiveValue::new(
                r#"{"type":"button","name":"send","text":"Send","value":"send"}"#,
            ),
        },
    };

    tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.execute_message_action("https://example.slack.com/", "T123", &request))
        .expect("message action should dispatch");
    let (path, body) = received.join().expect("mock Slack server should finish");

    assert_eq!(path, "/api/chat.attachmentAction");
    let payload: Value = serde_json::from_str(
        multipart_field_value(&body, "payload").expect("payload field should exist"),
    )
    .expect("payload should be JSON");
    assert_eq!(payload["is_ephemeral"], true);
}

#[test]
fn message_permalink_uses_chat_get_permalink_with_exact_message() {
    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(
                    r#"{"ok":true,"permalink":"https://example.slack.com/archives/C123/p1710000000000100"}"#,
                )
                .with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        let form = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        (path, form)
    });

    let mut api = SlackApi::new(browser_test_token(None));
    api.api_base_url = format!("http://{address}/api");
    let permalink = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.message_permalink("C123", "1710000000.000100"))
        .expect("message permalink should resolve");

    assert_eq!(
        permalink,
        "https://example.slack.com/archives/C123/p1710000000000100"
    );
    let (path, form) = received.join().expect("mock Slack server should finish");
    assert_eq!(path, "/api/chat.getPermalink");
    assert_eq!(
        form.keys().cloned().collect::<HashSet<_>>(),
        HashSet::from(["channel".to_string(), "message_ts".to_string()])
    );
    assert_eq!(form.get("channel").map(String::as_str), Some("C123"));
    assert_eq!(
        form.get("message_ts").map(String::as_str),
        Some("1710000000.000100")
    );
}

#[test]
fn current_user_status_uses_users_profile_set_with_exact_profile_fields() {
    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(
                    r#"{"ok":true,"profile":{"display_name":"Vincent","status_text":"Focus time","status_emoji":":headphones:","status_expiration":2000000000}}"#,
                )
                .with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        let form = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        (path, form)
    });

    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");
    let profile = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.set_current_user_status(&SlackUserStatus {
            text: " Focus time ".to_string(),
            emoji: "headphones".to_string(),
            expiration: 2_000_000_000,
        }))
        .expect("current user status should update");

    let (path, form) = received.join().expect("mock Slack server should finish");
    assert_eq!(path, "/api/users.profile.set");
    assert_eq!(
        form.keys().cloned().collect::<HashSet<_>>(),
        HashSet::from(["profile".to_string()])
    );
    let payload: Value = serde_json::from_str(
        form.get("profile")
            .expect("profile JSON should be included in the form"),
    )
    .expect("profile form value should be JSON");
    assert_eq!(
        payload,
        json!({
            "status_text": "Focus time",
            "status_emoji": ":headphones:",
            "status_expiration": 2_000_000_000_i64,
        })
    );
    assert_eq!(profile.display_name.as_deref(), Some("Vincent"));
    assert_eq!(
        profile.status(),
        Some(SlackUserStatus {
            text: "Focus time".to_string(),
            emoji: ":headphones:".to_string(),
            expiration: 2_000_000_000,
        })
    );
}

#[test]
fn clearing_current_user_status_sends_empty_text_emoji_and_zero_expiration() {
    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(
                    r#"{"ok":true,"profile":{"status_text":"","status_emoji":"","status_expiration":0}}"#,
                )
                .with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        let form = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        (path, form)
    });

    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");
    let profile = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.set_current_user_status(&SlackUserStatus::default()))
        .expect("current user status should clear");

    let (path, form) = received.join().expect("mock Slack server should finish");
    assert_eq!(path, "/api/users.profile.set");
    let payload: Value = serde_json::from_str(
        form.get("profile")
            .expect("profile JSON should be included in the form"),
    )
    .expect("profile form value should be JSON");
    assert_eq!(
        payload,
        json!({
            "status_text": "",
            "status_emoji": "",
            "status_expiration": 0,
        })
    );
    assert_eq!(profile.status(), None);
}

#[test]
fn current_user_status_rejects_text_longer_than_slacks_limit() {
    let api = SlackApi::new(user_test_token());
    let error = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.set_current_user_status(&SlackUserStatus {
            text: "a".repeat(101),
            ..Default::default()
        }))
        .expect_err("status text longer than 100 characters should be rejected");

    assert_eq!(error.category(), SlackErrorCategory::Validation);
}

#[test]
fn current_user_status_preserves_slack_errors_that_omit_profile() {
    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let received = thread::spawn(move || {
        let request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        request
            .respond(
                Response::from_string(r#"{"ok":false,"error":"missing_scope"}"#).with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        path
    });

    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");
    let error = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.set_current_user_status(&SlackUserStatus {
            text: "Focus time".to_string(),
            ..Default::default()
        }))
        .expect_err("Slack missing-scope response should remain an API error");

    assert!(matches!(
        error,
        SlackError::Api { ref code, .. } if code == "missing_scope"
    ));
    assert_eq!(
        received.join().expect("mock Slack server should finish"),
        "/api/users.profile.set"
    );
}

#[test]
fn message_permalink_validation_is_exact_and_workspace_scoped() {
    let workspace = "https://example.slack.com/";
    let expected = "https://example.slack.com/archives/C123/p1710000000000100";

    assert_eq!(
        constructed_message_permalink(workspace, "C123", "1710000000.0001").as_deref(),
        Some(expected)
    );
    assert_eq!(
        validated_message_permalink(
            &format!("{expected}?thread_ts=1710000000.000000&cid=C123"),
            workspace,
            "C123",
            "1710000000.000100",
        )
        .as_deref(),
        Some(
            "https://example.slack.com/archives/C123/p1710000000000100?thread_ts=1710000000.000000&cid=C123"
        )
    );

    for invalid in [
        "http://example.slack.com/archives/C123/p1710000000000100",
        "https://other.slack.com/archives/C123/p1710000000000100",
        "https://example.slack.com/archives/C999/p1710000000000100",
        "https://example.slack.com/archives/C123/p1710000000000200",
        "https://example.slack.com/archives/C123/p1710000000000100#fragment",
        "https://example.slack.com/archives/C123/p1710000000000100?token=secret",
        "https://example.slack.com/archives/C123/p1710000000000100?cid=C999",
        "https://example.slack.com/archives/C123/p1710000000000100?thread_ts=bad&cid=C123",
    ] {
        assert!(
            validated_message_permalink(invalid, workspace, "C123", "1710000000.000100").is_none(),
            "{invalid} should be rejected"
        );
    }
    assert!(constructed_message_permalink(workspace, "C123/path", "1710000000.000100").is_none());
    assert!(constructed_message_permalink(workspace, "C123", "1710000000.bad").is_none());
    assert!(
        constructed_message_permalink("https://example.invalid/", "C123", "1710000000.000100")
            .is_none()
    );
}

#[test]
fn slack_errors_classify_api_failures_for_recovery() {
    let auth = SlackError::api("auth.test", "invalid_auth");
    let rate_limited = SlackError::api("conversations.history", "ratelimited");
    let unexpected = SlackError::api("conversations.history", "fatal_error");

    assert_eq!(auth.category(), SlackErrorCategory::Authentication);
    assert_eq!(rate_limited.category(), SlackErrorCategory::RateLimited);
    assert_eq!(unexpected.category(), SlackErrorCategory::Unexpected);
}

#[test]
fn slack_errors_classify_validation_and_wrapped_sources() {
    let validation = SlackError::validation("download URL is not trusted");
    let timeout = SlackError::from(anyhow::Error::new(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "request timed out",
    )));
    let local_io = SlackError::from(anyhow::Error::new(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "cache is not writable",
    )));

    assert_eq!(validation.category(), SlackErrorCategory::Validation);
    assert_eq!(timeout.category(), SlackErrorCategory::Connectivity);
    assert_eq!(local_io.category(), SlackErrorCategory::LocalIo);
    assert!(matches!(
        &timeout,
        SlackError::Other(source)
            if source.downcast_ref::<std::io::Error>().is_some()
    ));
}

#[test]
fn conversation_mutation_params_validate_and_normalize_input() {
    assert_eq!(
        conversation_user_ids_param(&[" U2 ".to_string(), "U1".to_string(), "U2".to_string()], 8,)
            .unwrap(),
        "U1,U2"
    );
    assert!(conversation_user_ids_param(&[], 8).is_err());
    let distinct = (0..9).map(|index| format!("U{index}")).collect::<Vec<_>>();
    assert!(conversation_user_ids_param(&distinct, 8).is_err());
    assert_eq!(
        channel_creation_params("project_alpha-2", true).unwrap(),
        vec![
            ("name", "project_alpha-2".to_string()),
            ("is_private", "true".to_string())
        ]
    );
    assert!(channel_creation_params("Invalid channel", false).is_err());
    assert!(channel_creation_params(&"a".repeat(81), false).is_err());
}

fn message(ts: &str) -> SlackMessage {
    SlackMessage {
        ts: ts.to_string(),
        ..Default::default()
    }
}

#[test]
fn workspace_search_adds_prefix_wildcards_and_preserves_modifiers() {
    assert_eq!(
        workspace_search_api_query(
            "  supp bro ui from:ada -in:random has::eyes: \"exact quoted phrase\"  "
        ),
        "supp* bro* ui from:ada -in:random has::eyes: \"exact quoted phrase\""
    );
}

#[test]
fn workspace_search_ignores_quoted_modifier_values_when_filtering_results() {
    assert_eq!(
        workspace_search_content_query("broker from:\"Ada Lovelace\" support"),
        "broker support"
    );
}

#[test]
fn workspace_search_results_match_all_content_substrings() {
    let matches = vec![
        SearchMatch {
            username: Some("Ada Lovelace".to_string()),
            text: Some("The broker needs online payment support".to_string()),
            ..Default::default()
        },
        SearchMatch {
            username: Some("Ada Lovelace".to_string()),
            text: Some("The broker migration is complete".to_string()),
            ..Default::default()
        },
    ];

    let filtered = filter_workspace_search_matches("SUPP bro from:ada", matches);

    assert_eq!(filtered.len(), 1);
    assert_eq!(
        filtered[0].text.as_deref(),
        Some("The broker needs online payment support")
    );
}

#[test]
fn workspace_search_prioritizes_relevance_bands_over_api_order() {
    let matches = vec![
        SearchMatch {
            text: Some("supportive".to_string()),
            ..Default::default()
        },
        SearchMatch {
            text: Some("support".to_string()),
            ..Default::default()
        },
    ];

    let ranked = filter_workspace_search_matches("support", matches);

    assert_eq!(ranked[0].text.as_deref(), Some("support"));
    assert_eq!(ranked[1].text.as_deref(), Some("supportive"));
}

#[test]
fn workspace_search_preserves_api_order_within_a_relevance_band() {
    let matches = vec![
        SearchMatch {
            text: Some("support".to_string()),
            username: Some("Zed".to_string()),
            ..Default::default()
        },
        SearchMatch {
            text: Some("support".to_string()),
            username: Some("Ada".to_string()),
            ..Default::default()
        },
    ];

    let ranked = filter_workspace_search_matches("support", matches);

    assert_eq!(ranked[0].username.as_deref(), Some("Zed"));
    assert_eq!(ranked[1].username.as_deref(), Some("Ada"));
}

#[test]
fn modifier_only_workspace_search_keeps_api_results() {
    let matches = vec![SearchMatch {
        text: Some("Any message".to_string()),
        ..Default::default()
    }];

    assert_eq!(
        filter_workspace_search_matches("from:ada in:general", matches).len(),
        1
    );
}

#[test]
fn thread_replies_are_normalized_to_history_order() {
    let messages = thread_replies_in_history_order(vec![
        message("1710000000.000100"),
        message("1710000100.000100"),
        message("1710000200.000100"),
    ]);
    let timestamps = messages
        .iter()
        .map(|message| message.ts.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        timestamps,
        vec![
            "1710000200.000100",
            "1710000100.000100",
            "1710000000.000100"
        ]
    );
}

#[test]
fn retry_after_seconds_uses_bounded_positive_integer_values() {
    assert_eq!(retry_after_seconds("4"), 4);
    assert_eq!(retry_after_seconds("0"), DEFAULT_RETRY_AFTER_SECONDS);
    assert_eq!(
        retry_after_seconds("not-a-number"),
        DEFAULT_RETRY_AFTER_SECONDS
    );
    assert_eq!(retry_after_seconds("120"), 120);
    assert_eq!(retry_after_seconds("900"), MAX_RETRY_AFTER_SECONDS);
}

#[test]
fn media_content_types_allow_images_and_common_video_formats() {
    assert_eq!(
        supported_media_mime_type("image/jpeg; charset=binary"),
        Some("image/jpeg")
    );
    assert_eq!(supported_media_mime_type("image/avif"), Some("image/avif"));
    assert_eq!(supported_media_mime_type("video/mp4"), Some("video/mp4"));
    assert_eq!(
        supported_media_mime_type("video/webm; codecs=vp9"),
        Some("video/webm")
    );
    assert_eq!(supported_media_mime_type("audio/mpeg"), None);
    assert_eq!(supported_media_mime_type("text/html"), None);
    assert_eq!(supported_media_mime_type("application/octet-stream"), None);
}

#[test]
fn preview_mime_types_use_an_exact_allowlist() {
    let allowed = [
        ("image/png", PreviewAssetMime::Png),
        ("image/jpeg", PreviewAssetMime::Jpeg),
        ("image/gif", PreviewAssetMime::Gif),
        ("image/webp", PreviewAssetMime::Webp),
        ("image/avif", PreviewAssetMime::Avif),
        ("video/mp4", PreviewAssetMime::Mp4),
        ("video/webm", PreviewAssetMime::Webm),
        ("video/quicktime", PreviewAssetMime::Quicktime),
        ("video/ogg", PreviewAssetMime::Ogg),
    ];

    for (mime_type, expected) in allowed {
        assert_eq!(PreviewAssetMime::parse(mime_type), Some(expected));
        assert_eq!(expected.as_str(), mime_type);
        assert!(expected
            .extension()
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()));
        assert!(supported_preview_mime_type(mime_type));
    }
    for mime_type in [
        "image/svg+xml",
        "text/html",
        "application/octet-stream",
        "image/unknown",
    ] {
        assert_eq!(PreviewAssetMime::parse(mime_type), None);
        assert!(!supported_preview_mime_type(mime_type));
    }

    assert_eq!(
        PreviewAssetMime::parse(" Image/PNG; charset=binary "),
        Some(PreviewAssetMime::Png)
    );
    assert_eq!(
        PreviewAssetMime::parse("video/webm; codecs=vp9"),
        Some(PreviewAssetMime::Webm)
    );
    assert_eq!(PreviewAssetMime::ALL.len(), allowed.len());
}

#[test]
fn preview_payload_signatures_must_match_the_declared_mime_type() {
    let avif = iso_base_media_fixture(b"mif1", &[b"avif"]);
    let mp4 = iso_base_media_fixture(b"isom", &[b"mp42"]);
    let quicktime = iso_base_media_fixture(b"qt  ", &[]);
    let valid = [
        (PreviewAssetMime::Png, b"\x89PNG\r\n\x1a\n".as_slice()),
        (PreviewAssetMime::Jpeg, b"\xff\xd8\xff\xe0".as_slice()),
        (PreviewAssetMime::Gif, b"GIF89a".as_slice()),
        (PreviewAssetMime::Webp, b"RIFF\x04\0\0\0WEBP".as_slice()),
        (PreviewAssetMime::Avif, avif.as_slice()),
        (PreviewAssetMime::Mp4, mp4.as_slice()),
        (PreviewAssetMime::Webm, b"\x1a\x45\xdf\xa3".as_slice()),
        (PreviewAssetMime::Quicktime, quicktime.as_slice()),
        (PreviewAssetMime::Ogg, b"OggS".as_slice()),
    ];

    for (mime_type, bytes) in valid {
        assert!(mime_type.is_valid_payload(bytes), "{}", mime_type.as_str());
    }

    assert!(!PreviewAssetMime::Png.is_valid_payload(b"<html>"));
    assert!(!PreviewAssetMime::Jpeg.is_valid_payload(b"\x89PNG\r\n\x1a\n"));
    assert!(!PreviewAssetMime::Avif.is_valid_payload(&mp4));
    assert!(!PreviewAssetMime::Mp4.is_valid_payload(&avif));
    assert!(!PreviewAssetMime::Mp4.is_valid_payload(&quicktime));
    assert!(!PreviewAssetMime::Quicktime.is_valid_payload(&mp4));
    assert!(!PreviewAssetMime::Png.is_valid_payload(&[]));
    assert!(!PreviewAssetMime::Quicktime.is_valid_payload(b"\0\0\0\x08ftyp"));
    assert!(PreviewAssetMime::Png
        .validate_cached_content(MAX_PREVIEW_IMAGE_BYTES as u64, b"\x89PNG\r\n\x1a\n"));
    assert!(!PreviewAssetMime::Png
        .validate_cached_content(MAX_PREVIEW_IMAGE_BYTES as u64 + 1, b"\x89PNG\r\n\x1a\n"));
}

#[test]
fn preview_payload_validation_enforces_per_kind_size_bounds() {
    assert_eq!(PreviewAssetMime::Png.max_bytes(), MAX_PREVIEW_IMAGE_BYTES);
    assert_eq!(PreviewAssetMime::Mp4.max_bytes(), MAX_PREVIEW_VIDEO_BYTES);
    assert!(PreviewAssetMime::Png.validate_size(MAX_PREVIEW_IMAGE_BYTES));
    assert!(!PreviewAssetMime::Png.validate_size(MAX_PREVIEW_IMAGE_BYTES + 1));
    assert!(!PreviewAssetMime::Gif.validate_size(0));
}

#[test]
fn emoji_list_response_preserves_urls_and_aliases() {
    let response: EmojiListResponse = serde_json::from_value(serde_json::json!({
        "ok": true,
        "emoji": {
            "party_parrot": "https://emoji.example/parrot.gif",
            "ship_it": "alias:rocket"
        }
    }))
    .expect("emoji response should parse");

    assert_eq!(
        response.emoji.get("party_parrot").map(String::as_str),
        Some("https://emoji.example/parrot.gif")
    );
    assert_eq!(
        response.emoji.get("ship_it").map(String::as_str),
        Some("alias:rocket")
    );
}

#[test]
fn media_download_size_is_bounded() {
    assert!(ensure_media_size(None).is_ok());
    assert!(ensure_media_size(Some(MAX_MEDIA_DOWNLOAD_BYTES)).is_ok());
    assert!(ensure_media_size(Some(MAX_MEDIA_DOWNLOAD_BYTES + 1)).is_err());
}

#[test]
fn preview_chunks_are_rejected_before_exceeding_the_memory_limit() {
    let mut bytes = vec![1, 2];
    append_bounded_preview_chunk(&mut bytes, &[3, 4], 4).unwrap();
    assert_eq!(bytes, vec![1, 2, 3, 4]);

    let error = append_bounded_preview_chunk(&mut bytes, &[5], 4).unwrap_err();
    assert!(error.to_string().contains("too large"));
    assert_eq!(bytes, vec![1, 2, 3, 4]);
}

#[test]
fn authenticated_downloads_are_restricted_to_slack_https_hosts() {
    assert!(is_trusted_slack_download_url(
        "https://files.slack.com/files-pri/T1-F1/download/report.pdf"
    ));
    assert!(is_trusted_slack_download_url(
        "https://signicat.slack.com/files/U1/F1/report.pdf"
    ));
    assert!(is_trusted_slack_download_url("https://slack.com/file.pdf"));

    assert!(!is_trusted_slack_download_url(
        "http://files.slack.com/file.pdf"
    ));
    assert!(!is_trusted_slack_download_url(
        "https://slack.com.evil.example/file.pdf"
    ));
    assert!(!is_trusted_slack_download_url(
        "https://token@files.slack.com/file.pdf"
    ));
    assert!(!is_trusted_slack_download_url("not a URL"));
    assert!(ensure_trusted_slack_download_url(
        "https://files.slack.com/files-pri/T1-F1/download/report.pdf"
    )
    .is_ok());
    assert!(ensure_trusted_slack_download_url("https://evil.example/preview.png").is_err());
    assert!(supports_native_preview_asset_url(
        "https://files.slack.com/files-pri/T1-F1/download/report.pdf"
    ));
    assert!(!supports_native_preview_asset_url(
        "https://images.example.test/card.png"
    ));
}

#[test]
fn public_avatar_downloads_are_restricted_to_exact_https_hosts() {
    assert!(is_trusted_avatar_url(
        "https://avatars.slack-edge.com/2026-01-01/avatar_72.png"
    ));
    assert!(is_trusted_avatar_url(
        "https://secure.gravatar.com/avatar/hash.jpg"
    ));
    assert!(is_trusted_avatar_url(
        "https://a.slack-edge.com/80588/img/slackbot_72.png"
    ));
    assert!(is_trusted_avatar_url(
        "https://emoji.slack-edge.com/T1/custom/hash.png"
    ));
    assert!(is_trusted_avatar_url(
        "https://emoji.slack-edge.com/T1/custom/hash.gif"
    ));
    assert!(!is_trusted_avatar_url(
        "http://avatars.slack-edge.com/avatar.png"
    ));
    assert!(!is_trusted_avatar_url(
        "https://avatars.slack-edge.com.evil.example/avatar.png"
    ));
    assert!(!is_trusted_avatar_url(
        "https://token@secure.gravatar.com/avatar/hash.jpg"
    ));
    assert!(supports_native_preview_asset_url(
        "https://avatars.slack-edge.com/2026-01-01/avatar_72.png"
    ));
}

#[test]
fn preview_allowlist_accepts_gif_services_and_rejects_other_hosts() {
    for valid_url in [
        "https://media1.giphy.com/media/abc/200w.gif?cid=1&rid=200w.gif",
        "https://i.giphy.com/abc.gif",
        "https://giphy.com/abc.gif",
        "https://media.tenor.com/abc/tenor.gif",
        "https://a.slack-edge.com/dc483/img/plugins/giphy/service_32.png",
        "https://files.slack.com/files-pri/T1-F1/image.png",
    ] {
        assert!(supports_native_preview_asset_url(valid_url), "{valid_url}");
    }
    for invalid_url in [
        "https://images.example.test/card.gif",
        "https://evilgiphy.com/abc.gif",
        "https://media1.giphy.com.evil.example/abc.gif",
        "http://media1.giphy.com/abc.gif",
        "https://user@media1.giphy.com/abc.gif",
        "not a URL",
    ] {
        assert!(
            !supports_native_preview_asset_url(invalid_url),
            "{invalid_url}"
        );
    }
}

#[test]
fn image_proxy_is_allowlisted_exactly_and_never_authenticated() {
    let proxy = "https://slack-imgs.com/?c=1&o1=ro&url=https%3A%2F%2Fexample.com%2Fa.png";
    assert!(supports_native_preview_asset_url(proxy));
    assert!(!preview_download_needs_auth(proxy));
    assert!(preview_download_needs_auth(
        "https://files.slack.com/files-pri/T1-F1/a.png"
    ));
    for bad in [
        "http://slack-imgs.com/?url=x",
        "https://slack-imgs.com.evil.example/?url=x",
        "https://evil-slack-imgs.com/?url=x",
        "https://token@slack-imgs.com/?url=x",
    ] {
        assert!(!supports_native_preview_asset_url(bad), "{bad}");
    }
}

#[test]
fn attachment_download_size_is_bounded() {
    assert!(ensure_attachment_size(None).is_ok());
    assert!(ensure_attachment_size(Some(MAX_MEDIA_DOWNLOAD_BYTES)).is_ok());
    assert!(ensure_attachment_size(Some(MAX_MEDIA_DOWNLOAD_BYTES + 1)).is_err());
}

#[test]
fn completed_upload_targets_requested_thread() {
    let params = complete_upload_params(
        "files-json".to_string(),
        "C123",
        Some("1710000000.000100"),
        None,
    );

    assert!(params.contains(&("channel_id", "C123".to_string())));
    assert!(params.contains(&("thread_ts", "1710000000.000100".to_string())));
}

#[test]
fn completed_batch_upload_uses_rich_blocks_without_initial_comment() {
    let blocks = r#"[{"type":"rich_text"}]"#;
    let params = complete_upload_params(
        "files-json".to_string(),
        "C123",
        Some("1710000000.000100"),
        Some(blocks),
    );

    assert!(params.contains(&("blocks", blocks.to_string())));
    assert!(!params.iter().any(|(name, _)| *name == "initial_comment"));
}

#[test]
fn batch_upload_completes_ordered_files_once_with_rich_blocks() {
    let directory = std::env::temp_dir().join(format!(
        "conduit-batch-upload-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&directory).expect("upload fixture directory should exist");
    let paths = [directory.join("first.txt"), directory.join("second.txt")];
    std::fs::write(&paths[0], b"first").expect("first upload fixture should be written");
    std::fs::write(&paths[1], b"second").expect("second upload fixture should be written");

    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let server_base = format!("http://{address}");
    let upload_base = server_base.clone();
    let received = thread::spawn(move || {
        let mut observations = Vec::new();
        let mut upload_url_index = 0;
        for _ in 0..5 {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            let response = if path == "/api/files.getUploadURLExternal" {
                upload_url_index += 1;
                Response::from_string(format!(
                    r#"{{"ok":true,"upload_url":"{upload_base}/upload/{upload_url_index}","file_id":"F{upload_url_index}"}}"#
                ))
                .with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                )
            } else if path.starts_with("/upload/") {
                Response::from_string("")
            } else if path == "/api/files.completeUploadExternal" {
                Response::from_string(
                    r#"{"ok":true,"files":[{"id":"F1","name":"first.txt"},{"id":"F2","name":"second.txt"}]}"#,
                )
                .with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                )
            } else {
                panic!("unexpected mock Slack path {path}");
            };
            request
                .respond(response)
                .expect("mock Slack response should be sent");
            observations.push((path, body));
        }
        observations
    });

    let blocks = r#"[{"type":"rich_text","elements":[]}]"#;
    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("{server_base}/api");
    let files = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.upload_files("C123", Some("1.0"), &paths, Some(blocks), |_| {}))
        .expect("batch upload should complete");

    assert_eq!(files.len(), 2);
    let observations = received.join().expect("mock Slack server should finish");
    assert_eq!(
        observations
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        vec![
            "/api/files.getUploadURLExternal",
            "/upload/1",
            "/api/files.getUploadURLExternal",
            "/upload/2",
            "/api/files.completeUploadExternal",
        ]
    );
    let complete_body = observations
        .iter()
        .find_map(|(path, body)| (path == "/api/files.completeUploadExternal").then_some(body))
        .expect("completion request should be present");
    let form = url::form_urlencoded::parse(complete_body.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    assert_eq!(form.get("channel_id").map(String::as_str), Some("C123"));
    assert_eq!(form.get("thread_ts").map(String::as_str), Some("1.0"));
    assert_eq!(form.get("blocks").map(String::as_str), Some(blocks));
    assert!(!form.contains_key("initial_comment"));
    let uploaded: Value = serde_json::from_str(form.get("files").expect("files form value"))
        .expect("files form value should be JSON");
    assert_eq!(uploaded.as_array().map(Vec::len), Some(2));
    assert_eq!(uploaded[0]["id"], "F1");
    assert_eq!(uploaded[1]["id"], "F2");
    std::fs::remove_dir_all(directory).expect("upload fixtures should be removed");
}

#[test]
fn rich_message_post_keeps_accessible_fallback_and_blocks() {
    let params = post_message_params(
        "C123",
        "Hello <@UADA>",
        Some(r#"[{"type":"rich_text"}]"#),
        None,
        Some("1710000000.000100"),
        "client-message-id",
    );

    assert!(params.contains(&("channel", "C123".to_string())));
    assert!(params.contains(&("text", "Hello <@UADA>".to_string())));
    assert!(params.contains(&("blocks", r#"[{"type":"rich_text"}]"#.to_string())));
    assert!(params.contains(&("thread_ts", "1710000000.000100".to_string())));
    assert!(params.contains(&("client_msg_id", "client-message-id".to_string())));
}

#[test]
fn execute_slash_command_params_formats_command_and_options() {
    let params =
        execute_slash_command_params("C123", "giphy", "cat dance", Some("1710000000.000100"));
    assert!(params.contains(&("channel", "C123".to_string())));
    assert!(params.contains(&("command", "/giphy".to_string())));
    assert!(params.contains(&("disp", "/giphy".to_string())));
    assert!(params.contains(&("text", "cat dance".to_string())));
    assert!(params.contains(&("thread_ts", "1710000000.000100".to_string())));

    let params_no_thread = execute_slash_command_params("C456", "/giphy", "banana", None);
    assert!(params_no_thread.contains(&("channel", "C456".to_string())));
    assert!(params_no_thread.contains(&("command", "/giphy".to_string())));
    assert!(params_no_thread.contains(&("text", "banana".to_string())));
    assert!(!params_no_thread.iter().any(|(k, _)| *k == "thread_ts"));
}

#[test]
fn execute_slash_command_posts_to_chat_command_endpoint() {
    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        assert_eq!(request.url(), "/api/chat.command");
        request
            .respond(
                Response::from_string(
                    r#"{"ok":true,"response":"ok","command":"/giphy","channel":"C123"}"#,
                )
                .with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        body
    });
    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");

    let response = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.execute_slash_command(
            "C123",
            "/giphy",
            "cat dance",
            Some("1710000000.000100"),
        ))
        .expect("slash command should execute");

    assert!(response.ok);
    assert_eq!(response.command.as_deref(), Some("/giphy"));
    let body = received.join().expect("mock Slack server should finish");
    let form = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    assert_eq!(form.get("channel").map(String::as_str), Some("C123"));
    assert_eq!(form.get("command").map(String::as_str), Some("/giphy"));
    assert_eq!(form.get("text").map(String::as_str), Some("cat dance"));
    assert_eq!(
        form.get("thread_ts").map(String::as_str),
        Some("1710000000.000100")
    );
}

#[test]
fn rich_message_post_restores_submitted_blocks_when_response_omits_them() {
    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(
                    r#"{"ok":true,"message":{"ts":"1710000000.000100","text":"Hello"}}"#,
                )
                .with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        body
    });
    let blocks = r#"[{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"Hello","style":{"bold":true}}]}]}]"#;
    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");

    let message = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.post_message("C123", "Hello", Some(blocks), None, None))
        .expect("message should post");

    assert!(matches!(
        message.document.nodes(),
        [crate::rich_message::MessageNode::RichText(_)]
    ));
    let body = received.join().expect("mock Slack server should finish");
    let form = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    assert_eq!(form.get("blocks").map(String::as_str), Some(blocks));
}

#[test]
fn rich_message_post_sends_attachments_and_restores_omitted_attachments() {
    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(
                    r#"{"ok":true,"message":{"ts":"1710000000.000100","text":"Forwarded"}}"#,
                )
                .with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        body
    });
    let attachments = r#"[{"author_name":"Scott","text":"Original message","footer":"Slack conversation","ts":"1710000000"}]"#;
    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");

    let message = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.post_message("C123", "Forwarded", None, Some(attachments), None))
        .expect("message should post");

    assert!(message.attachments.is_some());
    let body = received.join().expect("mock Slack server should finish");
    let form = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    assert_eq!(
        form.get("attachments").map(String::as_str),
        Some(attachments)
    );
}

#[test]
fn rich_message_update_targets_existing_message_and_restores_omitted_blocks() {
    let server = Server::http("127.0.0.1:0").expect("mock Slack server should start");
    let address = server.server_addr();
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(
                    r#"{"ok":true,"channel":"C123","ts":"1710000000.000100","text":"Edited","message":{"text":"Edited","user":"U1"}}"#,
                )
                .with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        (path, body)
    });
    let blocks = r#"[{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"Edited","style":{"italic":true}}]}]}]"#;
    let original = SlackMessage {
        user: Some("U1".into()),
        text: Some("Original".into()),
        ts: "1710000000.000100".into(),
        reactions: Some(vec![]),
        ..SlackMessage::default()
    };
    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");

    let message = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.update_message("C123", &original, "Edited", Some(blocks)))
        .expect("message should update");

    assert_eq!(message.ts, original.ts);
    assert_eq!(message.body_text(), "Edited");
    assert_eq!(message.reactions, original.reactions);
    assert!(message.edited.is_some());
    assert!(matches!(
        message.document.nodes(),
        [crate::rich_message::MessageNode::RichText(_)]
    ));
    let (path, body) = received.join().expect("mock Slack server should finish");
    assert_eq!(path, "/api/chat.update");
    let form = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    assert_eq!(form.get("channel").map(String::as_str), Some("C123"));
    assert_eq!(
        form.get("ts").map(String::as_str),
        Some("1710000000.000100")
    );
    assert_eq!(form.get("text").map(String::as_str), Some("Edited"));
    assert_eq!(form.get("blocks").map(String::as_str), Some(blocks));
    assert!(!form.contains_key("client_msg_id"));
    assert!(!form.contains_key("thread_ts"));
}

#[test]
fn plain_message_update_omits_blocks_without_changing_the_target() {
    let form = update_message_params("C123", "1710000000.000100", "Edited", None)
        .into_iter()
        .collect::<HashMap<_, _>>();

    assert_eq!(form.get("channel").map(String::as_str), Some("C123"));
    assert_eq!(
        form.get("ts").map(String::as_str),
        Some("1710000000.000100")
    );
    assert_eq!(form.get("text").map(String::as_str), Some("Edited"));
    assert!(!form.contains_key("blocks"));
}

#[test]
fn starred_conversation_ids_are_paginated_and_ignore_message_stars() {
    let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
    let address = server
        .server_addr()
        .to_ip()
        .expect("mock Slack server should use an IP address");
    let received = thread::spawn(move || {
        let mut observations = Vec::new();
        for response_body in [
            r#"{
                "ok": true,
                "items": [
                    {"type": "channel", "channel": "C1"},
                    {"type": "im", "channel": "D1"},
                    {"type": "group", "group": "G1"},
                    {"type": "message", "channel": "C-message", "message": {"ts": "1.0"}}
                ],
                "response_metadata": {"next_cursor": "next-page"}
            }"#,
            r#"{
                "ok": true,
                "items": [{"type": "channel", "channel": "C2"}],
                "response_metadata": {"next_cursor": ""}
            }"#,
        ] {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            request
                .respond(
                    Response::from_string(response_body).with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
                )
                .expect("mock Slack response should be sent");
            observations.push((path, body));
        }
        observations
    });

    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");
    let ids = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.starred_conversation_ids())
        .expect("starred conversations should load");

    assert_eq!(
        ids,
        HashSet::from([
            "C1".to_string(),
            "C2".to_string(),
            "D1".to_string(),
            "G1".to_string(),
        ])
    );
    let observations = received.join().expect("mock Slack server should finish");
    assert_eq!(observations.len(), 2);
    for (index, (path, body)) in observations.iter().enumerate() {
        assert_eq!(path, "/api/stars.list");
        let form = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(form.get("limit").map(String::as_str), Some("200"));
        assert_eq!(
            form.get("cursor").map(String::as_str),
            (index == 1).then_some("next-page")
        );
    }
}

#[test]
fn conversation_star_toggle_uses_a_bare_channel_target() {
    let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
    let address = server
        .server_addr()
        .to_ip()
        .expect("mock Slack server should use an IP address");
    let received = thread::spawn(move || {
        let mut observations = Vec::new();
        for _ in 0..2 {
            let mut request = server.recv().expect("mock Slack request should arrive");
            let path = request.url().to_string();
            let mut body = String::new();
            request
                .as_reader()
                .read_to_string(&mut body)
                .expect("mock Slack request body should be readable");
            request
                .respond(
                    Response::from_string(r#"{"ok":true}"#).with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
                )
                .expect("mock Slack response should be sent");
            observations.push((path, body));
        }
        observations
    });

    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");
    let runtime = tokio::runtime::Runtime::new().expect("test runtime should start");
    runtime
        .block_on(api.set_conversation_starred("D123", true))
        .expect("conversation should be starred");
    runtime
        .block_on(api.set_conversation_starred("D123", false))
        .expect("conversation should be unstarred");

    let observations = received.join().expect("mock Slack server should finish");
    assert_eq!(observations.len(), 2);
    for ((path, body), expected_method) in observations
        .into_iter()
        .zip(["/api/stars.add", "/api/stars.remove"])
    {
        assert_eq!(path, expected_method);
        let form = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(form.get("channel").map(String::as_str), Some("D123"));
        assert!(!form.contains_key("timestamp"));
    }
}

#[test]
fn partial_media_download_lives_next_to_destination() {
    let destination = Path::new("/tmp/conduit/media/photo.jpg");
    let partial = partial_download_path(destination);

    assert_eq!(partial.parent(), destination.parent());
    assert!(partial
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("photo.jpg.") && name.ends_with(".part")));
}

#[test]
fn discovery_requests_only_non_archived_channel_types() {
    assert_eq!(
        paginated_list_params(Some(" next-page "), true),
        vec![
            ("types", "public_channel,private_channel".to_string()),
            ("exclude_archived", "true".to_string()),
            ("limit", "200".to_string()),
            ("cursor", "next-page".to_string()),
        ]
    );
}

#[test]
fn users_requests_are_paginated_without_channel_parameters() {
    assert_eq!(
        paginated_list_params(Some("users-page"), false),
        vec![
            ("limit", "200".to_string()),
            ("cursor", "users-page".to_string()),
        ]
    );
    assert_eq!(
        paginated_list_params(Some("  "), false),
        vec![("limit", "200".to_string())]
    );
}

#[test]
fn discovery_filter_rejects_archived_channels_and_direct_messages() {
    let public_channel = SlackConversation {
        is_channel: Some(true),
        ..Default::default()
    };
    let private_channel = SlackConversation {
        is_private: Some(true),
        ..Default::default()
    };
    let archived_channel = SlackConversation {
        is_channel: Some(true),
        is_archived: Some(true),
        ..Default::default()
    };
    let direct_message = SlackConversation {
        is_im: Some(true),
        ..Default::default()
    };

    assert!(is_discoverable_conversation(&public_channel));
    assert!(is_discoverable_conversation(&private_channel));
    assert!(!is_discoverable_conversation(&archived_channel));
    assert!(!is_discoverable_conversation(&direct_message));
}

#[test]
fn pagination_cursor_is_trimmed_and_empty_values_end_pagination() {
    assert_eq!(
        next_cursor(Some(ResponseMetadata {
            next_cursor: Some(" next-page ".to_string()),
        }))
        .as_deref(),
        Some("next-page")
    );
    assert_eq!(
        next_cursor(Some(ResponseMetadata {
            next_cursor: Some("  ".to_string()),
        })),
        None
    );
    assert_eq!(next_cursor(None), None);
}

#[test]
fn conversation_property_logging_is_opt_in_and_bounded() {
    assert_eq!(conversation_property_log_limit(None, 100), 0);
    assert_eq!(
        conversation_property_log_limit(Some("true"), 100),
        DEFAULT_DEBUG_CONVERSATION_PROPERTY_LIMIT
    );
    assert_eq!(conversation_property_log_limit(Some("1"), 5), 5);
    assert_eq!(conversation_property_log_limit(Some("7"), 100), 7);
    assert_eq!(conversation_property_log_limit(Some("all"), 100), 100);
    assert_eq!(conversation_property_log_limit(Some("invalid"), 100), 0);
}

#[test]
fn message_page_has_more_uses_response_metadata_cursor() {
    let page = SlackMessagePage::from_response(
        HistoryResponse {
            ok: true,
            error: None,
            messages: vec![message("1710000000.000100")],
            has_more: Some(false),
            response_metadata: Some(ResponseMetadata {
                next_cursor: Some(" next-page ".to_string()),
            }),
        },
        std::convert::identity,
    );

    assert!(page.has_more);
    assert_eq!(page.next_cursor.as_deref(), Some("next-page"));
}

#[test]
fn history_request_params_include_cursor_only_when_present() {
    assert_eq!(
        history_request_params("C123", None, CHANNEL_HISTORY_PAGE_LIMIT)
            .iter()
            .find(|(key, _)| *key == "limit")
            .map(|(_, value)| value.as_str()),
        Some(CHANNEL_HISTORY_PAGE_LIMIT.to_string()).as_deref()
    );
    assert!(
        !history_request_params("C123", None, CHANNEL_HISTORY_PAGE_LIMIT)
            .iter()
            .any(|(key, _)| *key == "cursor")
    );
    assert_eq!(
        history_request_params("C123", Some("next-page"), CHANNEL_HISTORY_PAGE_LIMIT)
            .iter()
            .find(|(key, _)| *key == "cursor")
            .map(|(_, value)| value.as_str()),
        Some("next-page")
    );
}

#[test]
fn message_context_requests_are_bounded_inclusive_and_targeted() {
    let params = message_context_request_params("C123", "1710000000.000100");

    assert_eq!(
        params,
        vec![
            ("channel", "C123".to_string()),
            ("latest", "1710000000.000100".to_string()),
            ("inclusive", "true".to_string()),
            ("limit", "15".to_string()),
        ]
    );
}

#[test]
fn merged_thread_pages_are_newest_first_without_duplicates() {
    let page = |ts: &[&str], cursor: Option<&str>| SlackMessagePage {
        messages: ts
            .iter()
            .map(|ts| SlackMessage {
                ts: ts.to_string(),
                ..Default::default()
            })
            .collect(),
        has_more: cursor.is_some(),
        next_cursor: cursor.map(ToString::to_string),
    };
    // Pages arrive oldest-first; each is already newest-first inside.
    let merged = merge_thread_pages(vec![
        page(&["3.0", "2.0", "1.0"], Some("next")),
        page(&["5.0", "4.0", "1.0"], None),
    ]);
    assert_eq!(
        merged
            .messages
            .iter()
            .map(|message| message.ts.as_str())
            .collect::<Vec<_>>(),
        vec!["5.0", "4.0", "3.0", "2.0", "1.0"]
    );
    assert!(!merged.has_more);
    assert_eq!(merged.next_cursor, None);
}

#[test]
fn browser_workspace_urls_are_restricted_to_slack_origins() {
    assert_eq!(
        validated_workspace_api_base_url("https://example.slack.com/").unwrap(),
        "https://example.slack.com/api"
    );
    assert_eq!(
        validated_workspace_api_base_url("https://grid.example.slack.com:443/").unwrap(),
        "https://grid.example.slack.com/api"
    );

    for workspace_url in [
        "http://example.slack.com/",
        "https://example.slack.com:8443/",
        "https://person@example.slack.com/",
        "https://example.slack.com/path",
        "https://example.slack.com/?mode=online",
        "https://example.slack.com/#fragment",
        "https://slack.com.evil.example/",
        "https://slack.com/",
    ] {
        assert!(validated_workspace_api_base_url(workspace_url).is_err());
    }
}

#[test]
fn browser_session_auth_test_sends_upstream_form_and_cookie_shape() {
    let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
    let address = server
        .server_addr()
        .to_ip()
        .expect("mock Slack server should use an IP address");
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let headers = request
            .headers()
            .iter()
            .map(|header| {
                (
                    header.field.as_str().to_ascii_lowercase().to_string(),
                    header.value.as_str().to_string(),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(
                    r#"{"ok":true,"team":"Example","team_id":"T1","user":"Vincent","user_id":"U1","url":"https://example.slack.com/"}"#,
                )
                .with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        (path, headers, body)
    });

    let token = StoredToken {
        access_token: "xoxc-browser-token".to_string(),
        token_type: Some("browser_session".to_string()),
        scope: None,
        refresh_token: None,
        expires_in: None,
        expires_at: None,
        team_id: None,
        team_name: None,
        user_id: None,
        client_id: None,
        browser_cookie_d: Some("xoxd-cookie-value".to_string()),
        user_agent: Some("Exact Browser User Agent".to_string()),
    };
    let mut api = SlackApi::new(token);
    api.api_base_url = format!("http://{address}/api");
    let auth = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.auth_test())
        .expect("browser-session auth.test should succeed");
    assert_eq!(auth.team_id.as_deref(), Some("T1"));

    let (path, headers, body) = received.join().expect("mock Slack server should finish");
    assert_eq!(path, "/api/auth.test");
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer xoxc-browser-token")
    );
    assert_eq!(
        headers.get("user-agent").map(String::as_str),
        Some("Exact Browser User Agent")
    );
    let cookie = headers
        .get("cookie")
        .expect("browser-session cookie should be sent");
    assert!(cookie.starts_with("d=xoxd-cookie-value; d-s="));
    let form = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    assert_eq!(
        form.get("token").map(String::as_str),
        Some("xoxc-browser-token")
    );
}

#[test]
fn file_info_requests_the_file_and_returns_the_response_model() {
    let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
    let address = server
        .server_addr()
        .to_ip()
        .expect("mock Slack server should use an IP address");
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(r#"{"ok":true,"file":{"id":"F123","title":"Design"}}"#)
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("content type header should be valid"),
                    ),
            )
            .expect("mock Slack response should be sent");
        (path, body)
    });

    let token = StoredToken {
        access_token: "xoxp-test-token".to_string(),
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
    };
    let mut api = SlackApi::new(token);
    api.api_base_url = format!("http://{address}/api");
    let file = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.file("F123"))
        .expect("files.info should succeed");

    let (path, body) = received.join().expect("mock Slack server should finish");
    assert_eq!(path, "/api/files.info");
    let form = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    assert_eq!(form.get("file").map(String::as_str), Some("F123"));
    assert_eq!(file.id.as_deref(), Some("F123"));
    assert_eq!(file.display_title(), "Design");
}

#[test]
fn test_conversations_mark_parameter_validation() {
    let token = user_test_token();
    let api = SlackApi::new(token);
    let rt = tokio::runtime::Runtime::new().expect("test runtime should start");

    let res = rt.block_on(api.conversations_mark("", "1234.5678"));
    assert!(matches!(res, Err(SlackError::InvalidParameters)));

    let res = rt.block_on(api.conversations_mark("   \t\n", "1234.5678"));
    assert!(matches!(res, Err(SlackError::InvalidParameters)));

    let res = rt.block_on(api.conversations_mark("C12345", ""));
    assert!(matches!(res, Err(SlackError::InvalidParameters)));

    let res = rt.block_on(api.conversations_mark("C12345", "  \r\n"));
    assert!(matches!(res, Err(SlackError::InvalidParameters)));
}

#[test]
fn test_conversations_mark_request_formatting() {
    let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
    let address = server
        .server_addr()
        .to_ip()
        .expect("mock Slack server should use an IP address");
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(r#"{"ok":true}"#).with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        (path, body)
    });

    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");
    let result = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.conversations_mark("  C12345  ", "\t1234.5678\n"));
    assert!(result.is_ok());

    let (path, body) = received.join().expect("mock Slack server should finish");
    assert_eq!(path, "/api/conversations.mark");
    let form = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    assert_eq!(form.get("channel").map(String::as_str), Some("C12345"));
    assert_eq!(form.get("ts").map(String::as_str), Some("1234.5678"));
}

#[test]
fn test_subscriptions_thread_mark_parameter_validation() {
    let token = user_test_token();
    let api = SlackApi::new(token);
    let rt = tokio::runtime::Runtime::new().expect("test runtime should start");

    let res = rt.block_on(api.subscriptions_thread_mark("", "100.0", "100.1"));
    assert!(matches!(res, Err(SlackError::InvalidParameters)));

    let res = rt.block_on(api.subscriptions_thread_mark("C123", "", "100.1"));
    assert!(matches!(res, Err(SlackError::InvalidParameters)));

    let res = rt.block_on(api.subscriptions_thread_mark("C123", "100.0", ""));
    assert!(matches!(res, Err(SlackError::InvalidParameters)));

    let res = rt.block_on(api.subscriptions_thread_mark("   ", "  \n", "\t"));
    assert!(matches!(res, Err(SlackError::InvalidParameters)));
}

#[test]
fn test_subscriptions_thread_mark_request_formatting() {
    let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
    let address = server
        .server_addr()
        .to_ip()
        .expect("mock Slack server should use an IP address");
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(r#"{"ok":true}"#).with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        (path, body)
    });

    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");
    let result = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.subscriptions_thread_mark("  C12345  ", "\t1234.0001\n", " 1234.0002 "));
    assert!(result.is_ok());

    let (path, body) = received.join().expect("mock Slack server should finish");
    assert_eq!(path, "/api/subscriptions.thread.mark");
    let form = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    assert_eq!(form.get("channel").map(String::as_str), Some("C12345"));
    assert_eq!(form.get("thread_ts").map(String::as_str), Some("1234.0001"));
    assert_eq!(form.get("ts").map(String::as_str), Some("1234.0002"));
    assert_eq!(form.get("read").map(String::as_str), Some("1"));
}

fn thread_view_response(value: Value) -> ThreadViewResponse {
    serde_json::from_value(value).expect("thread view response should parse")
}

#[test]
fn followed_threads_page_keys_roots_by_channel_and_cursors_on_latest_reply() {
    let response = thread_view_response(json!({
        "ok": true,
        "has_more": true,
        "threads": [
            {"root_msg": {"type": "message", "ts": "3.0", "text": "a", "channel": "C1",
                "latest_reply": "9.0", "reply_count": 2, "subscribed": true}},
            {"root_msg": {"type": "message", "ts": "1.0", "text": "no channel"}},
            {"root_msg": {"type": "message", "ts": "2.0", "text": "b", "channel": "C2",
                "latest_reply": "8.0", "reply_count": 1}}
        ]
    }));

    let page = followed_threads_page(response, None);

    let keys = page
        .threads
        .iter()
        .map(|(channel_id, root)| (channel_id.as_str(), root.ts.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(keys, [("C1", "3.0"), ("C2", "2.0")]);
    assert_eq!(page.threads[0].1.subscribed, Some(true));
    assert_eq!(page.next_cursor.as_deref(), Some("8.0"));
}

#[test]
fn followed_threads_page_stops_without_more_or_when_cursor_stalls() {
    let root = json!({"root_msg": {"type": "message", "ts": "2.0", "text": "b", "channel": "C2"}});
    let last = followed_threads_page(
        thread_view_response(json!({"ok": true, "has_more": false, "threads": [root.clone()]})),
        None,
    );
    assert_eq!(last.next_cursor, None);

    let fallback = followed_threads_page(
        thread_view_response(json!({"ok": true, "has_more": true, "threads": [root.clone()]})),
        None,
    );
    assert_eq!(fallback.next_cursor.as_deref(), Some("2.0"));

    let stalled = followed_threads_page(
        thread_view_response(json!({"ok": true, "has_more": true, "threads": [root]})),
        Some("2.0"),
    );
    assert_eq!(stalled.next_cursor, None);
}

#[test]
fn followed_threads_page_request_formatting() {
    let server = Server::http(("127.0.0.1", 0)).expect("mock Slack server should bind");
    let address = server
        .server_addr()
        .to_ip()
        .expect("mock Slack server should use an IP address");
    let received = thread::spawn(move || {
        let mut request = server.recv().expect("mock Slack request should arrive");
        let path = request.url().to_string();
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .expect("mock Slack request body should be readable");
        request
            .respond(
                Response::from_string(
                    r#"{"ok":true,"has_more":false,"threads":[{"root_msg":{"type":"message","ts":"1.0","text":"hi","channel":"C1"},"latest_replies":[]}]}"#,
                )
                .with_header(
                    Header::from_bytes("Content-Type", "application/json")
                        .expect("content type header should be valid"),
                ),
            )
            .expect("mock Slack response should be sent");
        (path, body)
    });

    let mut api = SlackApi::new(user_test_token());
    api.api_base_url = format!("http://{address}/api");
    let page = tokio::runtime::Runtime::new()
        .expect("test runtime should start")
        .block_on(api.followed_threads_page(Some(" 1234.0001 ")))
        .expect("followed threads page should load");

    let (path, body) = received.join().expect("mock Slack server should finish");
    assert_eq!(path, "/api/subscriptions.thread.getView");
    let form = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect::<HashMap<_, _>>();
    assert_eq!(
        form.get("current_ts").map(String::as_str),
        Some("1234.0001")
    );
    assert_eq!(form.get("limit").map(String::as_str), Some("50"));
    assert_eq!(form.get("priority_mode").map(String::as_str), Some("all"));
    assert_eq!(page.threads.len(), 1);
    assert_eq!(page.threads[0].0, "C1");
    assert_eq!(page.next_cursor, None);
}

#[test]
fn conversation_members_params_trim_and_paginate() {
    assert_eq!(
        conversation_members_params(" C1 ", Some(" next ")).unwrap(),
        vec![
            ("channel", "C1".to_string()),
            ("limit", "200".to_string()),
            ("cursor", "next".to_string()),
        ]
    );
    assert_eq!(
        conversation_members_params("C1", Some("  ")).unwrap().len(),
        2
    );
    assert!(conversation_members_params("  ", None).is_err());
}

#[test]
fn conversation_text_params_use_field_name() {
    assert_eq!(
        conversation_text_params("C1", "topic", "  hello  ").unwrap(),
        vec![
            ("channel", "C1".to_string()),
            ("topic", "hello".to_string())
        ]
    );
    assert_eq!(
        conversation_text_params("C1", "purpose", "").unwrap()[1],
        ("purpose", String::new())
    );
    assert!(conversation_text_params("", "topic", "x").is_err());
}
