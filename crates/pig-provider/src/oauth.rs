//! Shared OAuth 2.0 + PKCE (RFC 7636) transactional token management.
//!
//! Used by both model providers and remote MCP HTTP servers. Guarantees:
//! - S256 PKCE verifier/challenge generation with URL-safe base64 (no padding)
//! - Single-flight token refresh under an async mutex so concurrent requests
//!   never race on rotating refresh tokens
//! - Atomic on-disk persistence (`FileTokenStore`, temp file + rename, 0600 on Unix)
//! - Reactive 401 refresh deduplication (`force_refresh`)

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::retry::http_client;

/// Refresh tokens proactively this many seconds before their advertised expiry.
pub const DEFAULT_EXPIRY_LEEWAY_SECS: u64 = 60;

/// Generate a high-entropy URL-safe OAuth 2.0 `state` token for CSRF protection.
pub fn generate_state() -> String {
    let mut raw = [0u8; 16];
    fill_entropy(&mut raw);
    URL_SAFE_NO_PAD.encode(raw)
}

/// Discovered OAuth 2.0 server metadata (RFC 9728 / RFC 8414).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthMetadata {
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
    #[serde(default)]
    pub token_endpoint: Option<String>,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
}

/// Discover OAuth 2.0 authorization & token endpoints for a resource server
/// using RFC 9728 (`/.well-known/oauth-protected-resource`) and RFC 8414
/// (`/.well-known/oauth-authorization-server`).
pub async fn discover_oauth_metadata(server_url: &str) -> Result<OAuthMetadata, String> {
    let parsed = reqwest::Url::parse(server_url)
        .map_err(|e| format!("Invalid server URL `{server_url}` for OAuth discovery: {e}"))?;
    let origin = parsed.origin().ascii_serialization();
    if origin == "null" {
        return Err(format!("Server URL `{server_url}` has no valid HTTP origin"));
    }
    let client = http_client();
    let mut auth_origin = origin.clone();
    let mut resource = None;

    // 1. Try RFC 9728 Protected Resource Metadata on the resource server origin.
    let prm_url = format!("{origin}/.well-known/oauth-protected-resource");
    if let Ok(resp) = client
        .get(&prm_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        && resp.status().is_success()
        && let Ok(body) = resp.json::<serde_json::Value>().await
    {
        if let Some(res) = body["resource"].as_str().filter(|s| !s.is_empty()) {
            resource = Some(res.to_string());
        }
        if let Some(as_url) = body["authorization_servers"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            auth_origin = as_url.trim_end_matches('/').to_string();
        }
    }

    // 2. Fetch RFC 8414 Authorization Server Metadata.
    let as_metadata_url = format!("{auth_origin}/.well-known/oauth-authorization-server");
    let resp = client
        .get(&as_metadata_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("OAuth metadata discovery failed for `{as_metadata_url}`: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "OAuth metadata discovery at `{as_metadata_url}` returned HTTP {}",
            resp.status()
        ));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Invalid JSON from `{as_metadata_url}`: {e}"))?;
    Ok(OAuthMetadata {
        authorization_endpoint: body["authorization_endpoint"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        token_endpoint: body["token_endpoint"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        registration_endpoint: body["registration_endpoint"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        resource,
    })
}

/// RFC 7636 PKCE verifier + S256 challenge pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
    pub method: &'static str,
}

impl PkceChallenge {
    /// Generate a fresh high-entropy PKCE verifier and its S256 challenge.
    pub fn generate() -> Self {
        let mut raw = [0u8; 32];
        fill_entropy(&mut raw);
        let verifier = URL_SAFE_NO_PAD.encode(raw);
        Self::from_verifier(verifier)
    }

    /// Derive the S256 challenge from an explicit verifier (useful for RFC 7636 tests).
    pub fn from_verifier(verifier: String) -> Self {
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Self {
            verifier,
            challenge,
            method: "S256",
        }
    }
}

/// OAuth 2.0 client & endpoint configuration (supports RFC 8707 `resource` for MCP).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthConfig {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub authorization_url: Option<String>,
    pub token_url: String,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub resource: Option<String>,
}

/// Persisted OAuth 2.0 token bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthTokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// Unix timestamp in seconds when `access_token` expires (`None` = non-expiring).
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

impl OAuthTokenSet {
    /// Whether this token is expired (or will expire within `leeway_secs`).
    pub fn is_expired(&self, now_secs: u64, leeway_secs: u64) -> bool {
        self.expires_at
            .is_some_and(|exp| now_secs.saturating_add(leeway_secs) >= exp)
    }
}

/// Persistence boundary for OAuth token bundles.
pub trait OAuthTokenStore: Send + Sync {
    fn load(&self) -> Result<Option<OAuthTokenSet>, String>;
    fn save(&self, tokens: &OAuthTokenSet) -> Result<(), String>;
    fn clear(&self) -> Result<(), String>;
}

/// Ephemeral in-memory token store (used in tests and when no `token_file` is configured).
#[derive(Default)]
pub struct MemoryTokenStore {
    tokens: Mutex<Option<OAuthTokenSet>>,
}

impl MemoryTokenStore {
    pub fn new(initial: Option<OAuthTokenSet>) -> Self {
        Self {
            tokens: Mutex::new(initial),
        }
    }
}

impl OAuthTokenStore for MemoryTokenStore {
    fn load(&self) -> Result<Option<OAuthTokenSet>, String> {
        Ok(self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone())
    }

    fn save(&self, tokens: &OAuthTokenSet) -> Result<(), String> {
        *self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(tokens.clone());
        Ok(())
    }

    fn clear(&self) -> Result<(), String> {
        *self
            .tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        Ok(())
    }
}

/// Atomic file-backed OAuth token store (temp file + rename, 0600 permissions on Unix).
pub struct FileTokenStore {
    path: PathBuf,
}

impl FileTokenStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl OAuthTokenStore for FileTokenStore {
    fn load(&self) -> Result<Option<OAuthTokenSet>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map(Some)
                .map_err(|e| format!("Failed to parse OAuth token store {}: {e}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!(
                "Failed to read OAuth token store {}: {e}",
                self.path.display()
            )),
        }
    }

    fn save(&self, tokens: &OAuthTokenSet) -> Result<(), String> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "Failed to create OAuth token directory {}: {e}",
                    parent.display()
                )
            })?;
        }
        let encoded = serde_json::to_vec_pretty(tokens)
            .map_err(|e| format!("Failed to serialize OAuth tokens: {e}"))?;
        let tmp = self.path.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            now_nanos()
        ));
        let write_result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            file.write_all(&encoded)?;
            file.flush()?;
            drop(file);
            std::fs::rename(&tmp, &self.path)
        })();
        if write_result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        write_result.map_err(|e| {
            format!(
                "Failed to persist OAuth token store {}: {e}",
                self.path.display()
            )
        })
    }

    fn clear(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!(
                "Failed to remove OAuth token store {}: {e}",
                self.path.display()
            )),
        }
    }
}

/// Transactional single-flight OAuth 2.0 token manager.
pub struct OAuthTokenManager {
    config: OAuthConfig,
    store: Arc<dyn OAuthTokenStore>,
    client: reqwest::Client,
    /// Serializes refreshes so N concurrent expired requests trigger exactly one
    /// token-endpoint round-trip (protecting rotating refresh tokens).
    refresh_lock: tokio::sync::Mutex<()>,
    leeway_secs: u64,
}

impl OAuthTokenManager {
    pub fn new(config: OAuthConfig, store: Arc<dyn OAuthTokenStore>) -> Self {
        Self {
            config,
            store,
            client: http_client(),
            refresh_lock: tokio::sync::Mutex::new(()),
            leeway_secs: DEFAULT_EXPIRY_LEEWAY_SECS,
        }
    }

    pub fn with_leeway_secs(mut self, leeway_secs: u64) -> Self {
        self.leeway_secs = leeway_secs;
        self
    }

    pub fn config(&self) -> &OAuthConfig {
        &self.config
    }

    /// Build the browser authorization URL carrying PKCE S256 parameters.
    pub fn build_authorization_url(
        &self,
        pkce: &PkceChallenge,
        state: &str,
    ) -> Result<String, String> {
        self.build_authorization_url_with_redirect(pkce, state, self.config.redirect_uri.as_deref())
    }

    /// Build the browser authorization URL using an explicit `redirect_uri`
    /// override (such as an ephemeral `http://127.0.0.1:<port>/callback`).
    pub fn build_authorization_url_with_redirect(
        &self,
        pkce: &PkceChallenge,
        state: &str,
        redirect_uri: Option<&str>,
    ) -> Result<String, String> {
        let base = self
            .config
            .authorization_url
            .as_deref()
            .ok_or_else(|| "OAuth authorization_url is not configured".to_string())?;
        let mut url = reqwest::Url::parse(base)
            .map_err(|e| format!("Invalid OAuth authorization_url `{base}`: {e}"))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", &self.config.client_id);
            query.append_pair("code_challenge", &pkce.challenge);
            query.append_pair("code_challenge_method", pkce.method);
            query.append_pair("state", state);
            if let Some(redirect) = redirect_uri {
                query.append_pair("redirect_uri", redirect);
            }
            if !self.config.scopes.is_empty() {
                query.append_pair("scope", &self.config.scopes.join(" "));
            }
            if let Some(resource) = &self.config.resource {
                query.append_pair("resource", resource);
            }
        }
        Ok(url.to_string())
    }

    /// Exchange an authorization code + PKCE verifier for tokens and persist them.
    pub async fn exchange_code(
        &self,
        code: &str,
        pkce: &PkceChallenge,
    ) -> Result<OAuthTokenSet, String> {
        self.exchange_code_with_redirect(code, pkce, self.config.redirect_uri.as_deref())
            .await
    }

    /// Exchange an authorization code + PKCE verifier using an explicit
    /// `redirect_uri` (matching the loopback URI sent during authorization).
    pub async fn exchange_code_with_redirect(
        &self,
        code: &str,
        pkce: &PkceChallenge,
        redirect_uri: Option<&str>,
    ) -> Result<OAuthTokenSet, String> {
        let _guard = self.refresh_lock.lock().await;
        let mut form = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("client_id".to_string(), self.config.client_id.clone()),
            ("code".to_string(), code.to_string()),
            ("code_verifier".to_string(), pkce.verifier.clone()),
        ];
        if let Some(secret) = &self.config.client_secret {
            form.push(("client_secret".to_string(), secret.clone()));
        }
        if let Some(redirect) = redirect_uri {
            form.push(("redirect_uri".to_string(), redirect.to_string()));
        }
        if let Some(resource) = &self.config.resource {
            form.push(("resource".to_string(), resource.clone()));
        }
        let tokens = self.post_token_form(&form, None).await?;
        self.store.save(&tokens)?;
        Ok(tokens)
    }

    /// Clear any persisted tokens in the underlying store.
    pub fn clear_tokens(&self) -> Result<(), String> {
        self.store.clear()
    }

    /// Run a complete interactive OAuth 2.0 + PKCE loopback authorization flow:
    /// 1. Binds a loopback listener on `127.0.0.1` (reusing the configured
    ///    loopback port/path from `config.redirect_uri`, or an ephemeral port
    ///    with `/callback`).
    /// 2. Generates a fresh S256 [`PkceChallenge`] and CSRF `state`, builds the
    ///    authorization URL, and hands it to `on_authorize_url`.
    /// 3. Awaits the browser callback on the loopback path, validates `state`,
    ///    responds with a clean HTML page, and exchanges the code for persisted
    ///    tokens.
    pub async fn authorize_with_loopback<F>(
        &self,
        timeout: Duration,
        cancel: CancellationToken,
        on_authorize_url: F,
    ) -> Result<OAuthTokenSet, String>
    where
        F: FnOnce(String),
    {
        let (bind_addr, callback_path, configured_host) =
            parse_loopback_redirect(self.config.redirect_uri.as_deref())?;
        let listener = TcpListener::bind(&bind_addr)
            .await
            .map_err(|e| format!("Failed to bind OAuth loopback listener on {bind_addr}: {e}"))?;
        let bound = listener
            .local_addr()
            .map_err(|e| format!("Failed to inspect OAuth loopback address: {e}"))?;
        let redirect_uri = format!("http://{configured_host}:{}{callback_path}", bound.port());

        let pkce = PkceChallenge::generate();
        let state = generate_state();
        let auth_url =
            self.build_authorization_url_with_redirect(&pkce, &state, Some(&redirect_uri))?;
        on_authorize_url(auth_url);

        let code = await_loopback_callback(&listener, &callback_path, &state, timeout, cancel)
            .await?;
        self.exchange_code_with_redirect(&code, &pkce, Some(&redirect_uri))
            .await
    }

    /// Return a valid access token, refreshing transactionally once if expired.
    pub async fn valid_access_token(&self) -> Result<String, String> {
        let now = now_secs();
        if let Some(current) = self.store.load()?
            && !current.access_token.is_empty()
            && !current.is_expired(now, self.leeway_secs)
        {
            return Ok(current.access_token);
        }

        let _guard = self.refresh_lock.lock().await;
        // Double-check after acquiring the mutex: a peer request may have
        // already refreshed and persisted a new token set while we waited.
        let now = now_secs();
        let current = self.store.load()?.ok_or_else(|| {
            "No OAuth token available; complete OAuth authorization first".to_string()
        })?;
        if !current.access_token.is_empty() && !current.is_expired(now, self.leeway_secs) {
            return Ok(current.access_token);
        }
        let refreshed = self.refresh_locked(&current).await?;
        Ok(refreshed.access_token)
    }

    /// Reactive refresh triggered by an upstream HTTP 401. If another concurrent
    /// caller already rotated past `failed_access_token`, returns the already
    /// refreshed token without issuing a duplicate refresh call.
    pub async fn force_refresh(&self, failed_access_token: &str) -> Result<String, String> {
        let _guard = self.refresh_lock.lock().await;
        let current = self.store.load()?.ok_or_else(|| {
            "Cannot refresh OAuth token: no token bundle stored".to_string()
        })?;
        if !current.access_token.is_empty() && current.access_token != failed_access_token {
            return Ok(current.access_token);
        }
        let refreshed = self.refresh_locked(&current).await?;
        Ok(refreshed.access_token)
    }

    async fn refresh_locked(&self, current: &OAuthTokenSet) -> Result<OAuthTokenSet, String> {
        let refresh_token = current.refresh_token.as_deref().ok_or_else(|| {
            "OAuth access token expired and no refresh_token is available".to_string()
        })?;
        let mut form = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("client_id".to_string(), self.config.client_id.clone()),
            ("refresh_token".to_string(), refresh_token.to_string()),
        ];
        if let Some(secret) = &self.config.client_secret {
            form.push(("client_secret".to_string(), secret.clone()));
        }
        if !self.config.scopes.is_empty() {
            form.push(("scope".to_string(), self.config.scopes.join(" ")));
        }
        if let Some(resource) = &self.config.resource {
            form.push(("resource".to_string(), resource.clone()));
        }
        let refreshed = self
            .post_token_form(&form, Some(refresh_token.to_string()))
            .await?;
        self.store.save(&refreshed)?;
        Ok(refreshed)
    }

    async fn post_token_form(
        &self,
        form: &[(String, String)],
        fallback_refresh_token: Option<String>,
    ) -> Result<OAuthTokenSet, String> {
        let response = self
            .client
            .post(&self.config.token_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(form)
            .send()
            .await
            .map_err(|e| format!("OAuth token request failed: {e}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("Failed to read OAuth token response: {e}"))?;
        if !status.is_success() {
            return Err(format!("OAuth token endpoint returned HTTP {status}: {body}"));
        }
        let payload: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("OAuth token response is not valid JSON: {e}"))?;
        let access_token = payload["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "OAuth token response missing access_token".to_string())?
            .to_string();
        let refresh_token = payload["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or(fallback_refresh_token);
        let token_type = payload["token_type"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("Bearer")
            .to_string();
        let expires_at = payload["expires_in"]
            .as_u64()
            .map(|secs| now_secs().saturating_add(secs));
        let scope = payload["scope"].as_str().map(str::to_string);
        Ok(OAuthTokenSet {
            access_token,
            refresh_token,
            token_type,
            expires_at,
            scope,
        })
    }
}

fn parse_loopback_redirect(redirect_uri: Option<&str>) -> Result<(String, String, String), String> {
    let Some(raw) = redirect_uri.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok((
            "127.0.0.1:0".to_string(),
            "/callback".to_string(),
            "127.0.0.1".to_string(),
        ));
    };
    let parsed = reqwest::Url::parse(raw)
        .map_err(|e| format!("Invalid OAuth redirect_uri `{raw}`: {e}"))?;
    if parsed.scheme() != "http" {
        return Err(format!(
            "OAuth loopback redirect_uri must use http://, got `{raw}`"
        ));
    }
    let host = parsed.host_str().unwrap_or("");
    if host != "127.0.0.1" && !host.eq_ignore_ascii_case("localhost") {
        return Err(format!(
            "OAuth loopback redirect_uri must target 127.0.0.1 or localhost, got `{host}`"
        ));
    }
    let port = parsed.port().unwrap_or(0);
    let path = if parsed.path().is_empty() {
        "/callback".to_string()
    } else {
        parsed.path().to_string()
    };
    Ok((format!("127.0.0.1:{port}"), path, host.to_string()))
}

async fn write_http_html(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    title: &str,
    message: &str,
) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family:system-ui,sans-serif;max-width:480px;margin:48px auto;padding:0 16px\">\
         <h2>{title}</h2><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

async fn await_loopback_callback(
    listener: &TcpListener,
    callback_path: &str,
    expected_state: &str,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("OAuth authorization timed out waiting for browser callback".to_string());
        }
        let accepted = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err("OAuth authorization cancelled".to_string());
            }
            res = tokio::time::timeout(remaining, listener.accept()) => match res {
                Ok(Ok((socket, _))) => socket,
                Ok(Err(e)) => return Err(format!("OAuth loopback accept failed: {e}")),
                Err(_) => return Err("OAuth authorization timed out waiting for browser callback".to_string()),
            }
        };
        let mut socket = accepted;
        let mut buf = [0u8; 8192];
        let n = match tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => continue,
        };
        let request = String::from_utf8_lossy(&buf[..n]);
        let Some(first_line) = request.lines().next() else {
            continue;
        };
        let mut parts = first_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("");
        if method != "GET" {
            write_http_html(
                &mut socket,
                "405 Method Not Allowed",
                "Method not allowed",
                "Expected a GET OAuth redirect.",
            )
            .await;
            continue;
        }
        let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
            write_http_html(
                &mut socket,
                "400 Bad Request",
                "Invalid callback request",
                "Could not parse OAuth callback URL.",
            )
            .await;
            continue;
        };
        if url.path() != callback_path {
            write_http_html(
                &mut socket,
                "404 Not Found",
                "Not found",
                "Waiting for OAuth callback.",
            )
            .await;
            continue;
        }
        let mut code: Option<String> = None;
        let mut state: Option<String> = None;
        let mut error: Option<String> = None;
        let mut error_description: Option<String> = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                "error" => error = Some(v.into_owned()),
                "error_description" => error_description = Some(v.into_owned()),
                _ => {}
            }
        }
        if let Some(err) = error {
            let detail = error_description
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|d| format!("{err}: {d}"))
                .unwrap_or(err);
            write_http_html(
                &mut socket,
                "400 Bad Request",
                "Authorization failed",
                "The OAuth provider returned an error. You may close this tab.",
            )
            .await;
            return Err(format!("OAuth provider rejected authorization: {detail}"));
        }
        if state.as_deref() != Some(expected_state) {
            write_http_html(
                &mut socket,
                "400 Bad Request",
                "Invalid OAuth state",
                "State mismatch detected; authorization aborted.",
            )
            .await;
            return Err("OAuth callback state mismatch (possible CSRF)".to_string());
        }
        let Some(code) = code.filter(|s| !s.is_empty()) else {
            write_http_html(
                &mut socket,
                "400 Bad Request",
                "Missing authorization code",
                "The callback URL did not include a code parameter.",
            )
            .await;
            return Err("OAuth callback did not include an authorization code".to_string());
        };
        write_http_html(
            &mut socket,
            "200 OK",
            "Authorization complete",
            "You may close this browser tab and return to Pig Code.",
        )
        .await;
        return Ok(code);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

fn fill_entropy(buf: &mut [u8]) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut seed = now_nanos() as u64
        ^ (u64::from(std::process::id()).rotate_left(19))
        ^ COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for chunk in buf.chunks_mut(8) {
        seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        let bytes = z.to_le_bytes();
        let len = chunk.len();
        chunk.copy_from_slice(&bytes[..len]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn pkce_s256_matches_rfc_7636_test_vector() {
        // RFC 7636 Appendix B test vector
        let pkce = PkceChallenge::from_verifier(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string(),
        );
        assert_eq!(pkce.method, "S256");
        assert_eq!(pkce.challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn authorization_url_includes_pkce_scopes_and_resource() {
        let cfg = OAuthConfig {
            client_id: "pig-client".into(),
            client_secret: None,
            authorization_url: Some("https://auth.example.com/authorize".into()),
            token_url: "https://auth.example.com/token".into(),
            redirect_uri: Some("http://127.0.0.1:8787/callback".into()),
            scopes: vec!["openid".into(), "mcp:read".into()],
            resource: Some("https://mcp.example.com".into()),
        };
        let mgr = OAuthTokenManager::new(cfg, Arc::new(MemoryTokenStore::default()));
        let pkce = PkceChallenge::generate();
        let url = mgr
            .build_authorization_url(&pkce, "state-123")
            .expect("auth url");
        assert!(url.contains("response_type=code"), "{url}");
        assert!(url.contains("client_id=pig-client"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains(&pkce.challenge), "{url}");
        assert!(url.contains("state=state-123"), "{url}");
        assert!(url.contains("resource=https%3A%2F%2Fmcp.example.com"), "{url}");
    }

    #[tokio::test]
    async fn single_flight_refresh_deduplicates_concurrent_callers_and_rotates_store() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let refresh_hits = Arc::new(AtomicUsize::new(0));
        let hits_server = Arc::clone(&refresh_hits);

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let hits = Arc::clone(&hits_server);
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let n = hits.fetch_add(1, Ordering::SeqCst) + 1;
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    let body = serde_json::json!({
                        "access_token": format!("fresh-token-{n}"),
                        "refresh_token": format!("rotated-refresh-{n}"),
                        "token_type": "Bearer",
                        "expires_in": 3600
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let tmp_file = std::env::temp_dir().join(format!(
            "pig-oauth-test-{}-{}.json",
            std::process::id(),
            now_nanos()
        ));
        let store = Arc::new(FileTokenStore::new(&tmp_file));
        store
            .save(&OAuthTokenSet {
                access_token: "stale-token".into(),
                refresh_token: Some("rt-0".into()),
                token_type: "Bearer".into(),
                expires_at: Some(1), // already expired
                scope: None,
            })
            .expect("seed store");

        let mgr = Arc::new(OAuthTokenManager::new(
            OAuthConfig {
                client_id: "cid".into(),
                client_secret: None,
                authorization_url: None,
                token_url: format!("http://{addr}/token"),
                redirect_uri: None,
                scopes: vec![],
                resource: None,
            },
            store.clone(),
        ));

        // 8 concurrent callers hitting an expired token must produce ONLY 1 refresh POST.
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let m = Arc::clone(&mgr);
            tasks.push(tokio::spawn(async move { m.valid_access_token().await }));
        }
        let results: Vec<String> = futures_util::future::join_all(tasks)
            .await
            .into_iter()
            .map(|r| r.unwrap().unwrap())
            .collect();
        assert_eq!(refresh_hits.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|t| t == "fresh-token-1"));

        // Reactive 401 force_refresh deduplicates too: 4 callers failing with
        // "fresh-token-1" trigger only 1 additional refresh -> "fresh-token-2".
        let mut force_tasks = Vec::new();
        for _ in 0..4 {
            let m = Arc::clone(&mgr);
            force_tasks.push(tokio::spawn(async move {
                m.force_refresh("fresh-token-1").await
            }));
        }
        let forced: Vec<String> = futures_util::future::join_all(force_tasks)
            .await
            .into_iter()
            .map(|r| r.unwrap().unwrap())
            .collect();
        assert_eq!(refresh_hits.load(Ordering::SeqCst), 2);
        assert!(forced.iter().all(|t| t == "fresh-token-2"));

        let persisted = store.load().unwrap().unwrap();
        assert_eq!(persisted.access_token, "fresh-token-2");
        assert_eq!(persisted.refresh_token.as_deref(), Some("rotated-refresh-2"));
        let _ = store.clear();
    }

    #[tokio::test]
    async fn loopback_pkce_flow_verifies_s256_ignores_favicon_and_persists_tokens() {
        let token_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind token");
        let token_addr = token_listener.local_addr().expect("token addr");
        let expected_challenge: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let challenge_srv = Arc::clone(&expected_challenge);

        tokio::spawn(async move {
            let Ok((mut socket, _)) = token_listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 8192];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let raw = String::from_utf8_lossy(&buf[..n]);
            let form_body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
            let params: std::collections::HashMap<String, String> =
                reqwest::Url::parse(&format!("http://127.0.0.1/?{form_body}"))
                    .unwrap()
                    .query_pairs()
                    .into_owned()
                    .collect();
            let verifier = params.get("code_verifier").cloned().unwrap_or_default();
            let derived = PkceChallenge::from_verifier(verifier).challenge;
            let expected = challenge_srv.lock().unwrap().clone().unwrap_or_default();
            let (status, body) = if params.get("code").map(String::as_str) == Some("auth-code-42")
                && !expected.is_empty()
                && derived == expected
            {
                (
                    "200 OK",
                    serde_json::json!({
                        "access_token": "pkce-access-token",
                        "refresh_token": "pkce-refresh-token",
                        "token_type": "Bearer",
                        "expires_in": 3600
                    })
                    .to_string(),
                )
            } else {
                (
                    "400 Bad Request",
                    serde_json::json!({ "error": "invalid_grant" }).to_string(),
                )
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        let tmp_file = std::env::temp_dir().join(format!(
            "pig-oauth-pkce-{}-{}.json",
            std::process::id(),
            now_nanos()
        ));
        let store = Arc::new(FileTokenStore::new(&tmp_file));
        let mgr = OAuthTokenManager::new(
            OAuthConfig {
                client_id: "pig-desktop".into(),
                client_secret: None,
                authorization_url: Some("https://auth.example.com/authorize".into()),
                token_url: format!("http://{token_addr}/token"),
                redirect_uri: None,
                scopes: vec!["mcp:tools".into()],
                resource: Some("https://mcp.example.com".into()),
            },
            store.clone(),
        );

        let challenge_capture = Arc::clone(&expected_challenge);
        let tokens = mgr
            .authorize_with_loopback(
                Duration::from_secs(5),
                CancellationToken::new(),
                move |auth_url| {
                    let parsed = reqwest::Url::parse(&auth_url).expect("parse auth url");
                    let qp: std::collections::HashMap<String, String> =
                        parsed.query_pairs().into_owned().collect();
                    assert_eq!(qp.get("code_challenge_method").map(String::as_str), Some("S256"));
                    *challenge_capture.lock().unwrap() = qp.get("code_challenge").cloned();
                    let redirect = qp.get("redirect_uri").cloned().expect("redirect_uri");
                    let state = qp.get("state").cloned().expect("state");
                    tokio::spawn(async move {
                        let client = reqwest::Client::new();
                        // Unrelated browser request (/favicon.ico) must not abort the flow.
                        let origin = reqwest::Url::parse(&redirect)
                            .unwrap()
                            .origin()
                            .ascii_serialization();
                        let _ = client.get(format!("{origin}/favicon.ico")).send().await;
                        let callback = format!("{redirect}?code=auth-code-42&state={state}");
                        let resp = client.get(&callback).send().await.expect("callback");
                        assert!(resp.status().is_success());
                    });
                },
            )
            .await
            .expect("pkce loopback flow");

        assert_eq!(tokens.access_token, "pkce-access-token");
        assert_eq!(tokens.refresh_token.as_deref(), Some("pkce-refresh-token"));
        assert_eq!(
            store.load().unwrap().unwrap().access_token,
            "pkce-access-token"
        );
        let _ = store.clear();
    }

    #[tokio::test]
    async fn loopback_pkce_rejects_csrf_state_mismatch() {
        let mgr = OAuthTokenManager::new(
            OAuthConfig {
                client_id: "pig-desktop".into(),
                client_secret: None,
                authorization_url: Some("https://auth.example.com/authorize".into()),
                token_url: "http://127.0.0.1:1/token".into(),
                redirect_uri: None,
                scopes: vec![],
                resource: None,
            },
            Arc::new(MemoryTokenStore::default()),
        );
        let err = mgr
            .authorize_with_loopback(
                Duration::from_secs(5),
                CancellationToken::new(),
                |auth_url| {
                    let parsed = reqwest::Url::parse(&auth_url).unwrap();
                    let redirect = parsed
                        .query_pairs()
                        .find(|(k, _)| k == "redirect_uri")
                        .unwrap()
                        .1
                        .into_owned();
                    tokio::spawn(async move {
                        let _ = reqwest::Client::new()
                            .get(format!("{redirect}?code=abc&state=wrong-state"))
                            .send()
                            .await;
                    });
                },
            )
            .await
            .expect_err("wrong state must fail");
        assert!(err.contains("state mismatch"), "{err}");
    }

    #[tokio::test]
    async fn discover_oauth_metadata_reads_protected_resource_and_auth_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let origin = format!("http://{addr}");
        let origin_srv = origin.clone();

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let origin = origin_srv.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("");
                    let body = match path {
                        "/.well-known/oauth-protected-resource" => serde_json::json!({
                            "resource": format!("{origin}/mcp"),
                            "authorization_servers": [origin]
                        })
                        .to_string(),
                        "/.well-known/oauth-authorization-server" => serde_json::json!({
                            "authorization_endpoint": format!("{origin}/oauth/authorize"),
                            "token_endpoint": format!("{origin}/oauth/token")
                        })
                        .to_string(),
                        _ => "{}".to_string(),
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let meta = discover_oauth_metadata(&format!("{origin}/mcp"))
            .await
            .expect("discover metadata");
        assert_eq!(
            meta.authorization_endpoint.as_deref(),
            Some(format!("{origin}/oauth/authorize").as_str())
        );
        assert_eq!(
            meta.token_endpoint.as_deref(),
            Some(format!("{origin}/oauth/token").as_str())
        );
        assert_eq!(
            meta.resource.as_deref(),
            Some(format!("{origin}/mcp").as_str())
        );
    }
}
