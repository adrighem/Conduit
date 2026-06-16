use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use keyring::Entry;
use rand::random;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tiny_http::{Response, Server};
use url::Url;

use crate::models::StoredToken;

const KEYRING_SERVICE: &str = "eu.vanadrighem.conduit";
const KEYRING_USER: &str = "slack-user-token";

pub const DEFAULT_REDIRECT_PORT: u16 = 8934;
pub const DEFAULT_USER_SCOPES: &[&str] = &[
    "channels:read",
    "channels:history",
    "groups:read",
    "groups:history",
    "im:read",
    "im:history",
    "mpim:read",
    "mpim:history",
    "users:read",
    "chat:write",
    "search:read",
    "stars:read",
    "stars:write",
    "reactions:read",
    "reactions:write",
    "files:read",
    "files:write",
];

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
pub struct SlackOAuthClient {
    http: Client,
}

impl SlackOAuthClient {
    pub fn new() -> Self {
        Self {
            http: Client::new(),
        }
    }

    pub async fn authenticate(&self, config: OAuthConfig) -> Result<StoredToken> {
        if config.client_id.trim().is_empty() {
            return Err(anyhow!("Slack client ID is required"));
        }

        let pkce = PkcePair::generate();
        let state = random_urlsafe(32);
        let authorize_url = build_authorize_url(&config, &pkce.challenge, &state)?;
        let redirect_uri = config.redirect_uri();
        let callback =
            wait_for_oauth_callback(config.redirect_port, authorize_url, state.clone()).await?;

        if callback.state.as_deref() != Some(state.as_str()) {
            return Err(anyhow!("OAuth state mismatch"));
        }

        if let Some(error) = callback.error {
            return Err(anyhow!("Slack authorization failed: {error}"));
        }

        let code = callback
            .code
            .ok_or_else(|| anyhow!("Slack authorization did not return a code"))?;

        exchange_user_code(
            &self.http,
            &config.client_id,
            &redirect_uri,
            &pkce.verifier,
            &code,
        )
        .await
    }
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

fn build_authorize_url(config: &OAuthConfig, challenge: &str, state: &str) -> Result<String> {
    let mut url = Url::parse("https://slack.com/oauth/v2/authorize")?;
    url.query_pairs_mut()
        .append_pair("client_id", config.client_id.trim())
        .append_pair("user_scope", &config.user_scopes.join(","))
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
) -> Result<OAuthCallback> {
    tokio::task::spawn_blocking(move || {
        let addr = SocketAddr::from(([127, 0, 0, 1], redirect_port));
        let server = Server::http(addr)
            .map_err(|error| anyhow!("failed to start local OAuth callback server: {error}"))?;

        open::that_detached(&authorize_url).context("failed to open Slack authorization URL")?;

        let request = server.recv().context("failed to receive OAuth callback")?;
        let callback_url = Url::parse(&format!("http://127.0.0.1{}", request.url()))
            .context("failed to parse OAuth callback URL")?;
        let params: HashMap<String, String> = callback_url.query_pairs().into_owned().collect();

        let state_ok = params.get("state").map(String::as_str) == Some(expected_state.as_str());
        let body = if state_ok && params.contains_key("code") {
            "Conduit is connected. You can close this window."
        } else {
            "Conduit could not complete Slack authorization. Return to the app for details."
        };
        let _ = request.respond(Response::from_string(body));

        Ok(OAuthCallback {
            code: params.get("code").cloned(),
            state: params.get("state").cloned(),
            error: params.get("error").cloned(),
        })
    })
    .await?
}

async fn exchange_user_code(
    http: &Client,
    client_id: &str,
    redirect_uri: &str,
    code_verifier: &str,
    code: &str,
) -> Result<StoredToken> {
    let response = http
        .post("https://slack.com/api/oauth.v2.user.access")
        .form(&[
            ("client_id", client_id),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", code_verifier),
            ("grant_type", "authorization_code"),
        ])
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

    let access_token = response
        .access_token
        .ok_or_else(|| anyhow!("Slack OAuth response did not include an access token"))?;

    Ok(StoredToken {
        access_token,
        token_type: response.token_type,
        scope: response.scope,
        refresh_token: response.refresh_token,
        expires_in: response.expires_in,
        team_id: response.team.as_ref().and_then(|team| team.id.clone()),
        team_name: response.team.and_then(|team| team.name),
        user_id: response.authed_user.and_then(|user| user.id),
    })
}
