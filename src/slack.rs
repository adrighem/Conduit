use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use reqwest::header::{CONTENT_TYPE, COOKIE, RETRY_AFTER, USER_AGENT};
use reqwest::multipart::Form;
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::auth::browser_session_cookie_header;
use crate::client_counts::ClientCountsPayload;
use crate::http_client;
use crate::models::{
    AuthInfo, SavedItem, SearchMatch, SlackAttachment, SlackConversation, SlackFile, SlackMessage,
    SlackMessageEdit, SlackUser, SlackUserGroup, SlackUserProfile, SlackUserStatus, StoredToken,
};
use crate::rich_message::SlackControlAction;
use crate::search::{
    SearchField, SearchQuery, ID_FIELD_WEIGHT, PRIMARY_FIELD_WEIGHT, SECONDARY_FIELD_WEIGHT,
};
use crate::slack_message_wire::{deserialize_message, deserialize_messages};
use crate::unread_ledger::ServerReadCounts;

const MAX_UPLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_MEDIA_DOWNLOAD_BYTES: u64 = MAX_UPLOAD_BYTES;
pub(crate) const MAX_PREVIEW_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_PREVIEW_VIDEO_BYTES: usize = 16 * 1024 * 1024;
const MAX_RATE_LIMIT_RETRIES: usize = 2;
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(10);
const API_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_RETRY_AFTER_SECONDS: u64 = 1;
const MAX_RETRY_AFTER_SECONDS: u64 = 300;
pub(crate) const CHANNEL_HISTORY_PAGE_LIMIT: usize = 30;
pub(crate) const MESSAGE_CONTEXT_LIMIT: usize = 15;
const THREAD_HISTORY_PAGE_LIMIT: usize = 50;
const FOLLOWED_THREADS_PAGE_LIMIT: usize = 50;
/// Upper bound on pages fetched to reach a search hit deep in a thread.
const THREAD_CONTEXT_MAX_PAGES: usize = 20;
const DEFAULT_DEBUG_CONVERSATION_PROPERTY_LIMIT: usize = 20;
const DEBUG_CONVERSATION_PROPERTIES_ENV: &str = "CONDUIT_DEBUG_CONVERSATION_PROPERTIES";
const CONVERSATIONS_LIST_METHOD: &str = "conversations.list";
const USERS_CONVERSATIONS_METHOD: &str = "users.conversations";
const USERS_LIST_METHOD: &str = "users.list";
const SLACK_API_BASE_URL: &str = "https://slack.com/api";
static NEXT_CLIENT_MESSAGE_ID: AtomicU64 = AtomicU64::new(0);

fn next_client_message_id() -> String {
    let counter = NEXT_CLIENT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let digest = Sha256::digest(format!("{}:{now}:{counter}", std::process::id()).as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn browser_action_client_token() -> String {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("web-{milliseconds}")
}

fn parse_private_action_json(value: &str) -> Result<Value> {
    let action: Value = serde_json::from_str(value)
        .map_err(|_| SlackError::validation("Slack message action metadata is invalid"))?;
    let Some(action_object) = action.as_object() else {
        return Err(SlackError::validation(
            "Slack message action metadata is invalid",
        ));
    };
    if action_object.get("type").and_then(Value::as_str) != Some("button")
        || action_object.contains_key("confirm")
        || action_object.contains_key("url")
    {
        return Err(SlackError::validation(
            "Slack message action metadata is unsupported",
        ));
    }
    Ok(action)
}

pub type Result<T> = std::result::Result<T, SlackError>;

pub(crate) fn constructed_message_permalink(
    workspace_url: &str,
    channel_id: &str,
    message_ts: &str,
) -> Option<String> {
    let target = crate::message_handoff::MessageRef::new(channel_id, message_ts).ok()?;
    crate::message_handoff::SafeSlackPermalink::construct(workspace_url, &target)
        .ok()
        .map(|permalink| permalink.as_str().to_string())
}

#[cfg(test)]
pub(crate) fn validated_message_permalink(
    permalink: &str,
    workspace_url: &str,
    channel_id: &str,
    message_ts: &str,
) -> Option<String> {
    let target = crate::message_handoff::MessageRef::new(channel_id, message_ts).ok()?;
    crate::message_handoff::SafeSlackPermalink::validate_authoritative(
        permalink,
        workspace_url,
        &target,
    )
    .ok()
    .map(|permalink| permalink.as_str().to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlackErrorCategory {
    Authentication,
    Connectivity,
    RateLimited,
    LocalIo,
    Validation,
    Unexpected,
}

#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    #[error("Slack method {method} failed: {code}")]
    Api { method: String, code: String },
    #[error("Slack method {method} was rate limited; try again shortly")]
    RateLimited { method: String },
    #[error("{message}")]
    Validation { message: String },
    #[error("Invalid parameters")]
    InvalidParameters,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl SlackError {
    fn api(method: impl Into<String>, code: impl Into<String>) -> Self {
        Self::Api {
            method: method.into(),
            code: code.into(),
        }
    }

    fn validation(message: impl Into<String>) -> Self {
        Self::Validation {
            message: message.into(),
        }
    }

    pub fn category(&self) -> SlackErrorCategory {
        match self {
            Self::Api { code, .. } if slack_error_code_requires_authentication(code) => {
                SlackErrorCategory::Authentication
            }
            Self::Api { code, .. } if slack_error_code_is_rate_limited(code) => {
                SlackErrorCategory::RateLimited
            }
            Self::Api { .. } => SlackErrorCategory::Unexpected,
            Self::RateLimited { .. } => SlackErrorCategory::RateLimited,
            Self::Validation { .. } | Self::InvalidParameters => SlackErrorCategory::Validation,
            Self::Other(error) => classify_wrapped_slack_error(error),
        }
    }

    pub fn is_permission_denied(&self) -> bool {
        matches!(
            self,
            Self::Api { code, .. }
                if matches!(
                    code.as_str(),
                    "access_denied"
                        | "cant_invite"
                        | "invitee_cant_see_channel"
                        | "missing_scope"
                        | "no_external_invite_permission"
                        | "no_permission"
                        | "not_in_channel"
                        | "restricted_action"
                        | "user_is_restricted"
                )
        )
    }
}

fn slack_error_code_requires_authentication(code: &str) -> bool {
    matches!(
        code,
        "account_inactive" | "invalid_auth" | "not_authed" | "token_expired" | "token_revoked"
    )
}

fn slack_error_code_is_rate_limited(code: &str) -> bool {
    matches!(code, "ratelimited" | "rate_limited")
}

fn classify_wrapped_slack_error(error: &anyhow::Error) -> SlackErrorCategory {
    for source in error.chain() {
        if let Some(request) = source.downcast_ref::<reqwest::Error>() {
            if request.status().is_some_and(|status| {
                status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
            }) {
                return SlackErrorCategory::Authentication;
            }
            if request.is_timeout() || request.is_connect() || request.is_request() {
                return SlackErrorCategory::Connectivity;
            }
        }
        if let Some(io) = source.downcast_ref::<std::io::Error>() {
            return match io.kind() {
                std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::TimedOut => SlackErrorCategory::Connectivity,
                _ => SlackErrorCategory::LocalIo,
            };
        }
    }
    SlackErrorCategory::Unexpected
}

fn workspace_search_api_query(query: &str) -> String {
    let mut in_quoted_phrase = false;
    query
        .split_whitespace()
        .map(|term| {
            let quoted = in_quoted_phrase || term.contains('"');
            if term.matches('"').count() % 2 == 1 {
                in_quoted_phrase = !in_quoted_phrase;
            }
            if quoted
                || workspace_search_term_is_modifier(term)
                || term.contains('*')
                || term.chars().count() < 3
            {
                term.to_string()
            } else {
                format!("{term}*")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn workspace_search_term_is_modifier(term: &str) -> bool {
    term.starts_with('-') || term.contains(':')
}

fn workspace_search_content_query(query: &str) -> String {
    let mut in_quoted_modifier = false;
    query
        .split_whitespace()
        .filter(|term| {
            if in_quoted_modifier {
                if term.matches('"').count() % 2 == 1 {
                    in_quoted_modifier = false;
                }
                return false;
            }
            if workspace_search_term_is_modifier(term) {
                if term.matches('"').count() % 2 == 1 {
                    in_quoted_modifier = true;
                }
                return false;
            }
            true
        })
        .map(|term| term.trim_matches(['"', '*']))
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn filter_workspace_search_matches(query: &str, matches: Vec<SearchMatch>) -> Vec<SearchMatch> {
    let content_query = workspace_search_content_query(query);
    let search_query = SearchQuery::parse(&content_query);
    let mut ranked = matches
        .into_iter()
        .enumerate()
        .filter_map(|(original_index, item)| {
            let score = search_query.score([
                SearchField::new(
                    item.text.as_deref().unwrap_or_default(),
                    PRIMARY_FIELD_WEIGHT,
                ),
                SearchField::new(
                    item.username.as_deref().unwrap_or_default(),
                    SECONDARY_FIELD_WEIGHT,
                ),
                SearchField::new(item.user.as_deref().unwrap_or_default(), ID_FIELD_WEIGHT),
                SearchField::new(
                    item.channel
                        .as_ref()
                        .and_then(|channel| channel.name.as_deref())
                        .unwrap_or_default(),
                    SECONDARY_FIELD_WEIGHT,
                ),
                SearchField::new(
                    item.channel
                        .as_ref()
                        .and_then(|channel| channel.id.as_deref())
                        .unwrap_or_default(),
                    ID_FIELD_WEIGHT,
                ),
            ])?;
            Some((score, original_index, item))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(
        |(left_score, left_index, _), (right_score, right_index, _)| {
            right_score
                .band()
                .cmp(&left_score.band())
                .then_with(|| left_index.cmp(right_index))
        },
    );
    ranked.into_iter().map(|(_, _, item)| item).collect()
}

#[derive(Clone)]
pub struct SlackApi {
    /// General client for unauthenticated or header-only GETs (asset
    /// downloads, upload URLs); follows redirects to public HTTPS hosts.
    http: Client,
    /// Client for requests carrying the token in the body; never follows a
    /// cross-origin redirect, since reqwest replays the body on 307/308.
    api_http: Client,
    pub(crate) api_base_url: String,
    access_token: String,
    browser_cookie_d: Option<String>,
    user_agent: Option<String>,
}

async fn send_tracked_slack_request(
    label: &str,
    request: reqwest::RequestBuilder,
) -> reqwest::Result<reqwest::Response> {
    crate::debug::pipeline_counters().record_api_request();
    crate::debug::record_api_request_method(label);
    request.send().await
}

#[derive(Clone, PartialEq, Eq)]
pub struct SlackMessageActionRequest {
    pub(crate) channel_id: String,
    pub(crate) message_ts: String,
    pub(crate) thread_ts: Option<String>,
    pub(crate) service_id: String,
    pub(crate) app_id: Option<String>,
    pub(crate) bot_user_id: Option<String>,
    pub(crate) is_ephemeral: bool,
    pub(crate) action: SlackControlAction,
}

impl std::fmt::Debug for SlackMessageActionRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SlackMessageActionRequest")
            .field("channel_id", &self.channel_id)
            .field("message_ts", &self.message_ts)
            .field("thread_ts", &self.thread_ts)
            .field("service_id", &"[REDACTED]")
            .field("app_id", &self.app_id.as_ref().map(|_| "[REDACTED]"))
            .field(
                "bot_user_id",
                &self.bot_user_id.as_ref().map(|_| "[REDACTED]"),
            )
            .field("action", &self.action)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SidebarTheme {
    pub column_bg: String,
    pub menu_bg_hover: String,
    pub active_item: String,
    pub active_item_text: String,
    pub hover_item: String,
    pub text_color: String,
    pub active_presence: String,
    pub mention_badge: Option<String>,
    pub top_nav_bg: Option<String>,
    pub top_nav_text: Option<String>,
}

impl SidebarTheme {
    pub fn default_aubergine() -> Self {
        preset_sidebar_theme("aubergine").expect("default aubergine theme must be valid")
    }
}

fn normalize_sidebar_theme_hex(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.starts_with('#')
        && trimmed.len() == 7
        && trimmed[1..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Some(trimmed.to_string());
    }
    if trimmed.len() == 6 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(format!("#{trimmed}"));
    }
    None
}

pub fn preset_sidebar_theme(name: &str) -> Option<SidebarTheme> {
    let normalized = name.trim().to_ascii_lowercase().replace('-', "_");
    match normalized.as_str() {
        "default" | "aubergine" => Some(SidebarTheme {
            column_bg: "#3F0E40".to_string(),
            menu_bg_hover: "#350d36".to_string(),
            active_item: "#1164A3".to_string(),
            active_item_text: "#FFFFFF".to_string(),
            hover_item: "#350D36".to_string(),
            text_color: "#FFFFFF".to_string(),
            active_presence: "#2BAC76".to_string(),
            mention_badge: Some("#CD2553".to_string()),
            top_nav_bg: Some("#350d36".to_string()),
            top_nav_text: Some("#FFFFFF".to_string()),
        }),
        "aubergine_classic" => parse_sidebar_theme_csv(
            "#4D394B,#3E313C,#4C9689,#FFFFFF,#3E313C,#FFFFFF,#38978D,#EB4D5C,#3E313C,#FFFFFF",
        ),
        "hoth" => parse_sidebar_theme_csv(
            "#F8F8FA,#FFFFFF,#2D9EE0,#FFFFFF,#FFFFFF,#383F45,#60D156,#FF4743,#FFFFFF,#383F45",
        ),
        "monument" => parse_sidebar_theme_csv(
            "#0D7E83,#076569,#F78640,#FFFFFF,#0B6C70,#FFFFFF,#F78640,#F78640,#076569,#FFFFFF",
        ),
        "chocolate" | "choco_mint" => parse_sidebar_theme_csv(
            "#54453B,#42362E,#5DB09D,#FFFFFF,#4A3D34,#FFFFFF,#5DB09D,#FFFFFF,#42362E,#FFFFFF",
        ),
        "ochin" => parse_sidebar_theme_csv(
            "#303E4D,#2C3846,#6698C8,#FFFFFF,#2A3542,#FFFFFF,#94E864,#783653,#2C3846,#FFFFFF",
        ),
        "goth" | "dark" => parse_sidebar_theme_csv(
            "#1A1D21,#222529,#1264A3,#FFFFFF,#222529,#D1D2D3,#007A5A,#E01E5A,#1A1D21,#D1D2D3",
        ),
        "solaris" => parse_sidebar_theme_csv(
            "#FCF8E3,#F3ECD0,#C55F37,#FFFFFF,#EDE4BE,#383F45,#60D156,#FF4743,#F3ECD0,#383F45",
        ),
        "brin" => parse_sidebar_theme_csv(
            "#203E5F,#1A324C,#569F8B,#FFFFFF,#1D3856,#FFFFFF,#60D156,#CD2553,#1A324C,#FFFFFF",
        ),
        "sweet_treat" => parse_sidebar_theme_csv(
            "#F2D388,#E8C97E,#C98474,#FFFFFF,#DFC075,#383F45,#874C62,#C98474,#E8C97E,#383F45",
        ),
        "mood_indigo" => parse_sidebar_theme_csv(
            "#333456,#2C2D4A,#595B83,#FFFFFF,#2C2D4A,#FFFFFF,#595B83,#F4ABC4,#2C2D4A,#FFFFFF",
        ),
        "dagobah" => parse_sidebar_theme_csv(
            "#353F3E,#2C3534,#94A187,#FFFFFF,#2F3837,#FFFFFF,#94A187,#C1A87D,#2C3534,#FFFFFF",
        ),
        "pro_hunter" => parse_sidebar_theme_csv(
            "#374046,#2F363C,#61605C,#FFFFFF,#2F363C,#FFFFFF,#61605C,#D24726,#2F363C,#FFFFFF",
        ),
        _ => None,
    }
}

fn parse_sidebar_theme_csv(raw: &str) -> Option<SidebarTheme> {
    let tokens: Vec<String> = raw
        .split(',')
        .filter_map(normalize_sidebar_theme_hex)
        .collect();
    if tokens.len() < 7 {
        return None;
    }
    Some(SidebarTheme {
        column_bg: tokens[0].clone(),
        menu_bg_hover: tokens[1].clone(),
        active_item: tokens[2].clone(),
        active_item_text: tokens[3].clone(),
        hover_item: tokens[4].clone(),
        text_color: tokens[5].clone(),
        active_presence: tokens[6].clone(),
        mention_badge: tokens.get(7).cloned(),
        top_nav_bg: tokens.get(8).cloned(),
        top_nav_text: tokens.get(9).cloned(),
    })
}

fn parse_sidebar_theme_str(raw: &str) -> Option<SidebarTheme> {
    let trimmed = raw.trim();
    if let Some(theme) = parse_sidebar_theme_csv(trimmed) {
        return Some(theme);
    }
    if let Some(theme) = preset_sidebar_theme(trimmed) {
        return Some(theme);
    }
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            return parse_sidebar_theme_value(&val);
        }
    }
    None
}

fn parse_sidebar_theme_value(val: &Value) -> Option<SidebarTheme> {
    match val {
        Value::String(s) => parse_sidebar_theme_str(s),
        Value::Object(map) => {
            let column_bg = map
                .get("column_bg")
                .and_then(Value::as_str)
                .and_then(normalize_sidebar_theme_hex)?;
            let menu_bg_hover = map
                .get("menu_bg_hover")
                .or_else(|| map.get("menu_bg"))
                .and_then(Value::as_str)
                .and_then(normalize_sidebar_theme_hex)?;
            let active_item = map
                .get("active_item")
                .and_then(Value::as_str)
                .and_then(normalize_sidebar_theme_hex)?;
            let active_item_text = map
                .get("active_item_text")
                .and_then(Value::as_str)
                .and_then(normalize_sidebar_theme_hex)?;
            let hover_item = map
                .get("hover_item")
                .and_then(Value::as_str)
                .and_then(normalize_sidebar_theme_hex)?;
            let text_color = map
                .get("text_color")
                .and_then(Value::as_str)
                .and_then(normalize_sidebar_theme_hex)?;
            let active_presence = map
                .get("active_presence")
                .or_else(|| map.get("presence"))
                .and_then(Value::as_str)
                .and_then(normalize_sidebar_theme_hex)?;
            let mention_badge = map
                .get("mention_badge")
                .and_then(Value::as_str)
                .and_then(normalize_sidebar_theme_hex);
            let top_nav_bg = map
                .get("top_nav_bg")
                .or_else(|| map.get("nav_bg"))
                .and_then(Value::as_str)
                .and_then(normalize_sidebar_theme_hex);
            let top_nav_text = map
                .get("top_nav_text")
                .or_else(|| map.get("nav_text"))
                .and_then(Value::as_str)
                .and_then(normalize_sidebar_theme_hex);

            Some(SidebarTheme {
                column_bg,
                menu_bg_hover,
                active_item,
                active_item_text,
                hover_item,
                text_color,
                active_presence,
                mention_badge,
                top_nav_bg,
                top_nav_text,
            })
        }
        _ => None,
    }
}

/// Slack's current theme pref (`ia_theme`): four picked colours plus the
/// "darker sidebars" switch. The sidebar is a light or dark tint of the
/// primary colour; the gradient setting is ignored.
fn parse_ia_theme(val: &Value) -> Option<SidebarTheme> {
    let parsed;
    let map = match val {
        Value::String(raw) => {
            parsed = serde_json::from_str::<Value>(raw).ok()?;
            parsed.as_object()?
        }
        Value::Object(map) => map,
        _ => return None,
    };
    let use_custom = map
        .get("useCustomHex")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let color = |key: &str| -> Option<String> {
        let entry = map.get(key)?;
        let custom = || {
            entry
                .get("custom")
                .and_then(Value::as_str)
                .and_then(normalize_sidebar_theme_hex)
        };
        let palette = || {
            entry
                .get("palette")
                .and_then(Value::as_str)
                .and_then(ia_theme_palette_hex)
                .map(str::to_string)
        };
        if use_custom {
            custom().or_else(palette)
        } else {
            palette().or_else(custom)
        }
    };
    let primary = color("primary")?;
    let selected = color("highlight1").unwrap_or_else(|| primary.clone());
    let presence = color("highlight2").unwrap_or_else(|| "#2BAC76".to_string());
    let important = color("important");
    let darker = map
        .get("sidebarInverted")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (column_bg, text_color) = if darker {
        (mix_hex(&primary, "#FFFFFF", 0.1)?, "#FFFFFF".to_string())
    } else {
        (
            mix_hex(&primary, "#FFFFFF", 0.9)?,
            mix_hex(&primary, "#000000", 0.5)?,
        )
    };
    let hover = mix_hex(&column_bg, &text_color, 0.1)?;
    Some(SidebarTheme {
        menu_bg_hover: hover.clone(),
        hover_item: hover,
        active_item_text: contrast_text_hex(&selected)?,
        active_item: selected,
        text_color,
        column_bg,
        active_presence: presence,
        mention_badge: important,
        top_nav_text: Some(contrast_text_hex(&primary)?),
        top_nav_bg: Some(primary),
    })
}

/// Hex values of Slack's named theme palettes, where known. Unknown names
/// fall back to the entry's `custom` hex.
fn ia_theme_palette_hex(name: &str) -> Option<&'static str> {
    match name {
        "aubergine" => Some("#4A154B"),
        _ => None,
    }
}

fn hex_rgb(hex: &str) -> Option<[f64; 3]> {
    let hex = normalize_sidebar_theme_hex(hex)?;
    let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok().map(f64::from);
    Some([channel(1)?, channel(3)?, channel(5)?])
}

/// `base` moved `amount` (0..=1) of the way towards `toward`.
fn mix_hex(base: &str, toward: &str, amount: f64) -> Option<String> {
    let (base, toward) = (hex_rgb(base)?, hex_rgb(toward)?);
    let mixed = |i: usize| (base[i] + (toward[i] - base[i]) * amount).round() as u8;
    Some(format!("#{:02X}{:02X}{:02X}", mixed(0), mixed(1), mixed(2)))
}

/// White or near-black text, whichever reads better on `background`.
fn contrast_text_hex(background: &str) -> Option<String> {
    let [r, g, b] = hex_rgb(background)?;
    let luma = (0.299 * r + 0.587 * g + 0.114 * b) / 255.0;
    Some(if luma > 0.6 { "#1D1C1D" } else { "#FFFFFF" }.to_string())
}

impl SlackApi {
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn browser_cookie_d(&self) -> Option<&str> {
        self.browser_cookie_d.as_deref()
    }

    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    pub fn new(token: StoredToken) -> Self {
        Self {
            http: http_client::builder()
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .read_timeout(HTTP_READ_TIMEOUT)
                .build()
                .expect("valid Slack HTTP client configuration"),
            api_http: http_client::api_builder()
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .read_timeout(HTTP_READ_TIMEOUT)
                .build()
                .expect("valid Slack API HTTP client configuration"),
            api_base_url: SLACK_API_BASE_URL.to_string(),
            access_token: token.access_token,
            browser_cookie_d: token.browser_cookie_d,
            user_agent: token.user_agent,
        }
    }

    pub async fn auth_test(&self) -> Result<AuthInfo> {
        let response: AuthTestResponse = self.post_form("auth.test", &[]).await?;
        Ok(AuthInfo {
            team: response.team,
            team_id: response.team_id,
            user: response.user,
            user_id: response.user_id,
            url: response.url,
        })
    }

    /// The workspace icon (`team.info`), largest square variant first; `None`
    /// when the workspace uses Slack's default icon.
    pub(crate) async fn team_icon_url(&self) -> Result<Option<String>> {
        let response: TeamInfoResponse = self.post_form("team.info", &[]).await?;
        let Some(icon) = response.team.and_then(|team| team.icon) else {
            return Ok(None);
        };
        if icon.image_default == Some(true) {
            return Ok(None);
        }
        Ok([icon.image_132, icon.image_88, icon.image_68]
            .into_iter()
            .flatten()
            .find(|url| url.starts_with("https://")))
    }

    pub(crate) async fn fetch_sidebar_theme(&self) -> Result<Option<SidebarTheme>> {
        let mut prefs = None;
        match self
            .post_form::<ClientUserBootResponse>("client.userBoot", &[])
            .await
        {
            Ok(response) => {
                prefs = response.self_data.and_then(|s| s.prefs);
            }
            Err(error) => {
                crate::debug::log(
                    "slack",
                    &format!("client.userBoot failed, trying users.prefs.get error={error:#}"),
                );
            }
        }
        if prefs.is_none() {
            match self
                .post_form::<UsersPrefsGetResponse>("users.prefs.get", &[])
                .await
            {
                Ok(response) => {
                    prefs = response.prefs;
                }
                Err(error) => {
                    crate::debug::log("slack", &format!("users.prefs.get failed error={error:#}"));
                }
            }
        }

        let theme = prefs.as_ref().and_then(ClientUserBootPrefs::sidebar_theme);

        if let Some(theme) = theme {
            return Ok(Some(theme));
        }

        Ok(Some(SidebarTheme::default_aubergine()))
    }

    pub(crate) async fn execute_message_action(
        &self,
        workspace_url: &str,
        team_id: &str,
        request: &SlackMessageActionRequest,
    ) -> Result<()> {
        self.ensure_browser_session_credentials()?;
        if team_id.trim().is_empty()
            || request.channel_id.trim().is_empty()
            || request.message_ts.trim().is_empty()
            || request.service_id.trim().is_empty()
        {
            return Err(SlackError::validation(
                "Slack message action metadata is incomplete",
            ));
        }
        let api_base_url = self.browser_workspace_api_base_url(workspace_url)?;
        let client_token = browser_action_client_token();
        let common = [
            ("_x_app_name", "client".to_string()),
            ("_x_mode", "online".to_string()),
            ("_x_sonic", "true".to_string()),
        ];

        match &request.action {
            SlackControlAction::Block {
                action,
                attachment_id,
            } => {
                let action = parse_private_action_json(action.expose())?;
                let mut container = json!({
                    "type": "message",
                    "message_ts": request.message_ts,
                    "channel_id": request.channel_id,
                    "is_ephemeral": request.is_ephemeral,
                });
                if let Some(attachment_id) = attachment_id {
                    container["type"] = Value::String("message_attachment".to_string());
                    container["attachment_id"] = Value::from(*attachment_id);
                }
                if let Some(thread_ts) = request.thread_ts.as_ref() {
                    container["thread_ts"] = Value::String(thread_ts.clone());
                }
                let mut params = vec![
                    ("service_id", request.service_id.clone()),
                    ("service_team_id", team_id.to_string()),
                    ("actions", Value::Array(vec![action]).to_string()),
                    ("container", container.to_string()),
                    ("client_token", client_token),
                    ("state", json!({ "values": {} }).to_string()),
                    ("_x_reason", "dispatch_action_to_developer".to_string()),
                ];
                if let Some(app_id) = request.app_id.as_ref() {
                    params.push(("app_id", app_id.clone()));
                }
                params.extend(common);
                let _: MessageActionResponse = self
                    .post_browser_form(&api_base_url, "blocks.actions", &[], &params)
                    .await?;
            }
            SlackControlAction::LegacyAttachment {
                attachment_id,
                callback_id,
                action,
            } => {
                let action = parse_private_action_json(action.expose())?;
                let mut payload = json!({
                    "actions": [action],
                    "attachment_id": attachment_id,
                    "callback_id": callback_id.expose(),
                    "channel_id": request.channel_id,
                    "is_ephemeral": request.is_ephemeral,
                    "message_ts": request.message_ts,
                    "prompt_app_install": false,
                    "team_id": team_id,
                });
                if let Some(thread_ts) = request.thread_ts.as_ref() {
                    payload["thread_ts"] = Value::String(thread_ts.clone());
                }
                let bot_user_id = request.bot_user_id.as_ref().ok_or_else(|| {
                    SlackError::validation("Slack message action metadata is incomplete")
                })?;
                let mut params = vec![
                    ("payload", payload.to_string()),
                    ("client_token", client_token),
                    ("service_id", request.service_id.clone()),
                    ("bot_user_id", bot_user_id.clone()),
                    ("_x_reason", "user_attachment_action_dispatch".to_string()),
                ];
                if let Some(app_id) = request.app_id.as_ref() {
                    params.push(("app_id", app_id.clone()));
                }
                params.extend(common);
                let _: MessageActionResponse = self
                    .post_browser_form(&api_base_url, "chat.attachmentAction", &[], &params)
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn conversations(&self) -> Result<Vec<SlackConversation>> {
        let mut cursor: Option<String> = None;
        let mut conversations = Vec::new();

        loop {
            let mut params = vec![
                (
                    "types",
                    "public_channel,private_channel,mpim,im".to_string(),
                ),
                ("exclude_archived", "false".to_string()),
                ("limit", "200".to_string()),
            ];
            if let Some(cursor) = cursor.as_ref() {
                params.push(("cursor", cursor.clone()));
            }

            let response: ConversationListResponse =
                self.post_form(USERS_CONVERSATIONS_METHOD, &params).await?;
            conversations.extend(response.channels);

            cursor = response
                .response_metadata
                .and_then(|metadata| metadata.next_cursor);
            if cursor.as_deref().unwrap_or_default().is_empty() {
                break;
            }
        }

        conversations.sort_by_key(|conversation| conversation.display_name().to_lowercase());
        log_conversation_properties(USERS_CONVERSATIONS_METHOD, &conversations);
        Ok(conversations)
    }

    /// Lists every accessible public or private channel, including channels the
    /// current user has not joined yet.
    pub async fn discover_conversations(&self) -> Result<Vec<SlackConversation>> {
        let mut cursor: Option<String> = None;
        let mut conversations = Vec::new();

        loop {
            let params = paginated_list_params(cursor.as_deref(), true);
            let response: ConversationListResponse =
                self.post_form(CONVERSATIONS_LIST_METHOD, &params).await?;
            conversations.extend(
                response
                    .channels
                    .into_iter()
                    .filter(is_discoverable_conversation),
            );

            cursor = next_cursor(response.response_metadata);
            if cursor.is_none() {
                break;
            }
        }

        conversations.sort_by_key(|conversation| conversation.display_name().to_lowercase());
        Ok(conversations)
    }

    /// Lists workspace users across every page returned by Slack.
    pub async fn users(&self) -> Result<Vec<SlackUser>> {
        let mut cursor: Option<String> = None;
        let mut users = Vec::new();

        loop {
            let params = paginated_list_params(cursor.as_deref(), false);
            let response: UsersListResponse = self.post_form(USERS_LIST_METHOD, &params).await?;
            users.extend(response.members);

            cursor = next_cursor(response.response_metadata);
            if cursor.is_none() {
                break;
            }
        }

        users.sort_by_key(|user| user.display_name().unwrap_or_default().to_lowercase());
        Ok(users)
    }

    /// Lists workspace-defined emoji. Slack represents aliases as
    /// `alias:target`, which is intentionally preserved for catalog-level
    /// resolution.
    pub async fn custom_emojis(&self) -> Result<HashMap<String, String>> {
        let response: EmojiListResponse = self.post_form("emoji.list", &[]).await?;
        Ok(response.emoji)
    }

    pub async fn join_conversation(&self, channel_id: &str) -> Result<SlackConversation> {
        let response: ConversationJoinResponse = self
            .post_form("conversations.join", &[("channel", channel_id.to_string())])
            .await?;
        Ok(response.channel)
    }

    pub async fn leave_conversation(&self, channel_id: &str) -> Result<()> {
        let _: BasicResponse = self
            .post_form(
                "conversations.leave",
                &[("channel", channel_id.to_string())],
            )
            .await?;
        Ok(())
    }

    /// Fetches the full `conversations.info` record (topic, purpose, creator).
    pub async fn conversation_info(&self, channel_id: &str) -> Result<SlackConversation> {
        let channel_id = required_conversation_id(channel_id)?;
        let response: ConversationJoinResponse = self
            .post_form("conversations.info", &[("channel", channel_id)])
            .await?;
        Ok(response.channel)
    }

    /// Fetches one page of member IDs plus the cursor for the next page.
    pub async fn conversation_members_page(
        &self,
        channel_id: &str,
        cursor: Option<&str>,
    ) -> Result<(Vec<String>, Option<String>)> {
        let params = conversation_members_params(channel_id, cursor)?;
        let response: ConversationMembersResponse =
            self.post_form("conversations.members", &params).await?;
        Ok((response.members, next_cursor(response.response_metadata)))
    }

    pub async fn set_conversation_topic(&self, channel_id: &str, topic: &str) -> Result<()> {
        let params = conversation_text_params(channel_id, "topic", topic)?;
        let _: BasicResponse = self.post_form("conversations.setTopic", &params).await?;
        Ok(())
    }

    pub async fn set_conversation_purpose(&self, channel_id: &str, purpose: &str) -> Result<()> {
        let params = conversation_text_params(channel_id, "purpose", purpose)?;
        let _: BasicResponse = self.post_form("conversations.setPurpose", &params).await?;
        Ok(())
    }

    pub async fn conversations_mark(&self, channel_id: &str, ts: &str) -> Result<()> {
        let channel_id = channel_id.trim_matches(|c: char| c.is_ascii_whitespace());
        let ts = ts.trim_matches(|c: char| c.is_ascii_whitespace());
        if channel_id.is_empty() || ts.is_empty() {
            return Err(SlackError::InvalidParameters);
        }
        let _: BasicResponse = self
            .post_form(
                "conversations.mark",
                &[("channel", channel_id.to_string()), ("ts", ts.to_string())],
            )
            .await?;
        Ok(())
    }

    pub async fn subscriptions_thread_mark(
        &self,
        channel_id: &str,
        thread_ts: &str,
        ts: &str,
    ) -> Result<()> {
        let channel_id = channel_id.trim_matches(|c: char| c.is_ascii_whitespace());
        let thread_ts = thread_ts.trim_matches(|c: char| c.is_ascii_whitespace());
        let ts = ts.trim_matches(|c: char| c.is_ascii_whitespace());
        if channel_id.is_empty() || thread_ts.is_empty() || ts.is_empty() {
            return Err(SlackError::InvalidParameters);
        }
        let _: BasicResponse = self
            .post_form(
                "subscriptions.thread.mark",
                &[
                    ("channel", channel_id.to_string()),
                    ("thread_ts", thread_ts.to_string()),
                    ("ts", ts.to_string()),
                    ("read", "1".to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    /// One page of the followed-threads inbox the official client shows under
    /// Threads (`subscriptions.thread.getView`, undocumented). Pass the previous
    /// page's `next_cursor` to continue.
    pub async fn followed_threads_page(&self, cursor: Option<&str>) -> Result<FollowedThreadsPage> {
        let cursor = cursor
            .map(|cursor| cursor.trim().to_string())
            .filter(|cursor| !cursor.is_empty());
        let mut params = vec![
            ("limit", FOLLOWED_THREADS_PAGE_LIMIT.to_string()),
            ("priority_mode", "all".to_string()),
        ];
        if let Some(cursor) = cursor.as_ref() {
            params.push(("current_ts", cursor.clone()));
        }
        let response: ThreadViewResponse = self
            .post_form("subscriptions.thread.getView", &params)
            .await?;
        Ok(followed_threads_page(response, cursor.as_deref()))
    }

    /// Read-state baseline for every conversation (`client.counts`).
    pub async fn client_counts(&self) -> Result<Vec<ServerReadCounts>> {
        let response: ClientCountsResponse = self
            .post_form(
                "client.counts",
                &[
                    ("thread_counts_by_channel", "true".to_string()),
                    ("org_wide_aware", "true".to_string()),
                    ("include_file_channels", "true".to_string()),
                ],
            )
            .await?;
        Ok(response.payload.server_read_counts())
    }

    pub async fn open_direct_message(&self, user_id: &str) -> Result<SlackConversation> {
        self.open_direct_message_with_users(&[user_id.to_string()])
            .await
    }

    pub async fn open_direct_message_with_users(
        &self,
        user_ids: &[String],
    ) -> Result<SlackConversation> {
        let users = conversation_user_ids_param(user_ids, 8)?;
        let response: ConversationOpenResponse = self
            .post_form("conversations.open", &[("users", users)])
            .await?;
        Ok(response.channel)
    }

    pub async fn create_channel(&self, name: &str, is_private: bool) -> Result<SlackConversation> {
        let params = channel_creation_params(name, is_private)?;
        let response: ConversationJoinResponse =
            self.post_form("conversations.create", &params).await?;
        Ok(response.channel)
    }

    pub async fn invite_to_channel(
        &self,
        channel_id: &str,
        user_ids: &[String],
    ) -> Result<SlackConversation> {
        let users = conversation_user_ids_param(user_ids, 100)?;
        let response: ConversationJoinResponse = self
            .post_form(
                "conversations.invite",
                &[
                    ("channel", channel_id.to_string()),
                    ("users", users),
                    ("force", "true".to_string()),
                ],
            )
            .await?;
        Ok(response.channel)
    }

    pub async fn history(&self, channel_id: &str) -> Result<SlackMessagePage> {
        self.history_page(channel_id, None).await
    }

    pub async fn history_page(
        &self,
        channel_id: &str,
        cursor: Option<&str>,
    ) -> Result<SlackMessagePage> {
        let params = history_request_params(channel_id, cursor, CHANNEL_HISTORY_PAGE_LIMIT);

        let response: HistoryResponse = self.post_form("conversations.history", &params).await?;
        Ok(SlackMessagePage::from_response(
            response,
            std::convert::identity,
        ))
    }

    pub async fn history_context(
        &self,
        channel_id: &str,
        message_ts: &str,
    ) -> Result<SlackMessagePage> {
        let params = message_context_request_params(channel_id, message_ts);
        let response: HistoryResponse = self.post_form("conversations.history", &params).await?;
        Ok(SlackMessagePage::from_response(
            response,
            std::convert::identity,
        ))
    }

    pub async fn thread_replies(&self, channel_id: &str, ts: &str) -> Result<SlackMessagePage> {
        self.thread_replies_page(channel_id, ts, None).await
    }

    pub async fn thread_replies_page(
        &self,
        channel_id: &str,
        ts: &str,
        cursor: Option<&str>,
    ) -> Result<SlackMessagePage> {
        let mut params = vec![
            ("channel", channel_id.to_string()),
            ("ts", ts.to_string()),
            ("limit", THREAD_HISTORY_PAGE_LIMIT.to_string()),
        ];
        if let Some(cursor) = cursor.filter(|cursor| !cursor.trim().is_empty()) {
            params.push(("cursor", cursor.to_string()));
        }

        let response: HistoryResponse = self.post_form("conversations.replies", &params).await?;
        Ok(SlackMessagePage::from_response(
            response,
            thread_replies_in_history_order,
        ))
    }

    /// Thread pages from the start up to the one holding `message_ts`.
    /// `conversations.replies` pages oldest-first, so a `latest`-bounded
    /// request would miss replies past the first page.
    pub async fn thread_replies_context(
        &self,
        channel_id: &str,
        thread_ts: &str,
        message_ts: &str,
    ) -> Result<SlackMessagePage> {
        let mut pages = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..THREAD_CONTEXT_MAX_PAGES {
            let page = self
                .thread_replies_page(channel_id, thread_ts, cursor.as_deref())
                .await?;
            let found = page.messages.iter().any(|message| message.ts == message_ts);
            cursor = page.next_cursor.clone();
            pages.push(page);
            if found || cursor.is_none() {
                break;
            }
        }
        Ok(merge_thread_pages(pages))
    }

    pub async fn search_messages(&self, query: &str) -> Result<Vec<SearchMatch>> {
        let api_query = workspace_search_api_query(query);
        let response: SearchResponse = self
            .post_form(
                "search.messages",
                &[
                    ("query", api_query),
                    ("count", "100".to_string()),
                    ("page", "1".to_string()),
                ],
            )
            .await?;
        Ok(filter_workspace_search_matches(
            query,
            response.messages.matches,
        ))
    }

    pub async fn saved_items(&self) -> Result<Vec<SavedItem>> {
        self.star_items().await
    }

    pub async fn starred_conversation_ids(&self) -> Result<HashSet<String>> {
        Ok(self
            .star_items()
            .await?
            .into_iter()
            .filter_map(|item| match item.kind.as_deref() {
                Some("channel" | "im" | "mpim") => item.channel,
                Some("group") => item.group.or(item.channel),
                _ => None,
            })
            .filter(|id| !id.trim().is_empty())
            .collect())
    }

    pub async fn files(&self) -> Result<Vec<SlackFile>> {
        let response: FilesListResponse = self
            .post_form(
                "files.list",
                &[("count", "50".to_string()), ("page", "1".to_string())],
            )
            .await?;
        Ok(response.files)
    }

    pub async fn file(&self, file_id: &str) -> Result<SlackFile> {
        let response: FileInfoResponse = self
            .post_form("files.info", &[("file", file_id.to_string())])
            .await?;
        Ok(response.file)
    }

    #[allow(dead_code)]
    pub async fn user_display_name(&self, user_id: &str) -> Result<String> {
        Ok(self
            .user(user_id)
            .await?
            .display_name()
            .unwrap_or_else(|| user_id.to_string()))
    }

    pub async fn user(&self, user_id: &str) -> Result<SlackUser> {
        let response: UserInfoResponse = self
            .post_form("users.info", &[("user", user_id.to_string())])
            .await?;
        Ok(response.user)
    }

    pub async fn user_profile(&self, user_id: &str) -> Result<SlackUserProfile> {
        let response: UserProfileResponse = self
            .post_form(
                "users.profile.get",
                &[
                    ("user", user_id.to_string()),
                    ("include_labels", "true".to_string()),
                ],
            )
            .await?;
        response.profile.ok_or_else(|| {
            SlackError::Other(anyhow::anyhow!(
                "Slack users.profile.get response omitted the profile"
            ))
        })
    }

    pub async fn set_current_user_status(
        &self,
        status: &SlackUserStatus,
    ) -> Result<SlackUserProfile> {
        let status_text = status.text.trim();
        if status_text.chars().count() > 100 {
            return Err(SlackError::validation(
                "Slack status text must be 100 characters or fewer",
            ));
        }
        let emoji_name = status.emoji_name();
        let status_emoji = if emoji_name.is_empty() {
            String::new()
        } else {
            format!(":{emoji_name}:")
        };
        let profile = json!({
            "status_text": status_text,
            "status_emoji": status_emoji,
            "status_expiration": status.expiration.max(0),
        })
        .to_string();
        let response: UserProfileResponse = self
            .post_form("users.profile.set", &[("profile", profile)])
            .await?;
        response.profile.ok_or_else(|| {
            SlackError::Other(anyhow::anyhow!(
                "Slack users.profile.set response omitted the profile"
            ))
        })
    }

    #[allow(dead_code)]
    pub async fn user_groups(&self) -> Result<Vec<SlackUserGroup>> {
        let response: UserGroupsListResponse = self
            .post_form("usergroups.list", &[("include_users", "true".to_string())])
            .await?;
        Ok(response.usergroups)
    }

    pub async fn download_preview_asset(&self, url: &str) -> Result<DownloadedPreviewAsset> {
        if !supports_native_preview_asset_url(url) {
            return Err(SlackError::validation(
                "preview URL is not a trusted Slack asset URL",
            ));
        }
        let request = if preview_download_needs_auth(url) {
            self.authenticated_request(Method::GET, url)
        } else {
            self.http.get(url)
        };
        let response = send_tracked_slack_request("download_preview", request)
            .await
            .context("failed to download Slack preview asset")?
            .error_for_status()
            .context("Slack preview asset returned an HTTP error")?;

        let mime_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(PreviewAssetMime::parse)
            .ok_or_else(|| {
                SlackError::validation(
                    "Slack attachment preview returned an unsupported content type",
                )
            })?;

        let max_bytes = mime_type.max_bytes();

        if response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(SlackError::validation(
                "Slack attachment preview is too large",
            ));
        }

        let initial_capacity = response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default()
            .min(max_bytes);
        let mut bytes = Vec::with_capacity(initial_capacity);
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .context("failed to read Slack attachment preview bytes")?
        {
            append_bounded_preview_chunk(&mut bytes, &chunk, max_bytes)?;
        }

        if !mime_type.is_valid_payload(&bytes) {
            return Err(SlackError::validation(
                "Slack attachment preview bytes do not match the declared content type",
            ));
        }

        Ok(DownloadedPreviewAsset { mime_type, bytes })
    }

    /// Downloads viewable Slack media to `destination` without retaining the
    /// complete response in memory. The destination is replaced atomically
    /// after a successful download and never contains a partial response.
    pub async fn download_media(&self, url: &str, destination: &Path) -> Result<DownloadedMedia> {
        ensure_trusted_slack_download_url(url)?;
        let response = send_tracked_slack_request(
            "download_media",
            self.authenticated_request(Method::GET, url),
        )
        .await
        .context("failed to download Slack media")?
        .error_for_status()
        .context("Slack media returned an HTTP error")?;

        let mime_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(supported_media_mime_type)
            .ok_or_else(|| SlackError::validation("Slack media has an unsupported content type"))?
            .to_string();

        ensure_media_size(response.content_length())?;
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("failed to create the Slack media cache directory")?;
        }

        let partial_path = partial_download_path(destination);
        let parsed_preview_mime = PreviewAssetMime::parse(&mime_type);
        let result = async {
            let mut file = tokio::fs::File::create(&partial_path)
                .await
                .context("failed to create the Slack media cache file")?;
            let mut response = response;
            let mut size = 0_u64;
            let mut prefix = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .context("failed to read Slack media bytes")?
            {
                if prefix.len() < 64 {
                    let needed = (64 - prefix.len()).min(chunk.len());
                    prefix.extend_from_slice(&chunk[..needed]);
                }
                size = size
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| SlackError::validation("Slack media is larger than 1 GiB"))?;
                ensure_media_size(Some(size))?;
                file.write_all(&chunk)
                    .await
                    .context("failed to write the Slack media cache file")?;
            }
            if let Some(parsed) = parsed_preview_mime {
                if !parsed.validate_signature(&prefix) {
                    return Err(SlackError::validation(
                        "Slack media bytes do not match the declared content type",
                    ));
                }
            }
            file.flush()
                .await
                .context("failed to flush the Slack media cache file")?;
            drop(file);
            tokio::fs::rename(&partial_path, destination)
                .await
                .context("failed to finalize the Slack media cache file")?;
            Ok::<_, SlackError>(size)
        }
        .await;

        match result {
            Ok(size) => Ok(DownloadedMedia {
                path: destination.to_path_buf(),
                mime_type,
                size,
            }),
            Err(error) => {
                let _ = tokio::fs::remove_file(&partial_path).await;
                Err(error)
            }
        }
    }

    /// Downloads a private Slack attachment to a local cache path. Credentials
    /// are only attached after the URL has been restricted to Slack-owned HTTPS
    /// hosts, so an attachment can never forward the session to another host.
    pub async fn download_attachment<F>(
        &self,
        url: &str,
        destination: &Path,
        progress: F,
    ) -> Result<DownloadedAttachment>
    where
        F: Fn(DownloadProgressUpdate),
    {
        ensure_trusted_slack_download_url(url)?;

        if let Ok(metadata) = tokio::fs::metadata(destination).await {
            if metadata.is_file() && metadata.len() > 0 {
                ensure_attachment_size(Some(metadata.len()))?;
                progress(DownloadProgressUpdate::new(1.0, "Attachment ready"));
                return Ok(DownloadedAttachment {
                    path: destination.to_path_buf(),
                    size: metadata.len(),
                });
            }
        }

        progress(DownloadProgressUpdate::new(0.05, "Starting download"));
        let response = send_tracked_slack_request(
            "download_attachment",
            self.authenticated_request(Method::GET, url),
        )
        .await
        .context("failed to download Slack attachment")?
        .error_for_status()
        .context("Slack attachment returned an HTTP error")?;
        let expected_size = response.content_length();
        ensure_attachment_size(expected_size)?;

        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("failed to create the Slack attachment cache directory")?;
        }

        let partial_path = partial_download_path(destination);
        let result = async {
            let mut file = tokio::fs::File::create(&partial_path)
                .await
                .context("failed to create the Slack attachment cache file")?;
            let mut response = response;
            let mut size = 0_u64;
            while let Some(chunk) = response
                .chunk()
                .await
                .context("failed to read Slack attachment bytes")?
            {
                size = size.checked_add(chunk.len() as u64).ok_or_else(|| {
                    SlackError::validation("Slack attachment is larger than 1 GiB")
                })?;
                ensure_attachment_size(Some(size))?;
                file.write_all(&chunk)
                    .await
                    .context("failed to write the Slack attachment cache file")?;
                if let Some(total) = expected_size.filter(|total| *total > 0) {
                    let fraction = 0.05 + 0.9 * (size as f64 / total as f64).min(1.0);
                    progress(DownloadProgressUpdate::new(
                        fraction,
                        "Downloading attachment",
                    ));
                }
            }
            file.flush()
                .await
                .context("failed to flush the Slack attachment cache file")?;
            drop(file);
            tokio::fs::rename(&partial_path, destination)
                .await
                .context("failed to finalize the Slack attachment cache file")?;
            Ok::<_, SlackError>(size)
        }
        .await;

        match result {
            Ok(size) => {
                progress(DownloadProgressUpdate::new(1.0, "Attachment ready"));
                Ok(DownloadedAttachment {
                    path: destination.to_path_buf(),
                    size,
                })
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&partial_path).await;
                Err(error)
            }
        }
    }

    pub async fn post_message(
        &self,
        channel_id: &str,
        text: &str,
        blocks_json: Option<&str>,
        attachments_json: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<SlackMessage> {
        let client_msg_id = next_client_message_id();
        let params = post_message_params(
            channel_id,
            text,
            blocks_json,
            attachments_json,
            thread_ts,
            &client_msg_id,
        );

        let response: PostMessageResponse = self.post_form("chat.postMessage", &params).await?;
        let mut message = response.message;
        message.client_msg_id.get_or_insert(client_msg_id);
        if message.blocks.is_none() {
            if let Some(blocks) = blocks_json
                .filter(|blocks| !blocks.trim().is_empty())
                .and_then(|blocks| serde_json::from_str::<Value>(blocks).ok())
            {
                message.blocks = Some(blocks);
                message.refresh_canonical_content();
            }
        }
        if message.attachments.is_none() {
            if let Some(attachments) = attachments_json
                .filter(|attachments| !attachments.trim().is_empty())
                .and_then(|attachments| {
                    serde_json::from_str::<Vec<SlackAttachment>>(attachments).ok()
                })
            {
                message.attachments = Some(attachments);
                message.refresh_canonical_content();
            }
        }
        Ok(message)
    }

    pub async fn execute_slash_command(
        &self,
        channel_id: &str,
        command: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<SlashCommandResponse> {
        let mut params = execute_slash_command_params(channel_id, command, text, thread_ts);
        params.push(("client_token", browser_action_client_token()));
        self.post_form("chat.command", &params).await
    }

    pub async fn update_message(
        &self,
        channel_id: &str,
        original: &SlackMessage,
        text: &str,
        blocks_json: Option<&str>,
    ) -> Result<SlackMessage> {
        let params = update_message_params(channel_id, &original.ts, text, blocks_json);
        let response: UpdateMessageResponse = self.post_form("chat.update", &params).await?;
        Ok(merge_updated_message(original, text, blocks_json, response))
    }

    pub async fn message_permalink(&self, channel_id: &str, message_ts: &str) -> Result<String> {
        let response: MessagePermalinkResponse = self
            .post_form(
                "chat.getPermalink",
                &[
                    ("channel", channel_id.to_string()),
                    ("message_ts", message_ts.to_string()),
                ],
            )
            .await?;
        response
            .permalink
            .ok_or_else(|| SlackError::validation("Slack did not return a message permalink"))
    }

    pub async fn set_reaction(
        &self,
        channel_id: &str,
        ts: &str,
        name: &str,
        add: bool,
    ) -> Result<()> {
        let method = if add {
            "reactions.add"
        } else {
            "reactions.remove"
        };
        let _: BasicResponse = self
            .post_form(
                method,
                &[
                    ("channel", channel_id.to_string()),
                    ("timestamp", ts.to_string()),
                    ("name", name.to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn set_saved(&self, channel_id: &str, ts: &str, add: bool) -> Result<()> {
        let method = if add { "stars.add" } else { "stars.remove" };
        let _: BasicResponse = self
            .post_form(
                method,
                &[
                    ("channel", channel_id.to_string()),
                    ("timestamp", ts.to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn set_conversation_starred(&self, channel_id: &str, starred: bool) -> Result<()> {
        let channel_id = channel_id.trim();
        if channel_id.is_empty() {
            return Err(SlackError::validation("conversation ID is required"));
        }
        let method = if starred { "stars.add" } else { "stars.remove" };
        let response: Result<BasicResponse> = self
            .post_form(method, &[("channel", channel_id.to_string())])
            .await;
        match response {
            Ok(_) => Ok(()),
            Err(SlackError::Api { code, .. })
                if (starred && code == "already_starred")
                    || (!starred && code == "not_starred") =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub async fn upload_files<F>(
        &self,
        channel_id: &str,
        thread_ts: Option<&str>,
        paths: &[PathBuf],
        blocks_json: Option<&str>,
        progress: F,
    ) -> Result<Vec<SlackFile>>
    where
        F: Fn(UploadProgressUpdate),
    {
        if paths.is_empty() {
            return Err(SlackError::validation("select at least one file"));
        }
        let count = paths.len() as f64;
        let mut pending_files = Vec::with_capacity(paths.len());

        for (index, path) in paths.iter().enumerate() {
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| SlackError::validation("file path has no valid filename"))?
                .to_string();
            let metadata = tokio::fs::metadata(path)
                .await
                .with_context(|| format!("failed to inspect {}", path.display()))?;
            if metadata.len() > MAX_UPLOAD_BYTES {
                return Err(SlackError::validation(format!(
                    "{} is larger than 1 GiB",
                    path.display()
                )));
            }

            let base = index as f64 / count * 0.85;
            let span = 0.85 / count;
            progress(UploadProgressUpdate::new(
                base + span * 0.15,
                &format!("Reading {filename}"),
            ));
            let bytes = tokio::fs::read(path)
                .await
                .with_context(|| format!("failed to read {}", path.display()))?;

            progress(UploadProgressUpdate::new(
                base + span * 0.35,
                "Requesting upload URL",
            ));
            let upload: UploadUrlResponse = self
                .post_form(
                    "files.getUploadURLExternal",
                    &[
                        ("filename", filename.clone()),
                        ("length", bytes.len().to_string()),
                    ],
                )
                .await?;

            progress(UploadProgressUpdate::new(
                base + span * 0.60,
                &format!("Uploading {filename}"),
            ));
            self.ensure_trusted_upload_url(&upload.upload_url)?;
            send_tracked_slack_request(
                "upload_bytes",
                self.http.post(&upload.upload_url).body(bytes),
            )
            .await
            .context("failed to upload file bytes to Slack upload URL")?
            .error_for_status()
            .context("Slack upload URL returned an HTTP error")?;
            pending_files.push(json!({ "id": upload.file_id, "title": filename }));
            progress(UploadProgressUpdate::new(base + span, "File data uploaded"));
        }

        progress(UploadProgressUpdate::new(0.90, "Completing upload"));
        let files = Value::Array(pending_files).to_string();
        let params = complete_upload_params(files, channel_id, thread_ts, blocks_json);
        let complete: CompleteUploadResponse = self
            .post_form("files.completeUploadExternal", &params)
            .await?;

        progress(UploadProgressUpdate::new(1.0, "Upload complete"));
        if complete.files.is_empty() {
            return Err(SlackError::validation(
                "Slack did not return uploaded file metadata",
            ));
        }
        Ok(complete.files)
    }

    fn ensure_trusted_upload_url(&self, url: &str) -> Result<()> {
        #[cfg(test)]
        if let (Ok(api_url), Ok(target_url)) =
            (url::Url::parse(&self.api_base_url), url::Url::parse(url))
        {
            if matches!(api_url.host_str(), Some("127.0.0.1" | "localhost"))
                && target_url.scheme() == api_url.scheme()
                && target_url.host_str() == api_url.host_str()
                && target_url.port() == api_url.port()
                && target_url.username().is_empty()
                && target_url.password().is_none()
            {
                return Ok(());
            }
        }
        ensure_trusted_slack_download_url(url)
    }

    async fn post_form<T>(&self, method: &str, params: &[(&str, String)]) -> Result<T>
    where
        T: for<'de> Deserialize<'de> + SlackResponse,
    {
        let url = format!("{}/{method}", self.api_base_url.trim_end_matches('/'));
        let mut form = params.to_vec();
        if self.browser_cookie_d.is_some() && !form.iter().any(|(key, _)| *key == "token") {
            form.push(("token", self.access_token.clone()));
        }
        let mut retries = 0;

        loop {
            let response = send_tracked_slack_request(
                method,
                self.authenticated_request(Method::POST, &url)
                    .timeout(API_REQUEST_TIMEOUT)
                    .form(&form),
            )
            .await
            .with_context(|| format!("failed to call Slack method {method}"))?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = retry_after_delay(&response);
                if retries >= MAX_RATE_LIMIT_RETRIES {
                    crate::debug::log(
                        "slack",
                        &format!("Slack method {method} rate limited; not retrying automatically"),
                    );
                    return Err(SlackError::RateLimited {
                        method: method.to_string(),
                    });
                }
                retries += 1;
                crate::debug::log(
                    "slack",
                    &format!(
                        "Slack method {method} rate limited; retrying in {}s",
                        retry_after.as_secs()
                    ),
                );
                tokio::time::sleep(retry_after).await;
                continue;
            }

            let response = response
                .error_for_status()
                .with_context(|| format!("Slack method {method} returned an HTTP error"))?
                .json::<T>()
                .await
                .with_context(|| format!("failed to parse Slack method {method} response"))?;

            return response.into_result(method);
        }
    }

    async fn star_items(&self) -> Result<Vec<SavedItem>> {
        let mut cursor: Option<String> = None;
        let mut items = Vec::new();
        loop {
            let mut params = vec![("limit", "200".to_string())];
            if let Some(cursor) = cursor.as_ref() {
                params.push(("cursor", cursor.clone()));
            }
            let response: StarsListResponse = self.post_form("stars.list", &params).await?;
            items.extend(response.items);
            cursor = next_cursor(response.response_metadata);
            if cursor.is_none() {
                return Ok(items);
            }
        }
    }

    async fn post_browser_form<T>(
        &self,
        api_base_url: &str,
        method: &str,
        query_params: &[(&'static str, String)],
        params: &[(&'static str, String)],
    ) -> Result<T>
    where
        T: for<'de> Deserialize<'de> + SlackResponse,
    {
        let (cookie, user_agent) = self.browser_session_headers()?;
        let mut url =
            reqwest::Url::parse(&format!("{}/{method}", api_base_url.trim_end_matches('/')))
                .map_err(|_| SlackError::validation("Slack browser API URL is invalid"))?;
        if !query_params.is_empty() {
            let mut query = url.query_pairs_mut();
            for (key, value) in query_params {
                query.append_pair(key, value);
            }
        }

        let mut form = Form::new().text("token", self.access_token.clone());
        for (key, value) in params {
            form = form.text(*key, value.clone());
        }
        let mut request = self
            .api_http
            .post(url)
            .timeout(API_REQUEST_TIMEOUT)
            .header(COOKIE, cookie)
            .multipart(form);
        if let Some(user_agent) = user_agent {
            request = request.header(USER_AGENT, user_agent);
        }
        let response = send_tracked_slack_request(method, request)
            .await
            .with_context(|| format!("failed to call Slack method {method}"))?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            return Err(SlackError::RateLimited {
                method: method.to_string(),
            });
        }
        let response = response
            .error_for_status()
            .with_context(|| format!("Slack method {method} returned an HTTP error"))?
            .json::<T>()
            .await
            .with_context(|| format!("failed to parse Slack method {method} response"))?;

        response.into_result(method)
    }

    fn ensure_browser_session_credentials(&self) -> Result<()> {
        if self
            .browser_cookie_d
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
            || self.access_token.trim().is_empty()
        {
            return Err(SlackError::validation(
                "browser session credentials are unavailable",
            ));
        }
        Ok(())
    }

    fn browser_session_headers(&self) -> Result<(String, Option<&str>)> {
        self.ensure_browser_session_credentials()?;
        let cookie = self
            .browser_cookie_d
            .as_deref()
            .map(str::trim)
            .filter(|cookie| !cookie.is_empty())
            .ok_or_else(|| SlackError::validation("browser session credentials are unavailable"))?;
        let user_agent = self
            .user_agent
            .as_deref()
            .map(str::trim)
            .filter(|user_agent| !user_agent.is_empty());
        Ok((browser_session_cookie_header(cookie), user_agent))
    }

    fn browser_workspace_api_base_url(&self, workspace_url: &str) -> Result<String> {
        let workspace_api_base_url = validated_workspace_api_base_url(workspace_url)?;
        if self.api_base_url != SLACK_API_BASE_URL {
            return Ok(self.api_base_url.trim_end_matches('/').to_string());
        }
        Ok(workspace_api_base_url)
    }

    fn authenticated_request(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        // Only body-less GETs may use the redirect-following client; anything
        // with a body may carry `token=` and must stay on the same origin.
        let client = if method == Method::GET {
            &self.http
        } else {
            &self.api_http
        };
        let mut request = client.request(method, url).bearer_auth(&self.access_token);

        if let Some(cookie) = self
            .browser_cookie_d
            .as_deref()
            .map(str::trim)
            .filter(|cookie| !cookie.is_empty())
        {
            request = request.header(COOKIE, browser_session_cookie_header(cookie));
        }

        if let Some(user_agent) = self
            .user_agent
            .as_deref()
            .map(str::trim)
            .filter(|user_agent| !user_agent.is_empty())
        {
            request = request.header(USER_AGENT, user_agent);
        }

        request
    }
}

fn validated_workspace_api_base_url(workspace_url: &str) -> Result<String> {
    let workspace_url = workspace_url.trim();
    let mut parsed = reqwest::Url::parse(workspace_url)
        .map_err(|_| SlackError::validation("Slack workspace URL is invalid"))?;
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !matches!(parsed.path(), "" | "/")
        || parsed.port().is_some_and(|port| port != 443)
    {
        return Err(SlackError::validation("Slack workspace URL is not trusted"));
    }
    let host = parsed
        .host_str()
        .map(str::to_ascii_lowercase)
        .filter(|host| is_slack_workspace_host(host))
        .ok_or_else(|| SlackError::validation("Slack workspace URL is not trusted"))?;
    parsed
        .set_port(None)
        .map_err(|_| SlackError::validation("Slack workspace URL is not trusted"))?;
    Ok(format!("https://{host}/api"))
}

fn is_slack_workspace_host(host: &str) -> bool {
    let Some(workspace) = host.strip_suffix(".slack.com") else {
        return false;
    };
    !workspace.is_empty()
        && workspace.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn post_message_params(
    channel_id: &str,
    text: &str,
    blocks_json: Option<&str>,
    attachments_json: Option<&str>,
    thread_ts: Option<&str>,
    client_msg_id: &str,
) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("channel", channel_id.to_string()),
        ("text", text.to_string()),
        ("client_msg_id", client_msg_id.to_string()),
    ];
    if let Some(blocks_json) = blocks_json.filter(|blocks| !blocks.trim().is_empty()) {
        params.push(("blocks", blocks_json.to_string()));
    }
    if let Some(attachments_json) =
        attachments_json.filter(|attachments| !attachments.trim().is_empty())
    {
        params.push(("attachments", attachments_json.to_string()));
    }
    if let Some(thread_ts) = thread_ts.filter(|thread_ts| !thread_ts.trim().is_empty()) {
        params.push(("thread_ts", thread_ts.to_string()));
    }
    params
}

fn execute_slash_command_params(
    channel_id: &str,
    command: &str,
    text: &str,
    thread_ts: Option<&str>,
) -> Vec<(&'static str, String)> {
    let command = if command.starts_with('/') {
        command.to_string()
    } else {
        format!("/{}", command)
    };
    // Mirrors the Slack web client: `disp` is the command as typed.
    let mut params = vec![
        ("channel", channel_id.to_string()),
        ("disp", command.clone()),
        ("command", command),
        ("text", text.to_string()),
        ("_x_reason", "executeCommand".to_string()),
    ];
    if let Some(thread_ts) = thread_ts.filter(|thread_ts| !thread_ts.trim().is_empty()) {
        params.push(("thread_ts", thread_ts.to_string()));
    }
    params
}

fn update_message_params(
    channel_id: &str,
    message_ts: &str,
    text: &str,
    blocks_json: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("channel", channel_id.to_string()),
        ("ts", message_ts.to_string()),
        ("text", text.to_string()),
    ];
    if let Some(blocks_json) = blocks_json.filter(|blocks| !blocks.trim().is_empty()) {
        params.push(("blocks", blocks_json.to_string()));
    }
    params
}

fn merge_updated_message(
    original: &SlackMessage,
    submitted_text: &str,
    submitted_blocks: Option<&str>,
    response: UpdateMessageResponse,
) -> SlackMessage {
    let response_message = response.message.as_ref().and_then(Value::as_object);
    let response_string = |field: &str| {
        response_message
            .and_then(|message| message.get(field))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    };
    let mut message = original.clone();
    message.ts = response
        .ts
        .filter(|ts| !ts.trim().is_empty())
        .unwrap_or_else(|| original.ts.clone());
    message.text = response_string("text")
        .or(response.text)
        .or_else(|| Some(submitted_text.to_string()));
    message.user = response_string("user").or_else(|| original.user.clone());
    message.blocks = response_message
        .and_then(|value| value.get("blocks"))
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| submitted_blocks.and_then(|blocks| serde_json::from_str(blocks).ok()));
    message.edited = response_message
        .and_then(|value| value.get("edited"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .or_else(|| original.edited.clone())
        .or_else(|| {
            Some(SlackMessageEdit {
                user: message.user.clone(),
                ts: None,
            })
        });
    message.refresh_canonical_content();
    message
}

fn complete_upload_params(
    files: String,
    channel_id: &str,
    thread_ts: Option<&str>,
    blocks_json: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut params = vec![("files", files), ("channel_id", channel_id.to_string())];
    if let Some(thread_ts) = thread_ts.filter(|thread_ts| !thread_ts.trim().is_empty()) {
        params.push(("thread_ts", thread_ts.to_string()));
    }
    if let Some(blocks_json) = blocks_json.filter(|blocks| !blocks.trim().is_empty()) {
        params.push(("blocks", blocks_json.to_string()));
    }
    params
}

fn conversation_user_ids_param(user_ids: &[String], maximum: usize) -> Result<String> {
    let mut user_ids = user_ids
        .iter()
        .map(|user_id| user_id.trim())
        .filter(|user_id| !user_id.is_empty())
        .collect::<Vec<_>>();
    user_ids.sort_unstable();
    user_ids.dedup();
    if user_ids.is_empty() {
        return Err(SlackError::validation("select at least one person"));
    }
    if user_ids.len() > maximum {
        return Err(SlackError::validation(format!(
            "select no more than {maximum} people"
        )));
    }
    Ok(user_ids.join(","))
}

fn channel_creation_params(name: &str, is_private: bool) -> Result<Vec<(&'static str, String)>> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > 80
        || !name.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        })
    {
        return Err(SlackError::validation(
            "channel names must use lowercase letters, numbers, hyphens, or underscores",
        ));
    }
    Ok(vec![
        ("name", name.to_string()),
        ("is_private", is_private.to_string()),
    ])
}

fn paginated_list_params(
    cursor: Option<&str>,
    include_channel_types: bool,
) -> Vec<(&'static str, String)> {
    let mut params = Vec::with_capacity(4);
    if include_channel_types {
        params.push(("types", "public_channel,private_channel".to_string()));
        params.push(("exclude_archived", "true".to_string()));
    }
    params.push(("limit", "200".to_string()));
    if let Some(cursor) = cursor.map(str::trim).filter(|cursor| !cursor.is_empty()) {
        params.push(("cursor", cursor.to_string()));
    }
    params
}

fn required_conversation_id(channel_id: &str) -> Result<String> {
    let channel_id = channel_id.trim();
    if channel_id.is_empty() {
        return Err(SlackError::validation("conversation ID is required"));
    }
    Ok(channel_id.to_string())
}

fn conversation_members_params(
    channel_id: &str,
    cursor: Option<&str>,
) -> Result<Vec<(&'static str, String)>> {
    let mut params = vec![
        ("channel", required_conversation_id(channel_id)?),
        ("limit", "200".to_string()),
    ];
    if let Some(cursor) = cursor.map(str::trim).filter(|cursor| !cursor.is_empty()) {
        params.push(("cursor", cursor.to_string()));
    }
    Ok(params)
}

fn conversation_text_params(
    channel_id: &str,
    field: &'static str,
    text: &str,
) -> Result<Vec<(&'static str, String)>> {
    Ok(vec![
        ("channel", required_conversation_id(channel_id)?),
        (field, text.trim().to_string()),
    ])
}

/// Builds a page from a `subscriptions.thread.getView` response. The cursor is
/// the last root's latest activity; `max_ts` is a future watermark, not a cursor.
/// A cursor that does not move ends paging instead of re-serving the same page.
fn followed_threads_page(
    response: ThreadViewResponse,
    requested_cursor: Option<&str>,
) -> FollowedThreadsPage {
    let threads = response
        .threads
        .into_iter()
        .filter_map(|thread| {
            let channel_id = thread.root_msg.get("channel")?.as_str()?.trim().to_string();
            if channel_id.is_empty() {
                return None;
            }
            let root = deserialize_message(thread.root_msg).ok()?;
            Some((channel_id, root))
        })
        .collect::<Vec<_>>();
    let next_cursor = response
        .has_more
        .then(|| threads.last())
        .flatten()
        .map(|(_, root)| root.latest_reply.as_deref().unwrap_or(&root.ts).to_string())
        .filter(|cursor| Some(cursor.as_str()) != requested_cursor);
    FollowedThreadsPage {
        threads,
        next_cursor,
    }
}

fn next_cursor(metadata: Option<ResponseMetadata>) -> Option<String> {
    metadata
        .and_then(|metadata| metadata.next_cursor)
        .map(|cursor| cursor.trim().to_string())
        .filter(|cursor| !cursor.is_empty())
}

fn is_discoverable_conversation(conversation: &SlackConversation) -> bool {
    !conversation.is_archived.unwrap_or(false)
        && (conversation.is_channel.unwrap_or(false)
            || conversation.is_group.unwrap_or(false)
            || conversation.is_private.unwrap_or(false))
        && !conversation.is_im.unwrap_or(false)
        && !conversation.is_mpim.unwrap_or(false)
}

fn retry_after_delay(response: &reqwest::Response) -> Duration {
    let seconds = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(retry_after_seconds)
        .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS);

    Duration::from_secs(seconds)
}

fn retry_after_seconds(value: &str) -> u64 {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS)
        .min(MAX_RETRY_AFTER_SECONDS)
}

fn history_request_params(
    channel_id: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("channel", channel_id.to_string()),
        ("limit", limit.to_string()),
    ];
    if let Some(cursor) = cursor.filter(|cursor| !cursor.trim().is_empty()) {
        params.push(("cursor", cursor.to_string()));
    }
    params
}

fn message_context_request_params(
    channel_id: &str,
    message_ts: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("channel", channel_id.to_string()),
        ("latest", message_ts.to_string()),
        ("inclusive", "true".to_string()),
        ("limit", MESSAGE_CONTEXT_LIMIT.to_string()),
    ]
}

/// Joins thread pages into one newest-first page without duplicate parents,
/// keeping the last page's continuation.
fn merge_thread_pages(pages: Vec<SlackMessagePage>) -> SlackMessagePage {
    let (has_more, next_cursor) = pages
        .last()
        .map(|page| (page.has_more, page.next_cursor.clone()))
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    let mut messages = pages
        .into_iter()
        .flat_map(|page| page.messages)
        .filter(|message| seen.insert(message.ts.clone()))
        .collect::<Vec<_>>();
    messages.sort_by(|left, right| {
        if crate::models::slack_timestamp_is_after(&left.ts, &right.ts) {
            std::cmp::Ordering::Less
        } else if crate::models::slack_timestamp_is_after(&right.ts, &left.ts) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    SlackMessagePage {
        messages,
        has_more,
        next_cursor,
    }
}

/// Followed thread roots keyed by channel, newest activity first.
#[derive(Debug, Clone, Default)]
pub struct FollowedThreadsPage {
    pub threads: Vec<(String, SlackMessage)>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SlackMessagePage {
    pub messages: Vec<SlackMessage>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

impl SlackMessagePage {
    fn from_response(
        response: HistoryResponse,
        normalize_messages: impl FnOnce(Vec<SlackMessage>) -> Vec<SlackMessage>,
    ) -> Self {
        let next_cursor = response
            .response_metadata
            .and_then(|metadata| metadata.next_cursor)
            .and_then(|cursor| {
                let cursor = cursor.trim().to_string();
                (!cursor.is_empty()).then_some(cursor)
            });
        let has_more = response.has_more.unwrap_or(false) || next_cursor.is_some();

        Self {
            messages: normalize_messages(response.messages),
            has_more,
            next_cursor,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum PreviewAssetMime {
    Png,
    Jpeg,
    Gif,
    Webp,
    Avif,
    Mp4,
    Webm,
    Quicktime,
    Ogg,
}

impl PreviewAssetMime {
    pub(crate) const ALL: [Self; 9] = [
        Self::Png,
        Self::Jpeg,
        Self::Gif,
        Self::Webp,
        Self::Avif,
        Self::Mp4,
        Self::Webm,
        Self::Quicktime,
        Self::Ogg,
    ];

    pub(crate) fn parse(content_type: &str) -> Option<Self> {
        let normalized = content_type.split(';').next()?.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            "image/avif" => Some(Self::Avif),
            "video/mp4" => Some(Self::Mp4),
            "video/webm" => Some(Self::Webm),
            "video/quicktime" => Some(Self::Quicktime),
            "video/ogg" => Some(Self::Ogg),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Avif => "image/avif",
            Self::Mp4 => "video/mp4",
            Self::Webm => "video/webm",
            Self::Quicktime => "video/quicktime",
            Self::Ogg => "video/ogg",
        }
    }

    pub(crate) const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Mp4 => "mp4",
            Self::Webm => "webm",
            Self::Quicktime => "mov",
            Self::Ogg => "ogv",
        }
    }

    pub(crate) const fn is_video(self) -> bool {
        matches!(self, Self::Mp4 | Self::Webm | Self::Quicktime | Self::Ogg)
    }

    pub(crate) const fn max_bytes(self) -> usize {
        if self.is_video() {
            MAX_PREVIEW_VIDEO_BYTES
        } else {
            MAX_PREVIEW_IMAGE_BYTES
        }
    }

    pub(crate) const fn validate_size(self, size: usize) -> bool {
        size > 0 && size <= self.max_bytes()
    }

    pub(crate) fn validate_bytes(self, bytes: &[u8]) -> bool {
        self.validate_size(bytes.len()) && self.validate_signature(bytes)
    }

    pub(crate) fn validate_cached_content(self, size: u64, prefix: &[u8]) -> bool {
        usize::try_from(size)
            .ok()
            .is_some_and(|size| self.validate_size(size))
            && self.validate_signature(prefix)
    }

    pub(crate) fn is_valid_payload(self, bytes: &[u8]) -> bool {
        self.validate_bytes(bytes)
    }

    fn validate_signature(self, bytes: &[u8]) -> bool {
        match self {
            Self::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => bytes.starts_with(b"\xff\xd8\xff"),
            Self::Gif => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
            Self::Webp => {
                bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP"
            }
            Self::Avif => iso_base_media_has_brand(bytes, is_avif_brand),
            Self::Mp4 => iso_base_media_has_brand(bytes, is_mp4_brand),
            Self::Quicktime => iso_base_media_has_brand(bytes, is_quicktime_brand),
            Self::Webm => bytes.starts_with(b"\x1a\x45\xdf\xa3"),
            Self::Ogg => bytes.starts_with(b"OggS"),
        }
    }
}

impl std::fmt::Display for PreviewAssetMime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct DownloadedPreviewAsset {
    pub(crate) mime_type: PreviewAssetMime,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DownloadedMedia {
    pub path: PathBuf,
    pub mime_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DownloadedAttachment {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DownloadProgressUpdate {
    pub fraction: f64,
    pub label: String,
}

impl DownloadProgressUpdate {
    fn new(fraction: f64, label: &str) -> Self {
        Self {
            fraction,
            label: label.to_string(),
        }
    }
}

fn is_trusted_slack_download_url(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    url.host_str().is_some_and(|host| {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        host == "slack.com" || host.ends_with(".slack.com")
    })
}

fn is_trusted_avatar_url(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    url.host_str().is_some_and(|host| {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        host == "slack-edge.com"
            || host.ends_with(".slack-edge.com")
            || host == "secure.gravatar.com"
    })
}

/// The single allowlist for images the timeline fetches on its own:
/// message documents (image blocks, context icons, unfurls), attachments and
/// avatars. Slack hosts plus the GIF services Slack itself integrates with
/// (`/giphy`, the GIF picker). Arbitrary user-pasted hosts are never fetched.
pub(crate) fn supports_native_preview_asset_url(url: &str) -> bool {
    is_trusted_slack_download_url(url)
        || is_trusted_avatar_url(url)
        || is_trusted_gif_service_url(url)
        || is_slack_image_proxy_url(url)
}

/// Slack's public image proxy (exact host, https only). It serves any image
/// anonymously, so requests to it never carry Slack credentials.
fn is_slack_image_proxy_url(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| {
            host.trim_end_matches('.')
                .eq_ignore_ascii_case("slack-imgs.com")
        })
}

/// Only Slack-owned hosts get the workspace credentials; everything else
/// the preview downloader reaches (avatars, GIF services, the image proxy)
/// is fetched anonymously.
fn preview_download_needs_auth(url: &str) -> bool {
    is_trusted_slack_download_url(url)
}

fn is_trusted_gif_service_url(url: &str) -> bool {
    const GIF_SERVICE_HOSTS: [&str; 2] = ["giphy.com", "tenor.com"];
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    url.host_str().is_some_and(|host| {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        GIF_SERVICE_HOSTS.iter().any(|trusted| {
            host == *trusted
                || host
                    .strip_suffix(trusted)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
    })
}

fn ensure_trusted_slack_download_url(url: &str) -> Result<()> {
    if !is_trusted_slack_download_url(url) {
        return Err(SlackError::validation(
            "download URL is not a trusted Slack URL",
        ));
    }
    Ok(())
}

fn append_bounded_preview_chunk(bytes: &mut Vec<u8>, chunk: &[u8], max_bytes: usize) -> Result<()> {
    let next_size = bytes
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| SlackError::validation("Slack attachment preview is too large"))?;
    if next_size > max_bytes {
        return Err(SlackError::validation(
            "Slack attachment preview is too large",
        ));
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

fn supported_media_mime_type(content_type: &str) -> Option<&str> {
    let mime_type = content_type.split(';').next()?.trim();
    (mime_type.starts_with("image/")
        || matches!(
            mime_type,
            "video/mp4" | "video/webm" | "video/quicktime" | "video/x-matroska" | "video/ogg"
        ))
    .then_some(mime_type)
}

#[cfg(test)]
pub(crate) fn supported_preview_mime_type(mime_type: &str) -> bool {
    PreviewAssetMime::parse(mime_type).is_some()
}

fn is_avif_brand(brand: &[u8]) -> bool {
    matches!(brand, b"avif" | b"avis")
}

fn is_mp4_brand(brand: &[u8]) -> bool {
    matches!(
        brand,
        b"isom"
            | b"iso2"
            | b"iso3"
            | b"iso4"
            | b"iso5"
            | b"iso6"
            | b"iso7"
            | b"iso8"
            | b"iso9"
            | b"mp41"
            | b"mp42"
            | b"mp71"
            | b"avc1"
            | b"hvc1"
            | b"hev1"
            | b"dash"
            | b"M4V "
            | b"M4VH"
            | b"M4VP"
            | b"F4V "
            | b"F4P "
            | b"MSNV"
    ) || brand.starts_with(b"3gp")
        || brand.starts_with(b"3g2")
}

fn is_quicktime_brand(brand: &[u8]) -> bool {
    brand == b"qt  "
}

fn iso_base_media_has_brand(bytes: &[u8], mut matches_brand: impl FnMut(&[u8]) -> bool) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    if matches_brand(&bytes[8..12]) {
        return true;
    }
    let compatible_end = bytes.len().min(64);
    bytes
        .get(16..compatible_end)
        .is_some_and(|brands| brands.chunks_exact(4).any(matches_brand))
}

fn ensure_media_size(size: Option<u64>) -> Result<()> {
    if size.is_some_and(|size| size > MAX_MEDIA_DOWNLOAD_BYTES) {
        return Err(SlackError::validation("Slack media is larger than 1 GiB"));
    }
    Ok(())
}

fn ensure_attachment_size(size: Option<u64>) -> Result<()> {
    if size.is_some_and(|size| size > MAX_MEDIA_DOWNLOAD_BYTES) {
        return Err(SlackError::validation(
            "Slack attachment is larger than 1 GiB",
        ));
    }
    Ok(())
}

fn partial_download_path(destination: &Path) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.part", std::process::id()));
    destination.with_file_name(name)
}

#[derive(Debug, Clone)]
pub struct UploadProgressUpdate {
    pub fraction: f64,
    pub label: String,
}

impl UploadProgressUpdate {
    fn new(fraction: f64, label: &str) -> Self {
        Self {
            fraction,
            label: label.to_string(),
        }
    }
}

trait SlackResponse: Sized {
    fn ok(&self) -> bool;
    fn error(&self) -> Option<&str>;

    fn into_result(self, method: &str) -> Result<Self> {
        if self.ok() {
            Ok(self)
        } else {
            Err(SlackError::api(
                method,
                self.error().unwrap_or("unknown_error"),
            ))
        }
    }
}

macro_rules! impl_slack_response {
    ($type_name:ty) => {
        impl SlackResponse for $type_name {
            fn ok(&self) -> bool {
                self.ok
            }

            fn error(&self) -> Option<&str> {
                self.error.as_deref()
            }
        }
    };
}

#[derive(Debug, Deserialize)]
struct ResponseMetadata {
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthTestResponse {
    ok: bool,
    error: Option<String>,
    url: Option<String>,
    team: Option<String>,
    user: Option<String>,
    team_id: Option<String>,
    user_id: Option<String>,
}
impl_slack_response!(AuthTestResponse);

#[derive(Debug, Deserialize)]
struct TeamInfoResponse {
    ok: bool,
    error: Option<String>,
    team: Option<TeamInfo>,
}
impl_slack_response!(TeamInfoResponse);

#[derive(Debug, Deserialize)]
struct TeamInfo {
    icon: Option<TeamIcon>,
}

#[derive(Debug, Deserialize)]
struct TeamIcon {
    image_68: Option<String>,
    image_88: Option<String>,
    image_132: Option<String>,
    image_default: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ClientUserBootResponse {
    ok: bool,
    error: Option<String>,
    #[serde(rename = "self")]
    self_data: Option<ClientUserBootSelf>,
}
impl_slack_response!(ClientUserBootResponse);

#[derive(Debug, Deserialize)]
struct ClientUserBootSelf {
    prefs: Option<ClientUserBootPrefs>,
}

#[derive(Debug, Deserialize)]
struct ClientUserBootPrefs {
    ia_theme: Option<Value>,
    sidebar_theme: Option<Value>,
    sidebar_theme_custom_values: Option<Value>,
    sidebar_theme_custom: Option<Value>,
}

impl ClientUserBootPrefs {
    /// Current `ia_theme` first, then the legacy sidebar theme prefs.
    fn sidebar_theme(&self) -> Option<SidebarTheme> {
        self.ia_theme.as_ref().and_then(parse_ia_theme).or_else(|| {
            [
                &self.sidebar_theme_custom_values,
                &self.sidebar_theme_custom,
                &self.sidebar_theme,
            ]
            .into_iter()
            .flatten()
            .find_map(parse_sidebar_theme_value)
        })
    }
}

#[derive(Debug, Deserialize)]
struct UsersPrefsGetResponse {
    ok: bool,
    error: Option<String>,
    prefs: Option<ClientUserBootPrefs>,
}
impl_slack_response!(UsersPrefsGetResponse);

#[derive(Debug, Deserialize)]
struct MessageActionResponse {
    ok: bool,
    error: Option<String>,
}
impl_slack_response!(MessageActionResponse);

fn log_conversation_properties(method: &str, conversations: &[SlackConversation]) {
    if !crate::debug::enabled() {
        return;
    }

    crate::debug::log(
        "slack",
        &format!("{method} returned {} conversations", conversations.len()),
    );

    let log_limit = conversation_property_log_limit(
        std::env::var(DEBUG_CONVERSATION_PROPERTIES_ENV)
            .ok()
            .as_deref(),
        conversations.len(),
    );
    if log_limit == 0 {
        crate::debug::log(
            "slack",
            &format!(
                "conversation property logging disabled; set {DEBUG_CONVERSATION_PROPERTIES_ENV}=20 or all to enable"
            ),
        );
        return;
    }

    for conversation in conversations.iter().take(log_limit) {
        let properties = serde_json::to_string_pretty(conversation)
            .unwrap_or_else(|_| format!("{conversation:#?}"));
        crate::debug::log(
            "slack",
            &format!(
                "conversation id={} type={} title={} properties=\n{}",
                conversation.id,
                conversation_debug_kind(conversation),
                conversation.display_name(),
                properties
            ),
        );
    }

    if conversations.len() > log_limit {
        crate::debug::log(
            "slack",
            &format!(
                "conversation property logging truncated at {log_limit}/{}; set {DEBUG_CONVERSATION_PROPERTIES_ENV}=all to log every conversation",
                conversations.len()
            ),
        );
    }
}

fn conversation_property_log_limit(setting: Option<&str>, total: usize) -> usize {
    let Some(setting) = setting.map(str::trim).filter(|setting| !setting.is_empty()) else {
        return 0;
    };

    if setting.eq_ignore_ascii_case("all") {
        return total;
    }

    if setting.eq_ignore_ascii_case("true") || setting == "1" {
        return DEFAULT_DEBUG_CONVERSATION_PROPERTY_LIMIT.min(total);
    }

    setting.parse::<usize>().unwrap_or_default().min(total)
}

fn conversation_debug_kind(conversation: &SlackConversation) -> &'static str {
    if conversation.is_im.unwrap_or(false) {
        "direct_message"
    } else if conversation.is_mpim.unwrap_or(false) {
        "group_direct_message"
    } else if conversation.is_private.unwrap_or(false) || conversation.is_group.unwrap_or(false) {
        "private_channel"
    } else if conversation.is_channel.unwrap_or(false) {
        "public_channel"
    } else {
        "unknown"
    }
}

#[derive(Debug, Deserialize)]
struct ConversationListResponse {
    ok: bool,
    error: Option<String>,
    channels: Vec<SlackConversation>,
    response_metadata: Option<ResponseMetadata>,
}
impl_slack_response!(ConversationListResponse);

#[derive(Debug, Deserialize)]
struct ConversationJoinResponse {
    ok: bool,
    error: Option<String>,
    channel: SlackConversation,
}
impl_slack_response!(ConversationJoinResponse);

#[derive(Debug, Deserialize)]
struct ConversationMembersResponse {
    ok: bool,
    error: Option<String>,
    #[serde(default)]
    members: Vec<String>,
    response_metadata: Option<ResponseMetadata>,
}
impl_slack_response!(ConversationMembersResponse);

#[derive(Debug, Deserialize)]
struct ConversationOpenResponse {
    ok: bool,
    error: Option<String>,
    channel: SlackConversation,
}
impl_slack_response!(ConversationOpenResponse);

#[derive(Debug, Deserialize)]
struct UsersListResponse {
    ok: bool,
    error: Option<String>,
    members: Vec<SlackUser>,
    response_metadata: Option<ResponseMetadata>,
}
impl_slack_response!(UsersListResponse);

#[derive(Debug, Deserialize)]
struct HistoryResponse {
    ok: bool,
    error: Option<String>,
    #[serde(deserialize_with = "deserialize_messages")]
    messages: Vec<SlackMessage>,
    has_more: Option<bool>,
    response_metadata: Option<ResponseMetadata>,
}
impl_slack_response!(HistoryResponse);

#[derive(Debug, Deserialize)]
struct SearchMessages {
    matches: Vec<SearchMatch>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    ok: bool,
    error: Option<String>,
    messages: SearchMessages,
}
impl_slack_response!(SearchResponse);

#[derive(Debug, Deserialize)]
struct StarsListResponse {
    ok: bool,
    error: Option<String>,
    #[serde(default)]
    items: Vec<SavedItem>,
    response_metadata: Option<ResponseMetadata>,
}
impl_slack_response!(StarsListResponse);

#[derive(Debug, Deserialize)]
struct ThreadViewResponse {
    ok: bool,
    error: Option<String>,
    #[serde(default)]
    threads: Vec<ThreadViewEntry>,
    #[serde(default)]
    has_more: bool,
}
impl_slack_response!(ThreadViewResponse);

#[derive(Debug, Deserialize)]
struct ThreadViewEntry {
    root_msg: Value,
}

#[derive(Debug, Deserialize)]
struct FilesListResponse {
    ok: bool,
    error: Option<String>,
    files: Vec<SlackFile>,
}
impl_slack_response!(FilesListResponse);

#[derive(Debug, Deserialize)]
struct FileInfoResponse {
    ok: bool,
    error: Option<String>,
    file: SlackFile,
}
impl_slack_response!(FileInfoResponse);

#[derive(Debug, Deserialize)]
struct UserInfoResponse {
    ok: bool,
    error: Option<String>,
    user: SlackUser,
}
impl_slack_response!(UserInfoResponse);

#[derive(Debug, Deserialize)]
struct UserProfileResponse {
    ok: bool,
    error: Option<String>,
    profile: Option<SlackUserProfile>,
}
impl_slack_response!(UserProfileResponse);

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct UserGroupsListResponse {
    ok: bool,
    error: Option<String>,
    usergroups: Vec<SlackUserGroup>,
}
impl_slack_response!(UserGroupsListResponse);

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct EmojiListResponse {
    ok: bool,
    error: Option<String>,
    #[serde(default)]
    emoji: HashMap<String, String>,
}
impl_slack_response!(EmojiListResponse);

#[derive(Debug, Deserialize)]
struct PostMessageResponse {
    ok: bool,
    error: Option<String>,
    #[serde(deserialize_with = "deserialize_message")]
    message: SlackMessage,
}
impl_slack_response!(PostMessageResponse);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SlashCommandResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub response: Option<String>,
    pub command: Option<String>,
    pub channel: Option<String>,
}
impl_slack_response!(SlashCommandResponse);

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct UpdateMessageResponse {
    ok: bool,
    error: Option<String>,
    channel: Option<String>,
    ts: Option<String>,
    text: Option<String>,
    message: Option<Value>,
}
impl_slack_response!(UpdateMessageResponse);

#[derive(Debug, Deserialize)]
struct MessagePermalinkResponse {
    ok: bool,
    error: Option<String>,
    permalink: Option<String>,
}
impl_slack_response!(MessagePermalinkResponse);

#[derive(Debug, Deserialize)]
struct ClientCountsResponse {
    ok: bool,
    error: Option<String>,
    #[serde(flatten)]
    payload: ClientCountsPayload,
}
impl_slack_response!(ClientCountsResponse);

#[derive(Debug, Deserialize)]
struct BasicResponse {
    ok: bool,
    error: Option<String>,
}
impl_slack_response!(BasicResponse);

#[derive(Debug, Deserialize)]
struct UploadUrlResponse {
    ok: bool,
    error: Option<String>,
    upload_url: String,
    file_id: String,
}
impl_slack_response!(UploadUrlResponse);

#[derive(Debug, Deserialize)]
struct CompleteUploadResponse {
    ok: bool,
    error: Option<String>,
    files: Vec<SlackFile>,
}
impl_slack_response!(CompleteUploadResponse);

fn thread_replies_in_history_order(mut messages: Vec<SlackMessage>) -> Vec<SlackMessage> {
    // Slack conversations.replies returns the parent first, while conversations.history
    // returns newest-first. Keep these API methods consistent for the message renderer.
    messages.reverse();
    messages
}

#[cfg(test)]
mod tests;
