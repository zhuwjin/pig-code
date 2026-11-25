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
            DaemonMessage::Subscription { .. } => {}
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
