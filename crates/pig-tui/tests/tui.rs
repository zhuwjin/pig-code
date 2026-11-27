use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pig_daemon::{
    DaemonClient, DaemonConfig, DaemonEndpoint, DaemonMessage, serve, snapshot_events,
};
use pig_protocol::{ConversationItem, Event, Op, SessionStateSnapshot};
use pig_provider::mock;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

struct DaemonGuard {
    task: tokio::task::JoinHandle<Result<(), String>>,
    root: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct TuiGuard {
    child: Box<dyn portable_pty::Child>,
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn setup() -> (PathBuf, PathBuf, PathBuf) {
    let port = mock::start_mock_server();
    let root = std::env::temp_dir().join(format!(
        "pig-tui-test-{}-{}",
        std::process::id(),
        pig_core::model_io::now_ms()
    ));
    let workspace = root.join("workspace");
    let data_dir = root.join("data");
    std::fs::create_dir_all(&workspace).expect("create TUI test workspace");
    std::fs::write(
        workspace.join(mock::MOCK_FILE_NAME),
        mock::MOCK_FILE_CONTENT,
    )
    .expect("write TUI mock fixture");
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
    .expect("write TUI test config");
    (root, workspace, data_dir)
}

async fn endpoint(data_dir: &Path) -> DaemonEndpoint {
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
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn until_event(client: &DaemonClient, mut predicate: impl FnMut(&Event) -> bool) -> Event {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for TUI event");
        let Ok(message) = tokio::time::timeout(Duration::from_secs(1), client.recv()).await else {
            continue;
        };
        match message.expect("observer event stream closed") {
            DaemonMessage::Batch(batch) => {
                if let Some(event) = batch.events.into_iter().find(&mut predicate) {
                    return event;
                }
            }
            DaemonMessage::Snapshot(snapshot) => {
                if let Some(event) = snapshot_events(&snapshot).into_iter().find(&mut predicate) {
                    return event;
                }
            }
            DaemonMessage::Subscription { .. }
            | DaemonMessage::RpcResult { .. }
            | DaemonMessage::PluginEvent { .. }
            | DaemonMessage::TerminalOutput { .. }
            | DaemonMessage::TerminalExit { .. } => {}
            DaemonMessage::Error { detail } => panic!("daemon error: {detail}"),
        }
    }
}

async fn until_snapshot(client: &DaemonClient) -> SessionStateSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for TUI snapshot"
        );
        let Ok(message) = tokio::time::timeout(Duration::from_secs(1), client.recv()).await else {
            continue;
        };
        if let DaemonMessage::Snapshot(snapshot) = message.expect("observer snapshot closed") {
            return snapshot;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tui_shares_a_session_and_detaches_without_stopping_core() {
    let (root, workspace, data_dir) = setup();
    let config = root.join("config.toml");
    let task = tokio::spawn(serve(DaemonConfig {
        data_dir: data_dir.clone(),
        cwd: workspace.clone(),
        config_path: Some(config.clone()),
        selftest: true,
    }));
    let _daemon = DaemonGuard { task, root };
    let endpoint = endpoint(&data_dir).await;
    let observer = DaemonClient::connect(endpoint, None)
        .await
        .expect("connect observer");

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open TUI test PTY");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_pig-tui"));
    command.arg("--data-dir");
    command.arg(data_dir.as_os_str());
    command.arg("--cwd");
    command.arg(workspace.as_os_str());
    command.arg("--config");
    command.arg(config.as_os_str());
    command.arg("--new");
    command.cwd(&workspace);
    command.env("TERM", "xterm-256color");
    let child = pair
        .slave
        .spawn_command(command)
        .expect("spawn real pig-tui");
    let mut tui = TuiGuard { child };
    let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
    let mut writer = pair.master.take_writer().expect("take PTY writer");
    let output = Arc::new(Mutex::new(String::new()));
    let output_reader = output.clone();
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            output_reader
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push_str(&String::from_utf8_lossy(&buffer[..count]));
        }
    });

    let configured = until_event(&observer, |event| {
        matches!(event, Event::SessionConfigured { .. })
    })
    .await;
    let session_id = match configured {
        Event::SessionConfigured { session_id, .. } => session_id,
        _ => unreachable!(),
    };
    writer.write_all(b"SCENARIO_Q\r").unwrap();
    writer.flush().unwrap();
    let requested = until_event(&observer, |event| {
        matches!(event, Event::QuestionRequested { .. })
    })
    .await;
    let request_id = match requested {
        Event::QuestionRequested { request_id, .. } => request_id,
        _ => unreachable!(),
    };
    assert!(
        matches!(tui.child.try_wait(), Ok(None)),
        "TUI exited early: {}",
        output.lock().unwrap()
    );
    writer
        .write_all(format!("/answer {request_id} Option A | Yes\r").as_bytes())
        .unwrap();
    writer.flush().unwrap();
    until_event(&observer, |event| {
        matches!(
            event,
            Event::QuestionResolved { request_id: resolved, .. } if resolved == &request_id
        )
    })
    .await;

    writer.write_all(b"/quit\r").unwrap();
    writer.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = tui.child.try_wait().expect("inspect TUI process") {
            assert!(status.success(), "TUI failed: {}", output.lock().unwrap());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "TUI did not detach: {}",
            output.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!_daemon.task.is_finished());
    observer
        .send_op(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .unwrap();
    let restored = until_snapshot(&observer).await;
    assert_eq!(restored.session_id, session_id);
    assert!(restored.items.iter().any(|item| matches!(
        item,
        ConversationItem::User { text, .. } if text == mock::SCENARIO_Q_TRIGGER
    )));
}
