use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use pig_protocol::{EventCursor, Op};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::{DAEMON_PROTOCOL_VERSION, DaemonEndpoint, DaemonMessage, DaemonRequest};

#[derive(Clone, Debug)]
pub struct DaemonConnectConfig {
    pub data_dir: PathBuf,
    pub cwd: PathBuf,
    pub config_path: Option<PathBuf>,
}

#[derive(Clone)]
pub struct DaemonClient {
    pub ops: async_channel::Sender<Op>,
    pub messages: async_channel::Receiver<DaemonMessage>,
    cursor: std::sync::Arc<Mutex<Option<EventCursor>>>,
}

impl DaemonClient {
    pub fn send_op(&self, op: Op) -> Result<(), String> {
        self.ops.try_send(op).map_err(|error| error.to_string())
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
        let subscribe = serde_json::to_string(&DaemonRequest::Subscribe { after })
            .map_err(|error| error.to_string())?;
        websocket
            .send(Message::text(subscribe))
            .await
            .map_err(|error| error.to_string())?;
        Ok(spawn_io(websocket))
    }

    pub fn connect_or_spawn(config: DaemonConnectConfig) -> Result<Self, String> {
        let endpoint = discover_or_spawn(&config)?;
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        client_runtime().spawn(async move {
            let result = Self::connect(endpoint, None).await;
            let _ = result_tx.send(result);
        });
        result_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| "timed out initializing pig-daemon client".to_string())?
    }
}

fn spawn_io(websocket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>) -> DaemonClient {
    let (ops, op_receiver) = async_channel::bounded::<Op>(256);
    let (message_sender, messages) = async_channel::bounded::<DaemonMessage>(256);
    let cursor = std::sync::Arc::new(Mutex::new(None));
    let task_cursor = cursor.clone();
    tokio::spawn(async move {
        let (mut writer, mut reader) = websocket.split();
        let result = loop {
            tokio::select! {
                operation = op_receiver.recv() => {
                    match operation {
                        Ok(op) => {
                            let encoded = match serde_json::to_string(&DaemonRequest::Op { op }) {
                                Ok(encoded) => encoded,
                                Err(error) => break Err(error.to_string()),
                            };
                            if let Err(error) = writer.send(Message::text(encoded)).await {
                                break Err(error.to_string());
                            }
                        }
                        Err(_) => break Ok(()),
                    }
                }
                message = reader.next() => {
                    match message {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<DaemonMessage>(&text) {
                                Ok(message) => {
                                    match &message {
                                        DaemonMessage::Subscription { cursor: next, .. } => {
                                            *task_cursor.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(next.clone());
                                        }
                                        DaemonMessage::Batch(batch) => {
                                            *task_cursor.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(batch.cursor());
                                        }
                                        DaemonMessage::Snapshot(snapshot) => {
                                            *task_cursor.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(snapshot.cursor.clone());
                                        }
                                        DaemonMessage::Error { .. } => {}
                                    }
                                    if message_sender.send(message).await.is_err() {
                                        break Ok(());
                                    }
                                }
                                Err(error) => break Err(error.to_string()),
                            }
                        }
                        Some(Ok(Message::Binary(bytes))) => {
                            match serde_json::from_slice::<DaemonMessage>(&bytes) {
                                Ok(message) => {
                                    if message_sender.send(message).await.is_err() {
                                        break Ok(());
                                    }
                                }
                                Err(error) => break Err(error.to_string()),
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            if let Err(error) = writer.send(Message::Pong(payload)).await {
                                break Err(error.to_string());
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break Ok(()),
                        Some(Ok(_)) => {}
                        Some(Err(error)) => break Err(error.to_string()),
                    }
                }
            }
        };
        if let Err(error) = result {
            let _ = message_sender
                .send(DaemonMessage::Error { detail: error })
                .await;
        }
        message_sender.close();
    });
    DaemonClient {
        ops,
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
    command
        .spawn()
        .map_err(|error| format!("failed to launch pig-daemon: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(endpoint) = read_endpoint(&config.data_dir)
            && Some(&endpoint.token) != previous_token.as_ref()
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
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .ok_or_else(|| {
            format!(
                "pig-daemon executable not found (checked {})",
                candidates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}
