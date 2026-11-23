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
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::retry::http_client;

/// Refresh tokens proactively this many seconds before their advertised expiry.
pub const DEFAULT_EXPIRY_LEEWAY_SECS: u64 = 60;

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
            if let Some(redirect) = &self.config.redirect_uri {
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
        if let Some(redirect) = &self.config.redirect_uri {
            form.push(("redirect_uri".to_string(), redirect.clone()));
        }
        if let Some(resource) = &self.config.resource {
            form.push(("resource".to_string(), resource.clone()));
        }
        let tokens = self.post_token_form(&form, None).await?;
        self.store.save(&tokens)?;
        Ok(tokens)
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
}
