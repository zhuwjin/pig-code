use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pig_daemon::{DaemonClient, DaemonEndpoint, DaemonMessage, snapshot_events};
use pig_protocol::{
    ApprovalDecision, ConversationItem, ConversationToolStatus, Event, EventCursor, ExecMode,
    MessageDelivery, Op, SessionStateSnapshot,
};
use pig_provider::mock;

struct DaemonProcess {
    root: PathBuf,
    child: std::process::Child,
}

impl DaemonProcess {
    fn spawn() -> (Self, PathBuf, PathBuf) {
        let port = mock::start_mock_server();
        let root = std::env::temp_dir().join(format!(
            "pig-daemon-test-{}-{}",
            std::process::id(),
            pig_core::model_io::now_ms()
        ));
        let workspace = root.join("workspace");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&workspace).expect("create daemon test workspace");
        std::fs::write(
            workspace.join(mock::MOCK_FILE_NAME),
            mock::MOCK_FILE_CONTENT,
        )
        .expect("write daemon mock fixture");
        let config = root.join("config.toml");
        std::fs::write(
            &config,
            format!(
                r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
"#
            ),
        )
        .expect("write daemon test config");
        let child = daemon_command()
            .arg(format!("--data-dir={}", data_dir.display()))
            .arg(format!("--cwd={}", workspace.display()))
            .arg(format!("--config={}", config.display()))
            .env("PIG_SELFTEST", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn pig-daemon");
        let process = Self { root, child };
        (process, workspace, data_dir)
    }

    fn endpoint(&self, data_dir: &Path) -> DaemonEndpoint {
        let path = data_dir.join("daemon").join("endpoint.json");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(endpoint) = serde_json::from_slice::<DaemonEndpoint>(&bytes)
            {
                return endpoint;
            }
            assert!(
                Instant::now() < deadline,
                "daemon endpoint was not published"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn daemon_command() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_pig-daemon"))
}

struct Harness {
    name: &'static str,
    client: DaemonClient,
    events: VecDeque<(Event, EventCursor)>,
    snapshots: VecDeque<SessionStateSnapshot>,
    seen: HashSet<(String, u64)>,
}

impl Harness {
    async fn connect(endpoint: &DaemonEndpoint, name: &'static str) -> Self {
        let client = DaemonClient::connect(endpoint.clone(), None)
            .await
            .expect("connect authenticated daemon client");
        Self {
            name,
            client,
            events: VecDeque::new(),
            snapshots: VecDeque::new(),
            seen: HashSet::new(),
        }
    }

    fn receive(&mut self, message: DaemonMessage) {
        if std::env::var_os("PIG_DAEMON_TEST_TRACE").is_some() {
            eprintln!("{}: {message:?}", self.name);
        }
        match message {
            DaemonMessage::Subscription { .. }
            | DaemonMessage::RpcResult { .. }
            | DaemonMessage::PluginEvent { .. }
            | DaemonMessage::TerminalOutput { .. }
            | DaemonMessage::TerminalExit { .. } => {}
            DaemonMessage::Batch(batch) => {
                assert!(
                    self.seen.insert((batch.epoch.clone(), batch.sequence)),
                    "duplicate daemon cursor {}",
                    batch.sequence
                );
                let cursor = batch.cursor();
                self.events.extend(
                    batch
                        .events
                        .into_iter()
                        .map(|event| (event, cursor.clone())),
                );
            }
            DaemonMessage::Snapshot(snapshot) => {
                let cursor = snapshot.cursor.clone();
                self.events.extend(
                    snapshot_events(&snapshot)
                        .into_iter()
                        .map(|event| (event, cursor.clone())),
                );
                self.snapshots.push_back(snapshot);
            }
            DaemonMessage::Error { detail } => panic!("daemon error: {detail}"),
        }
    }

    async fn until_event(
        &mut self,
        mut predicate: impl FnMut(&Event) -> bool,
    ) -> (Event, EventCursor) {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            if let Some(index) = self.events.iter().position(|(event, _)| predicate(event)) {
                return self.events.remove(index).expect("matching event");
            }
            assert!(
                Instant::now() < deadline,
                "{} timed out waiting for event",
                self.name
            );
            let Ok(message) =
                tokio::time::timeout(Duration::from_secs(1), self.client.recv()).await
            else {
                continue;
            };
            self.receive(message.expect("daemon event stream closed"));
        }
    }

    async fn until_snapshot(&mut self) -> SessionStateSnapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(snapshot) = self.snapshots.pop_front() {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "timed out waiting for snapshot");
            let Ok(message) =
                tokio::time::timeout(Duration::from_secs(1), self.client.recv()).await
            else {
                continue;
            };
            self.receive(message.expect("daemon snapshot stream closed"));
        }
    }
}

fn new_session(cwd: &Path) -> Op {
    Op::NewSession {
        cwd: cwd.to_path_buf(),
        provider_id: None,
        model_id: None,
        reasoning_level: None,
        permissions: Some(ExecMode::Auto.permissions()),
        plan_enabled: Some(false),
    }
}

fn send(session_id: &str, content: &str) -> Op {
    Op::SendMessage {
        session_id: session_id.to_string(),
        content: content.to_string(),
        files: Vec::new(),
        images: Vec::new(),
        permissions: ExecMode::Auto.permissions(),
        delivery: MessageDelivery::Queue,
    }
}

fn configured_id(event: &Event) -> Option<String> {
    match event {
        Event::SessionConfigured { session_id, .. } => Some(session_id.clone()),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_client_attach_resolution_and_background_survival() {
    if !pig_core::task::platform_sandbox_status().supported {
        eprintln!("skipping daemon background test: native sandbox unavailable");
        return;
    }
    let (mut daemon, workspace, data_dir) = DaemonProcess::spawn();
    let endpoint = daemon.endpoint(&data_dir);

    let second = daemon_command()
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg(format!("--cwd={}", workspace.display()))
        .env("PIG_SELFTEST", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn competing daemon");
    let mut second = second;
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = second.try_wait().expect("inspect competing daemon") {
            break status;
        }
        assert!(Instant::now() < deadline, "second daemon did not exit");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(!status.success(), "a second writer must be rejected");

    let mut invalid = endpoint.clone();
    invalid.token = "invalid-token".to_string();
    assert!(DaemonClient::connect(invalid, None).await.is_err());

    let mut a = Harness::connect(&endpoint, "A").await;
    let mut b = Harness::connect(&endpoint, "B").await;
    a.client.send_op(new_session(&workspace)).unwrap();
    let configured = a
        .until_event(|event| matches!(event, Event::SessionConfigured { .. }))
        .await
        .0;
    let session_id = configured_id(&configured).expect("session id");
    let configured = b
        .until_event(|event| matches!(event, Event::SessionConfigured { .. }))
        .await
        .0;
    let b_id = configured_id(&configured).expect("second client session id");
    assert_eq!(session_id, b_id);

    b.client
        .send_op(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .unwrap();
    let attached = b.until_snapshot().await;
    assert_eq!(attached.session_id, session_id);
    assert!(attached.configuration.is_some());

    a.client
        .send_op(send(&session_id, mock::SCENARIO_Q_TRIGGER))
        .unwrap();
    let (question, _) = a
        .until_event(|event| matches!(event, Event::QuestionRequested { .. }))
        .await;
    let question_id = match question {
        Event::QuestionRequested { request_id, .. } => request_id,
        _ => unreachable!(),
    };
    let b_question = b
        .until_event(|event| matches!(event, Event::QuestionRequested { .. }))
        .await;
    let b_question_id = match b_question.0 {
        Event::QuestionRequested { request_id, .. } => request_id,
        _ => unreachable!(),
    };
    assert_eq!(question_id, b_question_id);

    let mut c = Harness::connect(&endpoint, "C").await;
    c.client
        .send_op(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .unwrap();
    let pending = c.until_snapshot().await;
    assert!(
        pending
            .pending_questions
            .iter()
            .any(|question| question.request_id == question_id)
    );

    a.client
        .send_op(Op::QuestionReply {
            request_id: question_id.clone(),
            answers: Some(vec![vec!["Option A".to_string()], vec!["Yes".to_string()]]),
        })
        .unwrap();
    for client in [&mut b, &mut c] {
        client
            .until_event(|event| {
                matches!(
                    event,
                    Event::QuestionResolved { request_id, .. } if request_id == &question_id
                )
            })
            .await;
    }
    a.until_event(|event| matches!(event, Event::TurnComplete { .. }))
        .await;

    a.client.send_op(new_session(&workspace)).unwrap();
    let configured = a
        .until_event(|event| matches!(event, Event::SessionConfigured { .. }))
        .await
        .0;
    let danger_session = configured_id(&configured).expect("danger session id");
    b.until_event(|event| {
        matches!(
            event,
            Event::SessionConfigured { session_id, .. } if session_id == &danger_session
        )
    })
    .await;
    a.client
        .send_op(send(&danger_session, mock::SCENARIO_DANGER_TRIGGER))
        .unwrap();
    let (approval, _) = a
        .until_event(|event| matches!(event, Event::ApprovalRequested { .. }))
        .await;
    let approval_id = match approval {
        Event::ApprovalRequested { request_id, .. } => request_id,
        _ => unreachable!(),
    };
    b.until_event(|event| matches!(event, Event::ApprovalRequested { .. }))
        .await;
    a.client
        .send_op(Op::ApprovalReply {
            request_id: approval_id.clone(),
            decision: ApprovalDecision::Allow,
            feedback: None,
        })
        .unwrap();
    b.until_event(|event| {
        matches!(
            event,
            Event::ApprovalResolved { request_id, .. } if request_id == &approval_id
        )
    })
    .await;
    a.client
        .send_op(Op::Interrupt {
            session_id: danger_session.clone(),
        })
        .unwrap();
    a.until_event(|event| matches!(event, Event::TurnAborted { .. }))
        .await;

    a.client.send_op(new_session(&workspace)).unwrap();
    let configured = a
        .until_event(|event| matches!(event, Event::SessionConfigured { .. }))
        .await
        .0;
    let slow_session = configured_id(&configured).expect("slow session id");
    b.until_event(|event| {
        matches!(
            event,
            Event::SessionConfigured { session_id, .. } if session_id == &slow_session
        )
    })
    .await;
    a.client
        .send_op(send(&slow_session, mock::SCENARIO_SLOW_TRIGGER))
        .unwrap();
    for client in [&mut a, &mut b] {
        client
            .until_event(|event| {
                matches!(
                    event,
                    Event::ConversationUpdate {
                        item: ConversationItem::Tool {
                            summary,
                            status: ConversationToolStatus::Running,
                            ..
                        },
                        ..
                    } if summary.contains("sleep 30")
                )
            })
            .await;
    }
    drop(a);
    drop(b);
    drop(c);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(daemon.child.try_wait().unwrap().is_none());

    let mut reopened = Harness::connect(&endpoint, "reopened").await;
    reopened
        .client
        .send_op(Op::OpenSession {
            session_id: slow_session.clone(),
        })
        .unwrap();
    let restored = reopened.until_snapshot().await;
    assert!(restored.running);
    assert!(restored.items.iter().any(|item| matches!(
        item,
        ConversationItem::Tool {
            tool,
            summary,
            status: ConversationToolStatus::Running,
            ..
        } if tool == "ExecCommand" && summary.contains("sleep 30")
    )));
    let restored_cursor = restored.cursor.clone();
    reopened
        .client
        .send_op(Op::Interrupt {
            session_id: slow_session.clone(),
        })
        .unwrap();
    let (_, aborted_cursor) = reopened
        .until_event(|event| matches!(event, Event::TurnAborted { .. }))
        .await;
    assert_eq!(aborted_cursor.epoch, restored_cursor.epoch);
    assert!(aborted_cursor.sequence > restored_cursor.sequence);
    drop(reopened);
    assert!(daemon.child.try_wait().unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn websocket_ping_frames_stay_connected_and_client_reconnects_after_daemon_restart() {
    use futures_util::{SinkExt as _, StreamExt as _};
    use pig_daemon::{DaemonConnectConfig, DaemonRequest};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

    let (mut daemon, workspace, data_dir) = DaemonProcess::spawn();
    let endpoint = daemon.endpoint(&data_dir);

    // Raw WebSocket client sends Ping before and after Subscribe and must receive Pong without dropping.
    let url = format!("ws://{}/", endpoint.address);
    let mut request = url.into_client_request().expect("client request");
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", endpoint.token)).expect("auth header"),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("connect raw websocket");
    ws.send(Message::Ping(b"pre".to_vec().into()))
        .await
        .expect("send pre-subscribe ping");
    let pong = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("pong timeout")
        .expect("ws open")
        .expect("pong frame");
    assert!(matches!(pong, Message::Pong(payload) if payload.as_ref() == b"pre"));

    let subscribe = serde_json::to_string(&DaemonRequest::Subscribe {
        after: None,
        session_ids: None,
    })
    .expect("encode subscribe");
    ws.send(Message::text(subscribe))
        .await
        .expect("send subscribe");
    let subscribed = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("subscription timeout")
        .expect("ws open")
        .expect("subscription frame");
    assert!(matches!(subscribed, Message::Text(_)));

    ws.send(Message::Ping(b"post".to_vec().into()))
        .await
        .expect("send post-subscribe ping");
    let pong = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("post pong timeout")
        .expect("ws open")
        .expect("post pong frame");
    assert!(matches!(pong, Message::Pong(payload) if payload.as_ref() == b"post"));
    drop(ws);

    // Managed client via connect_or_spawn automatically reconnects and signals resync after daemon crash.
    let config_path = daemon.root.join("config.toml");
    // Point connect_or_spawn to the compiled pig-daemon binary via PIG_DAEMON_BIN only when needed:
    // here we manually kill and respawn on the same data_dir so reconnect discovers the fresh daemon.
    let client = tokio::task::spawn_blocking({
        let data_dir = data_dir.clone();
        let workspace = workspace.clone();
        let config_path = config_path.clone();
        move || {
            DaemonClient::connect_or_spawn(DaemonConnectConfig {
                data_dir,
                cwd: workspace,
                config_path: Some(config_path),
            })
        }
    })
    .await
    .expect("join connect_or_spawn")
    .expect("connect managed client");

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), client.recv())
            .await
            .expect("initial subscription"),
        Ok(DaemonMessage::Subscription {
            resync_required: false,
            ..
        })
    ));

    // Kill the running daemon and launch a fresh one on the same data_dir.
    let _ = daemon.child.kill();
    let _ = daemon.child.wait();
    let _ = std::fs::remove_file(data_dir.join("daemon").join("endpoint.json"));
    daemon.child = daemon_command()
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg(format!("--cwd={}", workspace.display()))
        .arg(format!("--config={}", config_path.display()))
        .env("PIG_SELFTEST", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("respawn pig-daemon");
    let _ = daemon.endpoint(&data_dir);

    let reconnected = tokio::time::timeout(Duration::from_secs(10), client.recv())
        .await
        .expect("reconnect subscription timeout")
        .expect("client stream remained open across reconnect");
    assert!(matches!(
        reconnected,
        DaemonMessage::Subscription {
            resync_required: true,
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selftest_cli_and_rpc_methods_work_over_websocket() {
    use futures_util::{SinkExt as _, StreamExt as _};
    use pig_daemon::DaemonRequest;
    use std::io::BufRead as _;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

    let mut child = daemon_command()
        .arg("--selftest")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn pig-daemon --selftest");

    let stdout = child.stdout.take().expect("piped stdout");
    let endpoint = tokio::task::spawn_blocking(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read endpoint line");
        serde_json::from_str::<DaemonEndpoint>(line.trim()).expect("parse endpoint json")
    })
    .await
    .expect("join stdout reader");

    let url = format!("ws://{}/", endpoint.address);
    let mut request = url.into_client_request().expect("client request");
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", endpoint.token)).expect("auth header"),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("connect to selftest daemon");
    ws.send(Message::text(
        serde_json::to_string(&DaemonRequest::Subscribe {
            after: None,
            session_ids: None,
        })
        .unwrap(),
    ))
    .await
    .expect("subscribe");

    async fn call_rpc(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        ws.send(Message::text(
            serde_json::to_string(&DaemonRequest::Rpc {
                id: id.to_string(),
                method: method.to_string(),
                params,
            })
            .unwrap(),
        ))
        .await
        .expect("send rpc");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for rpc {method}");
            let frame = tokio::time::timeout(Duration::from_secs(2), ws.next())
                .await
                .expect("ws frame timeout")
                .expect("ws open")
                .expect("ws ok");
            let Message::Text(text) = frame else { continue };
            let message: DaemonMessage = serde_json::from_str(&text).expect("parse daemon msg");
            if let DaemonMessage::RpcResult {
                id: reply_id,
                result,
                error,
            } = message
                && reply_id == id
            {
                assert!(error.is_none(), "rpc {method} failed: {error:?}");
                return result.expect("rpc result");
            }
        }
    }

    let info = call_rpc(&mut ws, "1", "daemon.info", serde_json::json!({})).await;
    assert_eq!(info["selftest"], serde_json::json!(true));
    let cwd = info["cwd"].as_str().expect("cwd string").to_string();
    assert!(Path::new(&cwd).is_dir());

    let dir = call_rpc(
        &mut ws,
        "2",
        "fs.list_directory",
        serde_json::json!({ "path": cwd, "include_files": true }),
    )
    .await;
    assert!(
        dir["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|e| e["name"] == mock::MOCK_FILE_NAME)
    );

    let spans = call_rpc(
        &mut ws,
        "3",
        "code.highlight",
        serde_json::json!({ "code": "fn main() {}", "language": "rs" }),
    )
    .await;
    assert!(!spans.as_array().expect("spans array").is_empty());

    let sample = call_rpc(&mut ws, "4", "system.process_sample", serde_json::json!({})).await;
    assert!(sample["memory"].as_u64().unwrap_or(0) > 0);

    let sandbox = call_rpc(&mut ws, "5", "system.sandbox_status", serde_json::json!({})).await;
    assert!(sandbox["backend"].is_string());

    let term = call_rpc(
        &mut ws,
        "6",
        "terminal.open",
        serde_json::json!({ "cwd": info["cwd"] }),
    )
    .await;
    let terminal_id = term["terminal_id"].as_str().expect("terminal_id").to_string();
    let closed = call_rpc(
        &mut ws,
        "7",
        "terminal.close",
        serde_json::json!({ "terminal_id": terminal_id }),
    )
    .await;
    assert_eq!(closed, serde_json::json!(true));

    let _ = child.kill();
    let _ = child.wait();
}
