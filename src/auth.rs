use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use keyring::Entry;
use rand::random;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tiny_http::{Header, Response, Server, StatusCode};
use url::Url;

use crate::{config, models::StoredToken};

const KEYRING_SERVICE: &str = "eu.vanadrighem.conduit";
const KEYRING_USER: &str = "slack-user-token";
const KEYRING_APP_TOKEN_USER: &str = "slack-app-token";
const OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const OAUTH_CALLBACK_POLL_INTERVAL: Duration = Duration::from_millis(100);
const OAUTH_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const OAUTH_HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub const DEFAULT_REDIRECT_PORT: u16 = 8934;
pub const DEFAULT_USER_SCOPES: &[&str] = &[
    "channels:read",
    "channels:history",
    "channels:join",
    "channels:write",
    "groups:read",
    "groups:history",
    "groups:write",
    "im:read",
    "im:history",
    "im:write",
    "mpim:read",
    "mpim:history",
    "mpim:write",
    "users:read",
    "users:read.email",
    "users.profile:read",
    "usergroups:read",
    "emoji:read",
    "chat:write",
    "search:read",
    "stars:read",
    "stars:write",
    "reactions:read",
    "reactions:write",
    "files:read",
    "files:write",
];
const DEFAULT_BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    pub redirect_port: u16,
    pub user_scopes: Vec<String>,
}

impl OAuthConfig {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            redirect_port: DEFAULT_REDIRECT_PORT,
            user_scopes: DEFAULT_USER_SCOPES
                .iter()
                .map(|scope| scope.to_string())
                .collect(),
        }
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.redirect_port)
    }
}

#[derive(Debug, Clone)]
pub struct TokenStore;

impl TokenStore {
    pub fn load(&self) -> Result<Option<StoredToken>> {
        let entry = Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
        match entry.get_password() {
            Ok(serialized) => {
                let token =
                    serde_json::from_str(&serialized).context("stored Slack token is invalid")?;
                Ok(Some(token))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("failed to read Slack token from keyring"),
        }
    }

    pub fn save(&self, token: &StoredToken) -> Result<()> {
        let entry = Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
        let serialized = serde_json::to_string(token)?;
        entry
            .set_password(&serialized)
            .context("failed to save Slack token to keyring")
    }

    pub fn clear(&self) -> Result<()> {
        let entry = Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("failed to delete Slack token from keyring"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppTokenStore;

impl AppTokenStore {
    pub fn load(&self) -> Result<Option<String>> {
        let entry = Entry::new(KEYRING_SERVICE, KEYRING_APP_TOKEN_USER)?;
        match entry.get_password() {
            Ok(token) => normalize_app_token(&token).map(Some),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("failed to read Slack app token from keyring"),
        }
    }

    pub fn save(&self, token: &str) -> Result<()> {
        let token = normalize_app_token(token)?;
        let entry = Entry::new(KEYRING_SERVICE, KEYRING_APP_TOKEN_USER)?;
        entry
            .set_password(&token)
            .context("failed to save Slack app token to keyring")
    }
}

pub fn configured_app_token() -> Result<Option<String>> {
    match config::slack_app_token() {
        Some(token) => normalize_app_token(&token).map(Some),
        None => AppTokenStore.load(),
    }
}

fn normalize_app_token(token: &str) -> Result<String> {
    let token = token.trim();
    if !token.starts_with("xapp-") || token.len() <= "xapp-".len() {
        return Err(anyhow!("Enter a Slack app token beginning with xapp-"));
    }
    Ok(token.to_string())
}

#[derive(Debug, Clone)]
pub struct SlackOAuthClient {
    http: Client,
}

impl SlackOAuthClient {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .connect_timeout(OAUTH_HTTP_CONNECT_TIMEOUT)
                .timeout(OAUTH_HTTP_REQUEST_TIMEOUT)
                .build()
                .expect("valid OAuth HTTP client configuration"),
        }
    }

    pub async fn authenticate(&self, config: OAuthConfig, debug: bool) -> Result<StoredToken> {
        let client_id = config.client_id.trim().to_string();
        if client_id.is_empty() {
            return Err(anyhow!("Slack client ID is required"));
        }

        let pkce = PkcePair::generate();
        let state = random_urlsafe(32);
        let authorize_url = build_authorize_url(&config, &pkce.challenge, &state)?;
        let redirect_uri = config.redirect_uri();
        auth_debug(debug, &format!("client_id={client_id}"));
        auth_debug(debug, &format!("redirect_uri={redirect_uri}"));
        auth_debug(debug, &format!("scopes={}", config.user_scopes.join(",")));
        auth_debug(debug, &format!("authorize_url={authorize_url}"));
        let callback =
            wait_for_oauth_callback(config.redirect_port, authorize_url, state.clone(), debug)
                .await?;

        if callback.state.as_deref() != Some(state.as_str()) {
            auth_debug(debug, "callback state mismatch");
            return Err(anyhow!("Slack authorization state did not match"));
        }

        if let Some(error) = callback.error {
            auth_debug(debug, &format!("Slack returned authorize error={error}"));
            return Err(anyhow!("Slack authorization failed: {error}"));
        }

        let code = callback
            .code
            .ok_or_else(|| anyhow!("Slack authorization did not return a code"))?;

        exchange_user_code(
            &self.http,
            &client_id,
            &redirect_uri,
            &pkce.verifier,
            &code,
            debug,
        )
        .await
    }

    pub async fn refresh(&self, token: &StoredToken) -> Result<StoredToken> {
        let client_id = token
            .client_id
            .as_deref()
            .filter(|client_id| !client_id.trim().is_empty())
            .ok_or_else(|| anyhow!("stored Slack token cannot be refreshed without a client ID"))?;
        let refresh_token = token
            .refresh_token
            .as_deref()
            .ok_or_else(|| anyhow!("stored Slack token does not include a refresh token"))?;

        refresh_user_token(&self.http, client_id, refresh_token, token).await
    }
}

pub fn browser_session_token_from_env() -> Result<Option<StoredToken>> {
    browser_session_token_from_values(
        config::slack_xoxc_token(),
        config::slack_xoxd_token(),
        config::slack_user_agent(),
    )
}

#[derive(Debug, Clone)]
struct PkcePair {
    verifier: String,
    challenge: String,
}

impl PkcePair {
    fn generate() -> Self {
        let verifier = random_urlsafe(64);
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Self {
            verifier,
            challenge,
        }
    }
}

#[derive(Debug)]
struct OAuthCallback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    ok: bool,
    access_token: Option<String>,
    token_type: Option<String>,
    scope: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    authed_user: Option<AuthedUser>,
    team: Option<TokenTeam>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthedUser {
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenTeam {
    id: Option<String>,
    name: Option<String>,
}

fn random_urlsafe(size: usize) -> String {
    let mut bytes = vec![0u8; size];
    for byte in &mut bytes {
        *byte = random::<u8>();
    }
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn browser_session_token_from_values(
    xoxc_token: Option<String>,
    xoxd_token: Option<String>,
    user_agent: Option<String>,
) -> Result<Option<StoredToken>> {
    let xoxc_token = trimmed_value(xoxc_token);
    let xoxd_token = trimmed_value(xoxd_token);
    let user_agent =
        trimmed_value(user_agent).or_else(|| Some(DEFAULT_BROWSER_USER_AGENT.to_string()));

    match (xoxc_token, xoxd_token) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(anyhow!(
            "both XOXC and XOXD tokens are required for Slack browser-session authentication"
        )),
        (Some(access_token), Some(browser_cookie_d)) => {
            if !access_token.starts_with("xoxc-") {
                return Err(anyhow!("XOXC token must start with xoxc-"));
            }
            if !browser_cookie_d.starts_with("xoxd-") {
                return Err(anyhow!("XOXD token must start with xoxd-"));
            }

            Ok(Some(StoredToken {
                access_token,
                token_type: Some("browser_session".to_string()),
                scope: None,
                refresh_token: None,
                expires_in: None,
                expires_at: None,
                team_id: None,
                team_name: None,
                user_id: None,
                client_id: None,
                browser_cookie_d: Some(browser_cookie_d),
                user_agent,
            }))
        }
    }
}

fn trimmed_value(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn build_authorize_url(config: &OAuthConfig, challenge: &str, state: &str) -> Result<String> {
    let mut url = Url::parse("https://slack.com/oauth/v2_user/authorize")?;
    url.query_pairs_mut()
        .append_pair("client_id", config.client_id.trim())
        .append_pair("scope", &config.user_scopes.join(","))
        .append_pair("redirect_uri", &config.redirect_uri())
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url.to_string())
}

async fn wait_for_oauth_callback(
    redirect_port: u16,
    authorize_url: String,
    expected_state: String,
    debug: bool,
) -> Result<OAuthCallback> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut cancellation_guard = CancelBlockingCallbackOnDrop::new(Arc::clone(&cancelled));
    let callback = tokio::task::spawn_blocking(move || {
        let addr = SocketAddr::from(([127, 0, 0, 1], redirect_port));
        auth_debug(debug, &format!("binding local callback server on {addr}"));
        let server = Server::http(addr)
            .map_err(|error| anyhow!("failed to start local OAuth callback server: {error}"))?;

        auth_debug(debug, "opening authorization URL in default browser");
        open::that_detached(&authorize_url).context("failed to open Slack authorization URL")?;

        receive_oauth_callback(&server, &expected_state, &cancelled, debug)
    })
    .await??;
    cancellation_guard.disarm();
    Ok(callback)
}

struct CancelBlockingCallbackOnDrop {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl CancelBlockingCallbackOnDrop {
    fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelBlockingCallbackOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

fn receive_oauth_callback(
    server: &Server,
    expected_state: &str,
    cancelled: &AtomicBool,
    debug: bool,
) -> Result<OAuthCallback> {
    auth_debug(debug, "waiting up to 300 seconds for Slack callback");
    let deadline = Instant::now() + OAUTH_CALLBACK_TIMEOUT;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(anyhow!("Slack authorization was cancelled"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!("Slack authorization timed out"));
        }
        let Some(request) = server
            .recv_timeout(remaining.min(OAUTH_CALLBACK_POLL_INTERVAL))
            .context("failed to receive OAuth callback")?
        else {
            continue;
        };
        let callback_url = Url::parse(&format!("http://127.0.0.1{}", request.url()))
            .context("failed to parse OAuth callback URL")?;
        let params: HashMap<String, String> = callback_url.query_pairs().into_owned().collect();

        let state_ok = params.get("state").map(String::as_str) == Some(expected_state);
        let terminal = callback_url.path() == "/callback"
            && state_ok
            && (params.contains_key("code") || params.contains_key("error"));
        auth_debug(
            debug,
            &format!(
                "callback path={} state_ok={} error={} code_present={}",
                callback_url.path(),
                state_ok,
                params.get("error").map(String::as_str).unwrap_or("<none>"),
                params.contains_key("code")
            ),
        );
        let success = terminal && params.contains_key("code") && !params.contains_key("error");
        respond_to_oauth_request(request, success);
        if !terminal {
            continue;
        }

        return Ok(OAuthCallback {
            code: params.get("code").cloned(),
            state: params.get("state").cloned(),
            error: params.get("error").cloned(),
        });
    }
}

fn respond_to_oauth_request(request: tiny_http::Request, success: bool) {
    let page = callback_page(success);
    let mut response = Response::from_string(page).with_status_code(if success {
        StatusCode(200)
    } else {
        StatusCode(400)
    });
    if let Ok(header) = Header::from_bytes("Content-Type", "text/html; charset=utf-8") {
        response = response.with_header(header);
    }
    let _ = request.respond(response);
}

fn callback_page(success: bool) -> &'static str {
    if success {
        r#"<!doctype html><meta charset="utf-8"><title>Conduit connected</title><body style="font:16px system-ui,sans-serif;max-width:40rem;margin:4rem auto;line-height:1.5"><h1>Conduit is connected</h1><p>You can close this browser tab and return to Conduit.</p></body>"#
    } else {
        r#"<!doctype html><meta charset="utf-8"><title>Conduit authorization failed</title><body style="font:16px system-ui,sans-serif;max-width:40rem;margin:4rem auto;line-height:1.5"><h1>Conduit could not connect</h1><p>Return to Conduit for details.</p></body>"#
    }
}

async fn exchange_user_code(
    http: &Client,
    client_id: &str,
    redirect_uri: &str,
    code_verifier: &str,
    code: &str,
    debug: bool,
) -> Result<StoredToken> {
    auth_debug(
        debug,
        "exchanging authorization code with oauth.v2.user.access",
    );
    let response = oauth_user_access(
        http,
        &[
            ("client_id", client_id),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", code_verifier),
            ("grant_type", "authorization_code"),
        ],
    )
    .await?;

    let token = token_from_response(response, Some(client_id), None)?;
    auth_debug(
        debug,
        &format!(
            "token exchange succeeded team_id={} user_id={} scope={}",
            token.team_id.as_deref().unwrap_or("<unknown>"),
            token.user_id.as_deref().unwrap_or("<unknown>"),
            token.scope.as_deref().unwrap_or("<unknown>")
        ),
    );
    Ok(token)
}

async fn refresh_user_token(
    http: &Client,
    client_id: &str,
    refresh_token: &str,
    previous: &StoredToken,
) -> Result<StoredToken> {
    let response = oauth_user_access(
        http,
        &[
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ],
    )
    .await?;

    token_from_response(response, Some(client_id), Some(previous))
}

async fn oauth_user_access(http: &Client, params: &[(&str, &str)]) -> Result<OAuthTokenResponse> {
    let response = http
        .post("https://slack.com/api/oauth.v2.user.access")
        .form(params)
        .send()
        .await
        .context("failed to exchange Slack OAuth code")?
        .error_for_status()
        .context("Slack OAuth token endpoint returned an HTTP error")?
        .json::<OAuthTokenResponse>()
        .await
        .context("failed to parse Slack OAuth token response")?;

    if !response.ok {
        return Err(anyhow!(
            "Slack OAuth token exchange failed: {}",
            response
                .error
                .unwrap_or_else(|| "unknown_error".to_string())
        ));
    }

    Ok(response)
}

fn token_from_response(
    response: OAuthTokenResponse,
    client_id: Option<&str>,
    previous: Option<&StoredToken>,
) -> Result<StoredToken> {
    let access_token = response
        .access_token
        .ok_or_else(|| anyhow!("Slack OAuth response did not include an access token"))?;
    let expires_at = response.expires_in.map(StoredToken::expires_at_from_now);

    Ok(StoredToken {
        access_token,
        token_type: response.token_type.or_else(|| {
            previous
                .and_then(|token| token.token_type.clone())
                .or_else(|| Some("user".to_string()))
        }),
        scope: response
            .scope
            .or_else(|| previous.and_then(|token| token.scope.clone())),
        refresh_token: response
            .refresh_token
            .or_else(|| previous.and_then(|token| token.refresh_token.clone())),
        expires_in: response
            .expires_in
            .or_else(|| previous.and_then(|token| token.expires_in)),
        expires_at: expires_at.or_else(|| previous.and_then(|token| token.expires_at)),
        team_id: response
            .team
            .as_ref()
            .and_then(|team| team.id.clone())
            .or_else(|| previous.and_then(|token| token.team_id.clone())),
        team_name: response
            .team
            .and_then(|team| team.name)
            .or_else(|| previous.and_then(|token| token.team_name.clone())),
        user_id: response
            .authed_user
            .and_then(|user| user.id)
            .or_else(|| previous.and_then(|token| token.user_id.clone())),
        client_id: client_id
            .map(ToString::to_string)
            .or_else(|| previous.and_then(|token| token.client_id.clone())),
        browser_cookie_d: previous.and_then(|token| token.browser_cookie_d.clone()),
        user_agent: previous.and_then(|token| token.user_agent.clone()),
    })
}

fn auth_debug(enabled: bool, message: &str) {
    if enabled {
        eprintln!("[conduit::auth] {message}");
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::thread;

    use super::*;

    static CALLBACK_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn callback_server() -> (SocketAddr, Server) {
        let server = Server::http(("127.0.0.1", 0)).unwrap();
        let address = server.server_addr().to_ip().unwrap();
        (address, server)
    }

    fn send_callback_request(address: SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn wait_for_listener_release(address: SocketAddr) -> TcpListener {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match TcpListener::bind(address) {
                Ok(listener) => return listener,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Err(error) => panic!("OAuth callback listener was not released: {error}"),
            }
        }
    }

    #[test]
    fn builds_user_token_authorize_url() {
        let config = OAuthConfig {
            client_id: "123.456".to_string(),
            redirect_port: 8934,
            user_scopes: vec!["channels:read".to_string(), "chat:write".to_string()],
        };

        let url = build_authorize_url(&config, "challenge", "state").unwrap();
        let url = Url::parse(&url).unwrap();
        let params = url.query_pairs().into_owned().collect::<HashMap<_, _>>();

        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("slack.com"));
        assert_eq!(url.path(), "/oauth/v2_user/authorize");
        assert_eq!(params.get("client_id").map(String::as_str), Some("123.456"));
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some("channels:read,chat:write")
        );
        assert!(!params.contains_key("user_scope"));
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:8934/callback")
        );
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some("challenge")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(params.get("state").map(String::as_str), Some("state"));
    }

    #[test]
    fn oauth_callback_ignores_unrelated_requests_until_valid_callback() {
        let _test_guard = CALLBACK_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (address, server) = callback_server();
        let cancelled = Arc::new(AtomicBool::new(false));
        let receiver_cancelled = Arc::clone(&cancelled);
        let receiver = thread::spawn(move || {
            receive_oauth_callback(&server, "expected", &receiver_cancelled, false)
        });

        let rejected = send_callback_request(address, "/callback?code=wrong&state=unexpected");
        assert!(rejected.starts_with("HTTP/1.1 400"));
        let accepted = send_callback_request(address, "/callback?code=valid&state=expected");
        assert!(accepted.starts_with("HTTP/1.1 200"));

        let callback = receiver.join().unwrap().unwrap();
        assert_eq!(callback.code.as_deref(), Some("valid"));
        assert_eq!(callback.state.as_deref(), Some("expected"));
    }

    #[test]
    fn cancelled_oauth_callback_releases_its_listener_promptly() {
        let _test_guard = CALLBACK_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (address, server) = callback_server();
        let cancelled = Arc::new(AtomicBool::new(false));
        let receiver_cancelled = Arc::clone(&cancelled);
        let receiver = thread::spawn(move || {
            receive_oauth_callback(&server, "expected", &receiver_cancelled, false)
        });

        cancelled.store(true, Ordering::Release);
        let error = receiver.join().unwrap().unwrap_err();

        assert!(error.to_string().contains("cancelled"));
        let _listener = wait_for_listener_release(address);
    }

    #[test]
    fn default_user_scopes_include_discovery_and_user_group_access() {
        assert!(DEFAULT_USER_SCOPES.contains(&"usergroups:read"));
        assert!(DEFAULT_USER_SCOPES.contains(&"channels:join"));
        assert!(DEFAULT_USER_SCOPES.contains(&"users:read"));
        assert!(DEFAULT_USER_SCOPES.contains(&"users:read.email"));
        assert!(DEFAULT_USER_SCOPES.contains(&"users.profile:read"));
        assert!(DEFAULT_USER_SCOPES.contains(&"im:write"));
        assert!(DEFAULT_USER_SCOPES.contains(&"emoji:read"));
    }

    #[test]
    fn builds_browser_session_token_from_xoxc_xoxd_values() {
        let token = browser_session_token_from_values(
            Some(" xoxc-browser-token ".to_string()),
            Some(" xoxd-cookie-value ".to_string()),
            None,
        )
        .unwrap()
        .expect("token should be created");

        assert_eq!(token.access_token, "xoxc-browser-token");
        assert_eq!(token.token_type.as_deref(), Some("browser_session"));
        assert_eq!(token.browser_cookie_d.as_deref(), Some("xoxd-cookie-value"));
        assert_eq!(
            token.user_agent.as_deref(),
            Some(DEFAULT_BROWSER_USER_AGENT)
        );
        assert!(!token.should_refresh());
    }

    #[test]
    fn uses_custom_browser_session_user_agent() {
        let token = browser_session_token_from_values(
            Some("xoxc-browser-token".to_string()),
            Some("xoxd-cookie-value".to_string()),
            Some(" Browser User Agent ".to_string()),
        )
        .unwrap()
        .expect("token should be created");

        assert_eq!(token.user_agent.as_deref(), Some("Browser User Agent"));
    }

    #[test]
    fn rejects_partial_browser_session_values() {
        let error =
            browser_session_token_from_values(Some("xoxc-browser-token".to_string()), None, None)
                .unwrap_err()
                .to_string();

        assert!(error.contains("XOXC"));
        assert!(error.contains("XOXD"));
    }

    #[test]
    fn app_tokens_are_trimmed_and_require_the_xapp_prefix() {
        assert_eq!(
            normalize_app_token(" xapp-valid-token ").unwrap(),
            "xapp-valid-token"
        );
        assert!(normalize_app_token("xoxp-user-token").is_err());
        assert!(normalize_app_token("xapp-").is_err());
        assert!(normalize_app_token("  ").is_err());
    }
}
