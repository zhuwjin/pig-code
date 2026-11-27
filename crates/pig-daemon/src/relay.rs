use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use pig_core::AgentHandle;
use pig_protocol::{
    DAEMON_PROTOCOL_VERSION, DaemonMessage, DaemonRequest, DeviceCodePollResponse,
    DeviceCodeStartResponse, RELAY_PROTOCOL_VERSION, RelayAuthResponse, RelayConnectionStatus,
    RelayDaemonFrame, RelayDeviceInfo, RelayServerFrame,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

use crate::rpc::RpcContext;
use crate::server::{SUBSCRIBER_CHANNEL_CAPACITY, SharedState, dispatch_client_request};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(20);
const TUNNEL_OUTBOUND_CAPACITY: usize = 512;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelayCredentials {
    pub relay_url: String,
    pub user_id: String,
    pub username: String,
    pub session_token: String,
    pub device_id: String,
    pub device_token: String,
    pub device_name: String,
}

pub fn relay_credentials_path(data_dir: &Path) -> PathBuf {
    data_dir.join("daemon").join("relay.json")
}

pub fn load_saved_relay_credentials(data_dir: &Path) -> Option<RelayCredentials> {
    let bytes = std::fs::read(relay_credentials_path(data_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_relay_credentials(data_dir: &Path, creds: &RelayCredentials) -> Result<(), String> {
    let directory = data_dir.join("daemon");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    #[cfg(windows)]
    crate::server::restrict_windows_permissions(&directory);

    let path = relay_credentials_path(data_dir);
    let temporary = directory.join(format!("relay-{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    let encoded = serde_json::to_vec_pretty(creds).map_err(|error| error.to_string())?;
    file.write_all(&encoded)
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);
    std::fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
    #[cfg(windows)]
    crate::server::restrict_windows_permissions(&path);
    Ok(())
}

fn delete_relay_credentials(data_dir: &Path) {
    let _ = std::fs::remove_file(relay_credentials_path(data_dir));
}

pub fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("pig-{}-{}", std::env::consts::OS, std::env::consts::ARCH))
}

pub fn normalize_relay_http_base(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("relay URL cannot be empty".to_string());
    }
    let normalized = if let Some(rest) = trimmed.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    let parsed = url::Url::parse(&normalized).map_err(|error| format!("invalid relay URL: {error}"))?;
    if parsed.host_str().is_none() {
        return Err("relay URL must include a host".to_string());
    }
    Ok(normalized)
}

pub fn relay_ws_endpoint(base_http: &str, path: &str) -> Result<String, String> {
    let base = normalize_relay_http_base(base_http)?;
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return Err("unsupported relay URL scheme".to_string());
    };
    Ok(format!("{ws_base}{path}"))
}

#[derive(Default)]
struct RelayInner {
    credentials: Option<RelayCredentials>,
    connected: bool,
    last_error: Option<String>,
    generation: u64,
}

#[derive(Clone)]
pub struct RelayManager {
    data_dir: PathBuf,
    default_cwd: PathBuf,
    http: reqwest::Client,
    inner: Arc<Mutex<RelayInner>>,
    notify: Arc<Notify>,
    state: Arc<Mutex<SharedState>>,
    agent: Arc<AgentHandle>,
}

impl RelayManager {
    pub(crate) fn new(
        data_dir: PathBuf,
        default_cwd: PathBuf,
        auto_load: bool,
        state: Arc<Mutex<SharedState>>,
        agent: Arc<AgentHandle>,
    ) -> Self {
        let credentials = if auto_load {
            load_saved_relay_credentials(&data_dir)
        } else {
            None
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            data_dir,
            default_cwd,
            http,
            inner: Arc::new(Mutex::new(RelayInner {
                credentials,
                connected: false,
                last_error: None,
                generation: 1,
            })),
            notify: Arc::new(Notify::new()),
            state,
            agent,
        }
    }

    pub(crate) fn start(&self, rpc_ctx: RpcContext) {
        let manager = self.clone();
        tokio::spawn(async move {
            manager.supervisor_loop(rpc_ctx).await;
        });
    }

    pub fn status(&self) -> RelayConnectionStatus {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(creds) = &inner.credentials {
            RelayConnectionStatus {
                configured: true,
                connected: inner.connected,
                relay_url: Some(creds.relay_url.clone()),
                user_id: Some(creds.user_id.clone()),
                username: Some(creds.username.clone()),
                device_id: Some(creds.device_id.clone()),
                device_name: Some(creds.device_name.clone()),
                session_token: Some(creds.session_token.clone()),
                last_error: inner.last_error.clone(),
            }
        } else {
            RelayConnectionStatus {
                configured: false,
                connected: false,
                last_error: inner.last_error.clone(),
                ..RelayConnectionStatus::default()
            }
        }
    }

    fn apply_credentials(&self, creds: Option<RelayCredentials>) -> Result<(), String> {
        if let Some(creds) = &creds {
            save_relay_credentials(&self.data_dir, creds)?;
        } else {
            delete_relay_credentials(&self.data_dir);
        }
        {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.credentials = creds;
            inner.connected = false;
            inner.last_error = None;
            inner.generation = inner.generation.wrapping_add(1);
        }
        self.notify.notify_waiters();
        Ok(())
    }

    pub async fn register(
        &self,
        relay_url: &str,
        username: &str,
        password: &str,
        device_name: Option<&str>,
    ) -> Result<RelayConnectionStatus, String> {
        self.authenticate_endpoint(
            "/api/v1/auth/register",
            relay_url,
            username,
            password,
            device_name,
        )
        .await
    }

    pub async fn login(
        &self,
        relay_url: &str,
        username: &str,
        password: &str,
        device_name: Option<&str>,
    ) -> Result<RelayConnectionStatus, String> {
        self.authenticate_endpoint(
            "/api/v1/auth/login",
            relay_url,
            username,
            password,
            device_name,
        )
        .await
    }

    async fn authenticate_endpoint(
        &self,
        path: &str,
        relay_url: &str,
        username: &str,
        password: &str,
        device_name: Option<&str>,
    ) -> Result<RelayConnectionStatus, String> {
        let base = normalize_relay_http_base(relay_url)?;
        let resolved_name = device_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(default_device_name);
        let existing_device_id = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .credentials
            .as_ref()
            .filter(|c| c.relay_url == base)
            .map(|c| c.device_id.clone());

        let resp = self
            .http
            .post(format!("{base}{path}"))
            .json(&json!({
                "username": username,
                "password": password,
                "device_name": resolved_name,
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "device_id": existing_device_id,
            }))
            .send()
            .await
            .map_err(|error| format!("failed to reach relay: {error}"))?;
        let auth: RelayAuthResponse = decode_http_json(resp).await?;
        let device_id = auth
            .device_id
            .ok_or_else(|| "relay did not return a device_id".to_string())?;
        let device_token = auth
            .device_token
            .ok_or_else(|| "relay did not return a device_token".to_string())?;
        self.apply_credentials(Some(RelayCredentials {
            relay_url: base,
            user_id: auth.user.user_id,
            username: auth.user.username,
            session_token: auth.session_token,
            device_id,
            device_token,
            device_name: resolved_name,
        }))?;
        Ok(self.status())
    }

    pub async fn device_start(
        &self,
        relay_url: &str,
        device_name: Option<&str>,
    ) -> Result<DeviceCodeStartResponse, String> {
        let base = normalize_relay_http_base(relay_url)?;
        let resolved_name = device_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(default_device_name);
        let resp = self
            .http
            .post(format!("{base}/api/v1/auth/device/start"))
            .json(&json!({
                "device_name": resolved_name,
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
            }))
            .send()
            .await
            .map_err(|error| format!("failed to reach relay: {error}"))?;
        decode_http_json(resp).await
    }

    pub async fn device_poll(
        &self,
        relay_url: &str,
        device_code: &str,
        device_name: Option<&str>,
    ) -> Result<Value, String> {
        let base = normalize_relay_http_base(relay_url)?;
        let resolved_name = device_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(default_device_name);
        let resp = self
            .http
            .post(format!("{base}/api/v1/auth/device/poll"))
            .json(&json!({ "device_code": device_code }))
            .send()
            .await
            .map_err(|error| format!("failed to reach relay: {error}"))?;
        let poll: DeviceCodePollResponse = decode_http_json(resp).await?;
        match poll {
            DeviceCodePollResponse::Pending => Ok(json!({ "status": "pending" })),
            DeviceCodePollResponse::Expired => Ok(json!({ "status": "expired" })),
            DeviceCodePollResponse::Approved {
                user,
                session_token,
                device_id,
                device_token,
            } => {
                self.apply_credentials(Some(RelayCredentials {
                    relay_url: base,
                    user_id: user.user_id,
                    username: user.username,
                    session_token,
                    device_id,
                    device_token,
                    device_name: resolved_name,
                }))?;
                Ok(json!({
                    "status": "approved",
                    "relay": self.status(),
                }))
            }
        }
    }

    pub async fn approve_device_code(&self, user_code: &str) -> Result<bool, String> {
        let creds = self.require_credentials()?;
        let resp = self
            .http
            .post(format!("{}/api/v1/auth/device/approve", creds.relay_url))
            .bearer_auth(&creds.session_token)
            .json(&json!({ "user_code": user_code }))
            .send()
            .await
            .map_err(|error| format!("failed to reach relay: {error}"))?;
        let _: Value = decode_http_json(resp).await?;
        Ok(true)
    }

    pub async fn logout(&self) -> Result<RelayConnectionStatus, String> {
        let previous = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .credentials
            .clone();
        if let Some(creds) = previous {
            let _ = self
                .http
                .post(format!("{}/api/v1/auth/logout", creds.relay_url))
                .bearer_auth(&creds.session_token)
                .send()
                .await;
        }
        self.apply_credentials(None)?;
        Ok(self.status())
    }

    pub async fn list_devices(&self) -> Result<Vec<RelayDeviceInfo>, String> {
        let creds = self.require_credentials()?;
        list_relay_devices_with_client(&self.http, &creds.relay_url, &creds.session_token).await
    }

    pub async fn revoke_device(&self, device_id: &str) -> Result<bool, String> {
        let creds = self.require_credentials()?;
        let resp = self
            .http
            .delete(format!("{}/api/v1/devices/{device_id}", creds.relay_url))
            .bearer_auth(&creds.session_token)
            .send()
            .await
            .map_err(|error| format!("failed to reach relay: {error}"))?;
        let _: Value = decode_http_json(resp).await?;
        if creds.device_id == device_id {
            self.apply_credentials(None)?;
        }
        Ok(true)
    }

    fn require_credentials(&self) -> Result<RelayCredentials, String> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .credentials
            .clone()
            .ok_or_else(|| "not signed in to a relay account".to_string())
    }

    async fn supervisor_loop(self, rpc_ctx: RpcContext) {
        let mut backoff = Duration::from_millis(500);
        loop {
            let (creds, generation) = {
                let inner = self
                    .inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                (inner.credentials.clone(), inner.generation)
            };
            let Some(creds) = creds else {
                self.notify.notified().await;
                backoff = Duration::from_millis(500);
                continue;
            };

            let outcome = self.run_tunnel_once(&creds, generation, &rpc_ctx).await;
            let current_gen = {
                let mut inner = self
                    .inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                inner.connected = false;
                if let Err(ref error) = outcome
                    && inner.generation == generation
                {
                    inner.last_error = Some(error.clone());
                }
                inner.generation
            };
            if current_gen != generation {
                backoff = Duration::from_millis(500);
                continue;
            }
            match outcome {
                Ok(()) => {
                    backoff = Duration::from_millis(500);
                }
                Err(error) => {
                    tracing::debug!(%error, "relay tunnel disconnected; scheduling reconnect");
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = self.notify.notified() => {
                    backoff = Duration::from_millis(500);
                    continue;
                }
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    async fn run_tunnel_once(
        &self,
        creds: &RelayCredentials,
        generation: u64,
        rpc_ctx: &RpcContext,
    ) -> Result<(), String> {
        let ws_url = relay_ws_endpoint(&creds.relay_url, "/ws/daemon")?;
        let mut request = ws_url
            .into_client_request()
            .map_err(|error| error.to_string())?;
        let auth = HeaderValue::from_str(&format!("Bearer {}", creds.device_token))
            .map_err(|error| error.to_string())?;
        request.headers_mut().insert(AUTHORIZATION, auth);

        let (websocket, _) = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| "timed out connecting to pig-relay".to_string())?
        .map_err(|error| format!("relay connect failed: {error}"))?;

        let (mut sink, mut stream) = websocket.split();
        let (frame_tx, frame_rx) =
            async_channel::bounded::<RelayDaemonFrame>(TUNNEL_OUTBOUND_CAPACITY);

        let initial_workspaces = self.current_workspaces();
        frame_tx
            .send(RelayDaemonFrame::Register {
                protocol: RELAY_PROTOCOL_VERSION,
                device_id: creds.device_id.clone(),
                name: creds.device_name.clone(),
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                workspaces: initial_workspaces,
                daemon_protocol: DAEMON_PROTOCOL_VERSION,
            })
            .await
            .map_err(|error| error.to_string())?;

        let mut writer_task = tokio::spawn(async move {
            while let Ok(frame) = frame_rx.recv().await {
                let Ok(encoded) = serde_json::to_string(&frame) else {
                    continue;
                };
                if let Err(error) = sink.send(WsMessage::text(encoded)).await {
                    return Err(error.to_string());
                }
            }
            let _ = sink.close().await;
            Ok::<(), String>(())
        });

        let mut streams: HashMap<String, StreamEntry> = HashMap::new();
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        heartbeat.tick().await;

        let result = loop {
            tokio::select! {
                _ = self.notify.notified() => {
                    let current_gen = self
                        .inner
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .generation;
                    if current_gen != generation {
                        break Ok(());
                    }
                }
                _ = heartbeat.tick() => {
                    let workspaces = self.current_workspaces();
                    if frame_tx.send(RelayDaemonFrame::Heartbeat { workspaces }).await.is_err() {
                        break Err("relay tunnel writer closed".to_string());
                    }
                }
                writer_res = &mut writer_task => {
                    match writer_res {
                        Ok(Ok(())) => break Ok(()),
                        Ok(Err(error)) => break Err(error),
                        Err(error) => break Err(error.to_string()),
                    }
                }
                next = stream.next() => {
                    let Some(message) = next else {
                        break Err("relay tunnel closed by server".to_string());
                    };
                    let message = match message {
                        Ok(message) => message,
                        Err(error) => break Err(error.to_string()),
                    };
                    let text = match message {
                        WsMessage::Text(text) => text.to_string(),
                        WsMessage::Close(_) => break Err("relay closed tunnel".to_string()),
                        WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
                        _ => continue,
                    };
                    let frame: RelayServerFrame = match serde_json::from_str(&text) {
                        Ok(frame) => frame,
                        Err(error) => {
                            tracing::warn!(%error, "ignoring malformed relay server frame");
                            continue;
                        }
                    };
                    match frame {
                        RelayServerFrame::Registered { .. } => {
                            let mut inner = self
                                .inner
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if inner.generation == generation {
                                inner.connected = true;
                                inner.last_error = None;
                            }
                        }
                        RelayServerFrame::ClientConnected { stream_id, .. } => {
                            if let Some(old) = streams.remove(&stream_id) {
                                if let Some(sub_id) = old.subscriber_id {
                                    self.state
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                                        .unsubscribe(sub_id);
                                }
                                old.forward_task.abort();
                            }
                            let entry = spawn_stream_forwarder(stream_id.clone(), frame_tx.clone());
                            streams.insert(stream_id, entry);
                        }
                        RelayServerFrame::ClientRequest { stream_id, request, .. } => {
                            let entry = streams
                                .entry(stream_id.clone())
                                .or_insert_with(|| spawn_stream_forwarder(stream_id.clone(), frame_tx.clone()));
                            match request {
                                DaemonRequest::Subscribe { after, session_ids } => {
                                    let mut guard = self
                                        .state
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                                    if let Some(old_id) = entry.subscriber_id.take() {
                                        guard.unsubscribe(old_id);
                                    }
                                    let id = guard.subscribe(after, session_ids, entry.outgoing.clone());
                                    entry.subscriber_id = Some(id);
                                }
                                other => {
                                    let sub_id = match entry.subscriber_id {
                                        Some(id) => id,
                                        None => {
                                            let id = self
                                                .state
                                                .lock()
                                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                                .subscribe(None, None, entry.outgoing.clone());
                                            entry.subscriber_id = Some(id);
                                            id
                                        }
                                    };
                                    if let Err(error) = dispatch_client_request(
                                        other,
                                        sub_id,
                                        &entry.outgoing,
                                        &self.agent,
                                        &self.state,
                                        rpc_ctx,
                                    )
                                    .await
                                    {
                                        let _ = entry
                                            .outgoing
                                            .try_send(DaemonMessage::Error { detail: error });
                                    }
                                }
                            }
                        }
                        RelayServerFrame::ClientDisconnected { stream_id } => {
                            if let Some(entry) = streams.remove(&stream_id) {
                                if let Some(sub_id) = entry.subscriber_id {
                                    self.state
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                                        .unsubscribe(sub_id);
                                }
                                entry.forward_task.abort();
                            }
                        }
                        RelayServerFrame::Error { detail } => {
                            let mut inner = self
                                .inner
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if inner.generation == generation {
                                inner.last_error = Some(detail);
                            }
                        }
                    }
                }
            }
        };

        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for (_, entry) in streams.drain() {
                if let Some(sub_id) = entry.subscriber_id {
                    guard.unsubscribe(sub_id);
                }
                entry.forward_task.abort();
            }
        }
        frame_tx.close();
        writer_task.abort();
        result
    }

    fn current_workspaces(&self) -> Vec<String> {
        let mut paths = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .workspace_paths();
        if paths.is_empty() {
            paths.push(self.default_cwd.display().to_string());
        }
        paths
    }
}

fn spawn_stream_forwarder(
    stream_id: String,
    frame_tx: async_channel::Sender<RelayDaemonFrame>,
) -> StreamEntry {
    let (outgoing, rx) = async_channel::bounded::<DaemonMessage>(SUBSCRIBER_CHANNEL_CAPACITY);
    let forward_task = tokio::spawn(async move {
        while let Ok(message) = rx.recv().await {
            if frame_tx
                .send(RelayDaemonFrame::ClientMessage {
                    stream_id: stream_id.clone(),
                    message,
                    e2e: None,
                })
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = frame_tx
            .try_send(RelayDaemonFrame::ClientClosed {
                stream_id,
                reason: None,
            });
    });
    StreamEntry {
        subscriber_id: None,
        outgoing,
        forward_task,
    }
}

struct StreamEntry {
    subscriber_id: Option<u64>,
    outgoing: async_channel::Sender<DaemonMessage>,
    forward_task: tokio::task::JoinHandle<()>,
}

pub async fn list_relay_devices(
    relay_url: &str,
    session_token: &str,
) -> Result<Vec<RelayDeviceInfo>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    list_relay_devices_with_client(&client, relay_url, session_token).await
}

async fn list_relay_devices_with_client(
    http: &reqwest::Client,
    relay_url: &str,
    session_token: &str,
) -> Result<Vec<RelayDeviceInfo>, String> {
    #[derive(Deserialize)]
    struct DevicesResp {
        devices: Vec<RelayDeviceInfo>,
    }
    let base = normalize_relay_http_base(relay_url)?;
    let resp = http
        .get(format!("{base}/api/v1/devices"))
        .bearer_auth(session_token)
        .send()
        .await
        .map_err(|error| format!("failed to reach relay: {error}"))?;
    let parsed: DevicesResp = decode_http_json(resp).await?;
    Ok(parsed.devices)
}

async fn decode_http_json<T: for<'de> Deserialize<'de>>(
    resp: reqwest::Response,
) -> Result<T, String> {
    let status = resp.status();
    let text = resp.text().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        if let Ok(value) = serde_json::from_str::<Value>(&text)
            && let Some(err) = value.get("error").and_then(Value::as_str)
        {
            return Err(err.to_string());
        }
        return Err(format!("relay returned HTTP {}: {}", status.as_u16(), text));
    }
    serde_json::from_str(&text).map_err(|error| format!("invalid relay JSON response: {error}"))
}
