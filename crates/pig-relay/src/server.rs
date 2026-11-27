use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt as _, StreamExt as _};
use http_body_util::{BodyExt as _, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{
    AUTHORIZATION, CONNECTION, CONTENT_TYPE, HeaderValue, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY,
    UPGRADE,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::OnUpgrade;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use pig_protocol::{
    BotSyncPayload, DaemonMessage, DaemonRequest, DeviceCodePollResponse, DeviceCodeStartResponse,
    RELAY_PROTOCOL_VERSION, RelayAuthResponse, RelayDaemonFrame, RelayDeviceInfo, RelayServerFrame,
    RelayUser,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{RwLock, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::Role;
use uuid::Uuid;

use crate::auth::compute_websocket_accept;
use crate::store::{DeviceCodeStatus, RelayStore, now_ms};

const WEB_INDEX_HTML: &str = include_str!("../web/index.html");
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_BOT_SYNC_BODY_BYTES: usize = 4 * 1024 * 1024;
const CLIENT_STREAM_CAPACITY: usize = 256;
const DAEMON_TUNNEL_CAPACITY: usize = 512;

#[derive(Clone, Debug)]
pub struct RelayServerConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub public_url: Option<String>,
    pub allow_registration: bool,
}

#[derive(Clone)]
struct OnlineDaemon {
    conn_id: String,
    user_id: String,
    tx: async_channel::Sender<RelayServerFrame>,
    clients: Arc<RwLock<HashMap<String, async_channel::Sender<DaemonMessage>>>>,
}

#[derive(Default)]
struct RelayHub {
    daemons: RwLock<HashMap<String, OnlineDaemon>>,
}

impl RelayHub {
    async fn online_device_ids(&self) -> HashSet<String> {
        self.daemons.read().await.keys().cloned().collect()
    }

    async fn register_daemon(
        &self,
        device_id: String,
        user_id: String,
    ) -> (
        String,
        async_channel::Receiver<RelayServerFrame>,
        Arc<RwLock<HashMap<String, async_channel::Sender<DaemonMessage>>>>,
    ) {
        let conn_id = Uuid::new_v4().to_string();
        let (tx, rx) = async_channel::bounded(DAEMON_TUNNEL_CAPACITY);
        let clients = Arc::new(RwLock::new(HashMap::new()));
        let previous = {
            let mut guard = self.daemons.write().await;
            guard.insert(
                device_id,
                OnlineDaemon {
                    conn_id: conn_id.clone(),
                    user_id,
                    tx,
                    clients: Arc::clone(&clients),
                },
            )
        };
        if let Some(old) = previous {
            old.tx.close();
            let mut old_clients = old.clients.write().await;
            for (_, client_tx) in old_clients.drain() {
                client_tx.close();
            }
        }
        (conn_id, rx, clients)
    }

    async fn unregister_daemon(&self, device_id: &str, conn_id: &str) {
        let removed = {
            let mut guard = self.daemons.write().await;
            if guard
                .get(device_id)
                .is_some_and(|entry| entry.conn_id == conn_id)
            {
                guard.remove(device_id)
            } else {
                None
            }
        };
        if let Some(entry) = removed {
            entry.tx.close();
            let mut clients = entry.clients.write().await;
            for (_, client_tx) in clients.drain() {
                let _ = client_tx.try_send(DaemonMessage::Error {
                    detail: "target device disconnected".to_string(),
                });
                client_tx.close();
            }
        }
    }

    async fn disconnect_device(&self, device_id: &str) {
        let removed = self.daemons.write().await.remove(device_id);
        if let Some(entry) = removed {
            entry.tx.close();
            let mut clients = entry.clients.write().await;
            for (_, client_tx) in clients.drain() {
                let _ = client_tx.try_send(DaemonMessage::Error {
                    detail: "device access revoked".to_string(),
                });
                client_tx.close();
            }
        }
    }

    async fn get_daemon(&self, device_id: &str) -> Option<OnlineDaemon> {
        self.daemons.read().await.get(device_id).cloned()
    }
}

struct RelayState {
    config: RelayServerConfig,
    store: Mutex<RelayStore>,
    hub: RelayHub,
}

pub async fn serve(config: RelayServerConfig) -> Result<(), String> {
    serve_with_ready(config, None).await
}

pub async fn serve_with_ready(
    config: RelayServerConfig,
    ready: Option<oneshot::Sender<SocketAddr>>,
) -> Result<(), String> {
    let store = RelayStore::open(&config.data_dir)?;
    let listener = TcpListener::bind(config.bind)
        .await
        .map_err(|error| error.to_string())?;
    let local_addr = listener.local_addr().map_err(|error| error.to_string())?;
    let state = Arc::new(RelayState {
        config,
        store: Mutex::new(store),
        hub: RelayHub::default(),
    });
    if let Some(tx) = ready {
        let _ = tx.send(local_addr);
    }

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|error| error.to_string())?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let state = Arc::clone(&state);
                async move { Ok::<_, Infallible>(handle_http(state, req).await) }
            });
            if let Err(error) = http1::Builder::new()
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                tracing::debug!("relay HTTP connection closed: {error}");
            }
        });
    }
}

async fn handle_http(state: Arc<RelayState>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if method == Method::GET && path == "/ws/daemon" {
        return handle_daemon_ws_upgrade(state, req).await;
    }
    if method == Method::GET
        && let Some(device_id) = path.strip_prefix("/ws/client/")
        && !device_id.is_empty()
        && !device_id.contains('/')
    {
        return handle_client_ws_upgrade(state, req, device_id.to_string()).await;
    }

    match (method, path.as_str()) {
        (Method::GET, "/health") => json_response(
            StatusCode::OK,
            &serde_json::json!({
                "ok": true,
                "protocol": RELAY_PROTOCOL_VERSION,
                "time_ms": now_ms(),
            }),
        ),
        (Method::GET, "/") | (Method::GET, "/device") => html_response(WEB_INDEX_HTML),
        (Method::POST, "/api/v1/auth/register") => handle_register(state, req).await,
        (Method::POST, "/api/v1/auth/login") => handle_login(state, req).await,
        (Method::POST, "/api/v1/auth/logout") => handle_logout(state, req).await,
        (Method::GET, "/api/v1/auth/me") => handle_me(state, req).await,
        (Method::POST, "/api/v1/auth/device/start") => handle_device_start(state, req).await,
        (Method::POST, "/api/v1/auth/device/approve") => handle_device_approve(state, req).await,
        (Method::POST, "/api/v1/auth/device/poll") => handle_device_poll(state, req).await,
        (Method::GET, "/api/v1/devices") => handle_list_devices(state, req).await,
        (Method::GET, "/api/v1/bots/sync") => handle_get_bots_sync(state, req).await,
        (Method::POST, "/api/v1/bots/sync") => handle_post_bots_sync(state, req).await,
        (Method::DELETE, p) if p.starts_with("/api/v1/devices/") => {
            let device_id = p.trim_start_matches("/api/v1/devices/").to_string();
            handle_revoke_device(state, req, device_id).await
        }
        _ => error_response(StatusCode::NOT_FOUND, "not found"),
    }
}

#[derive(Deserialize)]
struct AuthCredentialsPayload {
    username: String,
    password: String,
    #[serde(default)]
    device_name: Option<String>,
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    arch: Option<String>,
    #[serde(default)]
    device_id: Option<String>,
}

async fn handle_register(state: Arc<RelayState>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    if !state.config.allow_registration {
        return error_response(
            StatusCode::FORBIDDEN,
            "account registration is disabled on this relay",
        );
    }
    let payload: AuthCredentialsPayload = match read_json(req).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut store = state.store.lock().expect("relay store lock");
    let user = match store.register_user(&payload.username, &payload.password) {
        Ok(user) => user,
        Err(detail) => return error_response(StatusCode::BAD_REQUEST, &detail),
    };
    finish_auth_response(&mut store, user, payload)
}

async fn handle_login(state: Arc<RelayState>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let payload: AuthCredentialsPayload = match read_json(req).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut store = state.store.lock().expect("relay store lock");
    let user = match store.authenticate_user(&payload.username, &payload.password) {
        Ok(user) => user,
        Err(detail) => return error_response(StatusCode::UNAUTHORIZED, &detail),
    };
    finish_auth_response(&mut store, user, payload)
}

fn finish_auth_response(
    store: &mut RelayStore,
    user: RelayUser,
    payload: AuthCredentialsPayload,
) -> Response<Full<Bytes>> {
    let label = payload
        .device_name
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("client");
    let session_token = match store.issue_session_token(&user.user_id, label) {
        Ok(token) => token,
        Err(detail) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail),
    };
    let (device_id, device_token) = if let Some(device_name) = payload
        .device_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
    {
        match store.register_device(
            &user.user_id,
            payload.device_id.as_deref(),
            device_name,
            payload.os.as_deref().unwrap_or("unknown"),
            payload.arch.as_deref().unwrap_or("unknown"),
        ) {
            Ok((id, token)) => (Some(id), Some(token)),
            Err(detail) => return error_response(StatusCode::BAD_REQUEST, &detail),
        }
    } else {
        (None, None)
    };
    json_response(
        StatusCode::OK,
        &RelayAuthResponse {
            user,
            session_token,
            device_id,
            device_token,
        },
    )
}

async fn handle_logout(state: Arc<RelayState>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let Some(token) = bearer_token(&req) else {
        return error_response(StatusCode::UNAUTHORIZED, "missing bearer token");
    };
    let mut store = state.store.lock().expect("relay store lock");
    match store.revoke_session_token(&token) {
        Ok(_) => json_response(StatusCode::OK, &serde_json::json!({ "ok": true })),
        Err(detail) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail),
    }
}

async fn handle_me(state: Arc<RelayState>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let user = match authenticate_session(&state, &req) {
        Ok(user) => user,
        Err(response) => return response,
    };
    json_response(StatusCode::OK, &user)
}

#[derive(Deserialize)]
struct DeviceCodeStartPayload {
    device_name: String,
    #[serde(default)]
    os: String,
    #[serde(default)]
    arch: String,
}

async fn handle_device_start(
    state: Arc<RelayState>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let base_url = infer_public_url(&state.config, &req);
    let payload: DeviceCodeStartPayload = match read_json(req).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let os = if payload.os.trim().is_empty() {
        "unknown"
    } else {
        payload.os.trim()
    };
    let arch = if payload.arch.trim().is_empty() {
        "unknown"
    } else {
        payload.arch.trim()
    };
    let challenge = {
        let mut store = state.store.lock().expect("relay store lock");
        match store.start_device_code(&payload.device_name, os, arch) {
            Ok(challenge) => challenge,
            Err(detail) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail),
        }
    };
    let verification_uri = format!(
        "{}/device?code={}",
        base_url.trim_end_matches('/'),
        challenge.user_code
    );
    let expires_in_secs = challenge
        .expires_at_ms
        .saturating_sub(now_ms())
        .div_ceil(1000);
    json_response(
        StatusCode::OK,
        &DeviceCodeStartResponse {
            device_code: challenge.device_code,
            user_code: challenge.user_code,
            verification_uri,
            expires_in_secs,
            interval_secs: 2,
        },
    )
}

#[derive(Deserialize)]
struct DeviceCodeApprovePayload {
    user_code: String,
}

async fn handle_device_approve(
    state: Arc<RelayState>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let user = match authenticate_session(&state, &req) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let payload: DeviceCodeApprovePayload = match read_json(req).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut store = state.store.lock().expect("relay store lock");
    match store.approve_device_code(&user.user_id, &payload.user_code) {
        Ok(()) => json_response(StatusCode::OK, &serde_json::json!({ "ok": true })),
        Err(detail) => error_response(StatusCode::BAD_REQUEST, &detail),
    }
}

#[derive(Deserialize)]
struct DeviceCodePollPayload {
    device_code: String,
}

async fn handle_device_poll(
    state: Arc<RelayState>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let payload: DeviceCodePollPayload = match read_json(req).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let status = {
        let mut store = state.store.lock().expect("relay store lock");
        match store.poll_device_code(&payload.device_code) {
            Ok(status) => status,
            Err(detail) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail),
        }
    };
    let body = match status {
        DeviceCodeStatus::Pending => DeviceCodePollResponse::Pending,
        DeviceCodeStatus::Expired => DeviceCodePollResponse::Expired,
        DeviceCodeStatus::Approved {
            user,
            session_token,
            device_id,
            device_token,
        } => DeviceCodePollResponse::Approved {
            user,
            session_token,
            device_id,
            device_token,
        },
    };
    json_response(StatusCode::OK, &body)
}

#[derive(Serialize)]
struct DeviceListResponse {
    devices: Vec<RelayDeviceInfo>,
}

async fn handle_list_devices(
    state: Arc<RelayState>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let user = match authenticate_session(&state, &req) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let online_ids = state.hub.online_device_ids().await;
    let devices = {
        let store = state.store.lock().expect("relay store lock");
        match store.list_user_devices(&user.user_id, &online_ids) {
            Ok(list) => list,
            Err(detail) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail),
        }
    };
    json_response(StatusCode::OK, &DeviceListResponse { devices })
}

async fn handle_revoke_device(
    state: Arc<RelayState>,
    req: Request<Incoming>,
    device_id: String,
) -> Response<Full<Bytes>> {
    let user = match authenticate_session(&state, &req) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let revoked = {
        let mut store = state.store.lock().expect("relay store lock");
        match store.revoke_device(&user.user_id, &device_id) {
            Ok(changed) => changed,
            Err(detail) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail),
        }
    };
    if !revoked {
        return error_response(StatusCode::NOT_FOUND, "device not found");
    }
    state.hub.disconnect_device(&device_id).await;
    json_response(StatusCode::OK, &serde_json::json!({ "ok": true }))
}

async fn handle_get_bots_sync(
    state: Arc<RelayState>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let user_id = match authenticate_user_or_device(&state, &req) {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let store = state.store.lock().expect("relay store lock");
    match store.get_bot_sync(&user_id) {
        Ok(payload) => json_response(StatusCode::OK, &payload),
        Err(detail) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail),
    }
}

async fn handle_post_bots_sync(
    state: Arc<RelayState>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let user_id = match authenticate_user_or_device(&state, &req) {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let incoming: BotSyncPayload = match read_json_with_limit(req, MAX_BOT_SYNC_BODY_BYTES).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let mut store = state.store.lock().expect("relay store lock");
    match store.merge_bot_sync(&user_id, &incoming) {
        Ok(merged) => json_response(StatusCode::OK, &merged),
        Err(detail) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail),
    }
}

async fn handle_daemon_ws_upgrade(
    state: Arc<RelayState>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let Some(token) = bearer_token(&req).or_else(|| query_param(&req, "token")) else {
        return error_response(StatusCode::UNAUTHORIZED, "missing device token");
    };
    let record = {
        let store = state.store.lock().expect("relay store lock");
        match store.verify_device_token(&token) {
            Ok(Some(record)) => record,
            Ok(None) => return error_response(StatusCode::UNAUTHORIZED, "invalid device token"),
            Err(detail) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail),
        }
    };
    let (response, on_upgrade) = match prepare_ws_upgrade(req) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    tokio::spawn(async move {
        if let Err(error) = run_daemon_tunnel(state, on_upgrade, record.device_id, record.user_id).await
        {
            tracing::debug!("daemon tunnel ended: {error}");
        }
    });
    response
}

async fn run_daemon_tunnel(
    state: Arc<RelayState>,
    on_upgrade: OnUpgrade,
    device_id: String,
    user_id: String,
) -> Result<(), String> {
    let upgraded = on_upgrade.await.map_err(|error| error.to_string())?;
    let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
    let (mut sink, mut stream) = ws.split();
    let (conn_id, server_rx, clients) = state
        .hub
        .register_daemon(device_id.clone(), user_id)
        .await;

    let writer = tokio::spawn(async move {
        while let Ok(frame) = server_rx.recv().await {
            let Ok(text) = serde_json::to_string(&frame) else {
                continue;
            };
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    while let Some(next) = stream.next().await {
        let Ok(message) = next else {
            break;
        };
        let text = match message {
            WsMessage::Text(text) => text.to_string(),
            WsMessage::Close(_) => break,
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            _ => continue,
        };
        let frame: RelayDaemonFrame = match serde_json::from_str(&text) {
            Ok(frame) => frame,
            Err(error) => {
                if let Some(daemon) = state.hub.get_daemon(&device_id).await {
                    let _ = daemon
                        .tx
                        .try_send(RelayServerFrame::Error {
                            detail: format!("invalid relay frame: {error}"),
                        });
                }
                continue;
            }
        };
        match frame {
            RelayDaemonFrame::Register {
                name,
                os,
                arch,
                workspaces,
                daemon_protocol,
                ..
            } => {
                {
                    let mut store = state.store.lock().expect("relay store lock");
                    let _ = store.update_device_presence(
                        &device_id,
                        Some(&name),
                        Some(&os),
                        Some(&arch),
                        &workspaces,
                        Some(daemon_protocol),
                    );
                }
                if let Some(daemon) = state.hub.get_daemon(&device_id).await {
                    let _ = daemon
                        .tx
                        .send(RelayServerFrame::Registered {
                            device_id: device_id.clone(),
                            server_time_ms: now_ms(),
                        })
                        .await;
                }
            }
            RelayDaemonFrame::Heartbeat { workspaces } => {
                let mut store = state.store.lock().expect("relay store lock");
                let _ = store.update_device_presence(
                    &device_id,
                    None,
                    None,
                    None,
                    &workspaces,
                    None,
                );
            }
            RelayDaemonFrame::ClientMessage {
                stream_id,
                message,
                ..
            } => {
                let target = clients.read().await.get(&stream_id).cloned();
                if let Some(client_tx) = target
                    && client_tx.send(message).await.is_err()
                {
                    clients.write().await.remove(&stream_id);
                }
            }
            RelayDaemonFrame::ClientClosed { stream_id, .. } => {
                if let Some(client_tx) = clients.write().await.remove(&stream_id) {
                    client_tx.close();
                }
            }
        }
    }

    state.hub.unregister_daemon(&device_id, &conn_id).await;
    writer.abort();
    Ok(())
}

async fn handle_client_ws_upgrade(
    state: Arc<RelayState>,
    req: Request<Incoming>,
    device_id: String,
) -> Response<Full<Bytes>> {
    let user = match authenticate_session(&state, &req) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let allowed = {
        let store = state.store.lock().expect("relay store lock");
        match store.device_belongs_to_user(&device_id, &user.user_id) {
            Ok(allowed) => allowed,
            Err(detail) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail),
        }
    };
    if !allowed {
        return error_response(StatusCode::FORBIDDEN, "device not found for account");
    }
    let Some(daemon) = state.hub.get_daemon(&device_id).await else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "target device is offline");
    };
    if daemon.user_id != user.user_id {
        return error_response(StatusCode::FORBIDDEN, "device belongs to another account");
    }
    let client_label = Some(user.username.clone());
    let (response, on_upgrade) = match prepare_ws_upgrade(req) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    tokio::spawn(async move {
        if let Err(error) = run_client_proxy(on_upgrade, daemon, client_label).await {
            tracing::debug!("relay client proxy ended: {error}");
        }
    });
    response
}

async fn run_client_proxy(
    on_upgrade: OnUpgrade,
    daemon: OnlineDaemon,
    client_label: Option<String>,
) -> Result<(), String> {
    let upgraded = on_upgrade.await.map_err(|error| error.to_string())?;
    let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
    let (mut sink, mut stream) = ws.split();

    let stream_id = Uuid::new_v4().to_string();
    let (client_tx, client_rx) = async_channel::bounded::<DaemonMessage>(CLIENT_STREAM_CAPACITY);
    daemon
        .clients
        .write()
        .await
        .insert(stream_id.clone(), client_tx);
    if daemon
        .tx
        .send(RelayServerFrame::ClientConnected {
            stream_id: stream_id.clone(),
            client_label,
        })
        .await
        .is_err()
    {
        daemon.clients.write().await.remove(&stream_id);
        let _ = sink
            .send(WsMessage::Text(
                serde_json::to_string(&DaemonMessage::Error {
                    detail: "target device disconnected".to_string(),
                })
                .unwrap_or_default()
                .into(),
            ))
            .await;
        return Ok(());
    }

    let writer = tokio::spawn(async move {
        while let Ok(message) = client_rx.recv().await {
            let Ok(text) = serde_json::to_string(&message) else {
                continue;
            };
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    while let Some(next) = stream.next().await {
        let Ok(message) = next else {
            break;
        };
        let text = match message {
            WsMessage::Text(text) => text.to_string(),
            WsMessage::Close(_) => break,
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            _ => continue,
        };
        let request: DaemonRequest = match serde_json::from_str(&text) {
            Ok(request) => request,
            Err(error) => {
                if let Some(tx) = daemon.clients.read().await.get(&stream_id).cloned() {
                    let _ = tx
                        .send(DaemonMessage::Error {
                            detail: format!("invalid request: {error}"),
                        })
                        .await;
                }
                continue;
            }
        };
        if daemon
            .tx
            .send(RelayServerFrame::ClientRequest {
                stream_id: stream_id.clone(),
                request,
                e2e: None,
            })
            .await
            .is_err()
        {
            break;
        }
    }

    daemon.clients.write().await.remove(&stream_id);
    let _ = daemon
        .tx
        .send(RelayServerFrame::ClientDisconnected { stream_id })
        .await;
    writer.abort();
    Ok(())
}

fn prepare_ws_upgrade(
    req: Request<Incoming>,
) -> Result<(Response<Full<Bytes>>, OnUpgrade), Response<Full<Bytes>>> {
    let key = req
        .headers()
        .get(SEC_WEBSOCKET_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| error_response(StatusCode::BAD_REQUEST, "missing Sec-WebSocket-Key"))?;
    let accept = compute_websocket_accept(&key);
    let on_upgrade = hyper::upgrade::on(req);
    let response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(UPGRADE, HeaderValue::from_static("websocket"))
        .header(CONNECTION, HeaderValue::from_static("Upgrade"))
        .header(SEC_WEBSOCKET_ACCEPT, accept)
        .body(Full::new(Bytes::new()))
        .expect("build websocket upgrade response");
    Ok((response, on_upgrade))
}

fn authenticate_session(
    state: &RelayState,
    req: &Request<Incoming>,
) -> Result<RelayUser, Response<Full<Bytes>>> {
    let Some(token) = bearer_token(req).or_else(|| query_param(req, "token")) else {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "missing session token",
        ));
    };
    let store = state.store.lock().expect("relay store lock");
    match store.verify_session_token(&token) {
        Ok(Some(user)) => Ok(user),
        Ok(None) => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid or revoked session token",
        )),
        Err(detail) => Err(error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail)),
    }
}

fn authenticate_user_or_device(
    state: &RelayState,
    req: &Request<Incoming>,
) -> Result<String, Response<Full<Bytes>>> {
    let Some(token) = bearer_token(req).or_else(|| query_param(req, "token")) else {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "missing bearer token",
        ));
    };
    let store = state.store.lock().expect("relay store lock");
    match store.resolve_user_id_from_token(&token) {
        Ok(Some(user_id)) => Ok(user_id),
        Ok(None) => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid or revoked token",
        )),
        Err(detail) => Err(error_response(StatusCode::INTERNAL_SERVER_ERROR, &detail)),
    }
}

fn bearer_token(req: &Request<Incoming>) -> Option<String> {
    req.headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn query_param(req: &Request<Incoming>, key: &str) -> Option<String> {
    let query = req.uri().query()?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

fn infer_public_url(config: &RelayServerConfig, req: &Request<Incoming>) -> String {
    if let Some(url) = &config.public_url {
        return url.clone();
    }
    if let Some(host) = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|value| value.to_str().ok())
    {
        return format!("http://{host}");
    }
    format!("http://{}", config.bind)
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    req: Request<Incoming>,
) -> Result<T, Response<Full<Bytes>>> {
    read_json_with_limit(req, MAX_BODY_BYTES).await
}

async fn read_json_with_limit<T: for<'de> Deserialize<'de>>(
    req: Request<Incoming>,
    max_bytes: usize,
) -> Result<T, Response<Full<Bytes>>> {
    let collected = req
        .into_body()
        .collect()
        .await
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, &error.to_string()))?;
    let bytes = collected.to_bytes();
    if bytes.len() > max_bytes {
        return Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request payload too large",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| error_response(StatusCode::BAD_REQUEST, &format!("invalid JSON: {error}")))
}

fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response<Full<Bytes>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .body(Full::new(Bytes::from(body)))
        .expect("build json response")
}

fn html_response(html: &'static str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )
        .body(Full::new(Bytes::from_static(html.as_bytes())))
        .expect("build html response")
}

fn error_response(status: StatusCode, detail: &str) -> Response<Full<Bytes>> {
    json_response(status, &serde_json::json!({ "error": detail }))
}
