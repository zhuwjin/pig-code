use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use pig_protocol::{EventCursor, Op};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::{DAEMON_PROTOCOL_VERSION, DaemonEndpoint, DaemonMessage, DaemonRequest};

type DaemonSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Debug)]
pub struct DaemonConnectConfig {
    pub data_dir: PathBuf,
    pub cwd: PathBuf,
    pub config_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct RemoteConnectConfig {
    pub relay_url: String,
    pub session_token: String,
    pub device_id: String,
}

#[derive(Clone)]
enum ReconnectTarget {
    Local(DaemonConnectConfig),
    Remote(RemoteConnectConfig),
}

#[derive(Clone)]
pub struct DaemonClient {
    pub ops: async_channel::Sender<Op>,
    requests: Option<async_channel::Sender<DaemonRequest>>,
    pub messages: async_channel::Receiver<DaemonMessage>,
    cursor: Arc<Mutex<Option<EventCursor>>>,
}

impl DaemonClient {
    pub fn from_channels(
        ops: async_channel::Sender<Op>,
        messages: async_channel::Receiver<DaemonMessage>,
    ) -> Self {
        Self {
            ops,
            requests: None,
            messages,
            cursor: Arc::new(Mutex::new(None)),
        }
    }

    pub fn send_op(&self, op: Op) -> Result<(), String> {
        self.ops.try_send(op).map_err(|error| error.to_string())
    }

    pub fn send_rpc(
        &self,
        id: impl Into<String>,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Result<(), String> {
        let sender = self
            .requests
            .as_ref()
            .ok_or_else(|| "RPC channel is unavailable on channel-backed client".to_string())?;
        sender
            .try_send(DaemonRequest::Rpc {
                id: id.into(),
                method: method.into(),
                params,
            })
            .map_err(|error| error.to_string())
    }

    pub async fn recv(&self) -> Result<DaemonMessage, async_channel::RecvError> {
        self.messages.recv().await
    }

    pub fn cursor(&self) -> Option<EventCursor> {
        self.cursor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub async fn connect(
        endpoint: DaemonEndpoint,
        after: Option<EventCursor>,
    ) -> Result<Self, String> {
        Self::connect_with_filter(endpoint, after, None).await
    }

    pub async fn connect_with_filter(
        endpoint: DaemonEndpoint,
        after: Option<EventCursor>,
        session_ids: Option<Vec<String>>,
    ) -> Result<Self, String> {
        let websocket = open_websocket(&endpoint, after, session_ids.clone()).await?;
        Ok(spawn_io(websocket, None, session_ids))
    }

    pub async fn connect_remote(
        remote: RemoteConnectConfig,
        after: Option<EventCursor>,
        session_ids: Option<Vec<String>>,
    ) -> Result<Self, String> {
        let websocket = open_remote_websocket(&remote, after, session_ids.clone()).await?;
        Ok(spawn_io(websocket, None, session_ids))
    }

    pub async fn connect_remote_managed(
        remote: RemoteConnectConfig,
        after: Option<EventCursor>,
        session_ids: Option<Vec<String>>,
    ) -> Result<Self, String> {
        let websocket = open_remote_websocket(&remote, after, session_ids.clone()).await?;
        Ok(spawn_io(
            websocket,
            Some(ReconnectTarget::Remote(remote)),
            session_ids,
        ))
    }

    pub fn connect_or_spawn(config: DaemonConnectConfig) -> Result<Self, String> {
        let endpoint = discover_or_spawn(&config)?;
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let reconnect_config = config.clone();
        client_runtime().spawn(async move {
            let result = open_websocket(&endpoint, None, None).await.map(|websocket| {
                spawn_io(
                    websocket,
                    Some(ReconnectTarget::Local(reconnect_config)),
                    None,
                )
            });
            let _ = result_tx.send(result);
        });
        result_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| "timed out initializing pig-daemon client".to_string())?
    }
}

async fn open_remote_websocket(
    remote: &RemoteConnectConfig,
    after: Option<EventCursor>,
    session_ids: Option<Vec<String>>,
) -> Result<DaemonSocket, String> {
    let url = crate::relay::relay_ws_endpoint(
        &remote.relay_url,
        &format!("/ws/client/{}", remote.device_id),
    )?;
    let mut request = url
        .into_client_request()
        .map_err(|error| error.to_string())?;
    let authorization = HeaderValue::from_str(&format!("Bearer {}", remote.session_token))
        .map_err(|error| error.to_string())?;
    request.headers_mut().insert(AUTHORIZATION, authorization);
    let (mut websocket, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| "timed out connecting to pig-relay".to_string())?
    .map_err(|error| error.to_string())?;
    let subscribe = serde_json::to_string(&DaemonRequest::Subscribe { after, session_ids })
        .map_err(|error| error.to_string())?;
    websocket
        .send(Message::text(subscribe))
        .await
        .map_err(|error| error.to_string())?;
    Ok(websocket)
}

async fn open_websocket(
    endpoint: &DaemonEndpoint,
    after: Option<EventCursor>,
    session_ids: Option<Vec<String>>,
) -> Result<DaemonSocket, String> {
    if endpoint.protocol != DAEMON_PROTOCOL_VERSION {
        return Err(format!(
            "unsupported daemon protocol {} (expected {})",
            endpoint.protocol, DAEMON_PROTOCOL_VERSION
        ));
    }
    let url = format!("ws://{}/", endpoint.address);
    let mut request = url
        .into_client_request()
        .map_err(|error| error.to_string())?;
    let authorization = HeaderValue::from_str(&format!("Bearer {}", endpoint.token))
        .map_err(|error| error.to_string())?;
    request.headers_mut().insert(AUTHORIZATION, authorization);
    let (mut websocket, _) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| "timed out connecting to pig-daemon".to_string())?
    .map_err(|error| error.to_string())?;
    let subscribe = serde_json::to_string(&DaemonRequest::Subscribe { after, session_ids })
        .map_err(|error| error.to_string())?;
    websocket
        .send(Message::text(subscribe))
        .await
        .map_err(|error| error.to_string())?;
    Ok(websocket)
}

fn record_cursor(cursor: &Mutex<Option<EventCursor>>, message: &DaemonMessage) {
    let next = match message {
        DaemonMessage::Subscription { cursor, .. } => Some(cursor.clone()),
        DaemonMessage::Batch(batch) => Some(batch.cursor()),
        DaemonMessage::Snapshot(snapshot) => Some(snapshot.cursor.clone()),
        DaemonMessage::RpcResult { .. }
        | DaemonMessage::PluginEvent { .. }
        | DaemonMessage::TerminalOutput { .. }
        | DaemonMessage::TerminalExit { .. }
        | DaemonMessage::Error { .. } => None,
    };
    if let Some(next) = next {
        *cursor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(next);
    }
}

enum SessionOutcome {
    Shutdown,
    Disconnected {
        reason: String,
        unsent_request: Option<DaemonRequest>,
    },
}

async fn drive_socket(
    websocket: DaemonSocket,
    op_receiver: &async_channel::Receiver<Op>,
    request_receiver: &async_channel::Receiver<DaemonRequest>,
    message_sender: &async_channel::Sender<DaemonMessage>,
    task_cursor: &Mutex<Option<EventCursor>>,
    mut pending_request: Option<DaemonRequest>,
) -> SessionOutcome {
    let (mut writer, mut reader) = websocket.split();
    if let Some(request) = pending_request.take() {
        let encoded = match serde_json::to_string(&request) {
            Ok(encoded) => encoded,
            Err(error) => {
                return SessionOutcome::Disconnected {
                    reason: error.to_string(),
                    unsent_request: None,
                };
            }
        };
        if let Err(error) = writer.send(Message::text(encoded)).await {
            return SessionOutcome::Disconnected {
                reason: error.to_string(),
                unsent_request: Some(request),
            };
        }
    }
    loop {
        tokio::select! {
            operation = op_receiver.recv() => {
                match operation {
                    Ok(op) => {
                        let req = DaemonRequest::Op { op };
                        let encoded = match serde_json::to_string(&req) {
                            Ok(encoded) => encoded,
                            Err(error) => {
                                return SessionOutcome::Disconnected {
                                    reason: error.to_string(),
                                    unsent_request: None,
                                };
                            }
                        };
                        if let Err(error) = writer.send(Message::text(encoded)).await {
                            return SessionOutcome::Disconnected {
                                reason: error.to_string(),
                                unsent_request: Some(req),
                            };
                        }
                    }
                    Err(_) => return SessionOutcome::Shutdown,
                }
            }
            request = request_receiver.recv() => {
                match request {
                    Ok(req) => {
                        let encoded = match serde_json::to_string(&req) {
                            Ok(encoded) => encoded,
                            Err(error) => {
                                return SessionOutcome::Disconnected {
                                    reason: error.to_string(),
                                    unsent_request: None,
                                };
                            }
                        };
                        if let Err(error) = writer.send(Message::text(encoded)).await {
                            return SessionOutcome::Disconnected {
                                reason: error.to_string(),
                                unsent_request: Some(req),
                            };
                        }
                    }
                    Err(_) => return SessionOutcome::Shutdown,
                }
            }
            message = reader.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<DaemonMessage>(&text) {
                            Ok(message) => {
                                record_cursor(task_cursor, &message);
                                if message_sender.send(message).await.is_err() {
                                    return SessionOutcome::Shutdown;
                                }
                            }
                            Err(error) => {
                                return SessionOutcome::Disconnected {
                                    reason: error.to_string(),
                                    unsent_request: None,
                                };
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        match serde_json::from_slice::<DaemonMessage>(&bytes) {
                            Ok(message) => {
                                record_cursor(task_cursor, &message);
                                if message_sender.send(message).await.is_err() {
                                    return SessionOutcome::Shutdown;
                                }
                            }
                            Err(error) => {
                                return SessionOutcome::Disconnected {
                                    reason: error.to_string(),
                                    unsent_request: None,
                                };
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if let Err(error) = writer.send(Message::Pong(payload)).await {
                            return SessionOutcome::Disconnected {
                                reason: error.to_string(),
                                unsent_request: None,
                            };
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        return SessionOutcome::Disconnected {
                            reason: "daemon connection closed".to_string(),
                            unsent_request: None,
                        };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        return SessionOutcome::Disconnected {
                            reason: error.to_string(),
                            unsent_request: None,
                        };
                    }
                }
            }
        }
    }
}

async fn reconnect_socket(
    target: &ReconnectTarget,
    cursor: Option<EventCursor>,
    session_ids: Option<Vec<String>>,
) -> Result<DaemonSocket, String> {
    const BACKOFF_MS: [u64; 5] = [100, 250, 500, 1000, 1500];
    let mut last_error = "daemon connection closed".to_string();
    for delay_ms in BACKOFF_MS {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        match target {
            ReconnectTarget::Local(config) => {
                let config_clone = config.clone();
                let endpoint = match tokio::task::spawn_blocking(move || {
                    discover_or_spawn(&config_clone)
                })
                .await
                {
                    Ok(Ok(endpoint)) => endpoint,
                    Ok(Err(error)) => {
                        last_error = error;
                        continue;
                    }
                    Err(error) => {
                        last_error = error.to_string();
                        continue;
                    }
                };
                match open_websocket(&endpoint, cursor.clone(), session_ids.clone()).await {
                    Ok(socket) => return Ok(socket),
                    Err(error) => last_error = error,
                }
            }
            ReconnectTarget::Remote(remote) => {
                match open_remote_websocket(remote, cursor.clone(), session_ids.clone()).await {
                    Ok(socket) => return Ok(socket),
                    Err(error) => last_error = error,
                }
            }
        }
    }
    Err(last_error)
}

fn spawn_io(
    mut websocket: DaemonSocket,
    reconnect_target: Option<ReconnectTarget>,
    session_ids: Option<Vec<String>>,
) -> DaemonClient {
    let (ops, op_receiver) = async_channel::bounded::<Op>(256);
    let (requests, request_receiver) = async_channel::bounded::<DaemonRequest>(256);
    let (message_sender, messages) = async_channel::bounded::<DaemonMessage>(256);
    let cursor = Arc::new(Mutex::new(None));
    let task_cursor = cursor.clone();
    tokio::spawn(async move {
        let mut pending_request = None;
        loop {
            match drive_socket(
                websocket,
                &op_receiver,
                &request_receiver,
                &message_sender,
                &task_cursor,
                pending_request.take(),
            )
            .await
            {
                SessionOutcome::Shutdown => break,
                SessionOutcome::Disconnected {
                    reason,
                    unsent_request,
                } => {
                    let Some(target) = reconnect_target.as_ref() else {
                        if reason != "daemon connection closed" {
                            let _ = message_sender
                                .send(DaemonMessage::Error { detail: reason })
                                .await;
                        }
                        break;
                    };
                    if op_receiver.is_closed() || message_sender.is_closed() {
                        break;
                    }
                    let resume_cursor = task_cursor
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    match reconnect_socket(target, resume_cursor, session_ids.clone()).await {
                        Ok(next_socket) => {
                            websocket = next_socket;
                            pending_request = unsent_request;
                        }
                        Err(error) => {
                            let _ = message_sender
                                .send(DaemonMessage::Error {
                                    detail: format!(
                                        "lost pig-daemon connection ({reason}); reconnect failed: {error}"
                                    ),
                                })
                                .await;
                            break;
                        }
                    }
                }
            }
        }
        message_sender.close();
    });
    DaemonClient {
        ops,
        requests: Some(requests),
        messages,
        cursor,
    }
}

fn client_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("daemon client runtime")
    })
}

fn endpoint_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("daemon").join("endpoint.json")
}

fn read_endpoint(data_dir: &std::path::Path) -> Option<DaemonEndpoint> {
    let bytes = std::fs::read(endpoint_path(data_dir)).ok()?;
    let endpoint: DaemonEndpoint = serde_json::from_slice(&bytes).ok()?;
    (endpoint.protocol == DAEMON_PROTOCOL_VERSION).then_some(endpoint)
}

fn probe(address: SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&address, Duration::from_millis(150)).is_ok()
}

fn discover_or_spawn(config: &DaemonConnectConfig) -> Result<DaemonEndpoint, String> {
    if let Some(endpoint) = read_endpoint(&config.data_dir)
        && probe(endpoint.address)
    {
        return Ok(endpoint);
    }
    let previous_token = read_endpoint(&config.data_dir).map(|endpoint| endpoint.token);
    let binary = daemon_binary()?;
    let mut command = std::process::Command::new(binary);
    command
        .arg(format!("--data-dir={}", config.data_dir.display()))
        .arg(format!("--cwd={}", config.cwd.display()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(config_path) = &config.config_path {
        command.arg(format!("--config={}", config_path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to launch pig-daemon: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let child_exited = matches!(child.try_wait(), Ok(Some(_)));
        if let Some(endpoint) = read_endpoint(&config.data_dir)
            && (Some(&endpoint.token) != previous_token.as_ref() || child_exited)
            && probe(endpoint.address)
        {
            return Ok(endpoint);
        }
        if Instant::now() >= deadline {
            return Err("pig-daemon did not publish a live endpoint".to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn daemon_binary() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("PIG_DAEMON_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("PIG_DAEMON_BIN {} is not a file", path.display()));
    }
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let name = if cfg!(windows) {
        "pig-daemon.exe"
    } else {
        "pig-daemon"
    };
    let mut candidates = Vec::new();
    if let Some(directory) = executable.parent() {
        candidates.push(directory.join(name));
        if let Some(parent) = directory.parent() {
            candidates.push(parent.join(name));
        }
    }
    if let Some(found) = candidates.iter().find(|candidate| candidate.is_file()) {
        return Ok(found.clone());
    }
    if let Some(path_env) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path_env) {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "pig-daemon executable not found (checked {} and PATH)",
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}
