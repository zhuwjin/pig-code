use std::path::PathBuf;
use std::time::{Duration, Instant};

use pig_protocol::{Event, Op};
use pig_provider::mock;

/// Create a real NTFS junction (a directory reparse point). Junctions, unlike
/// symlinks, need no elevation on Windows, so tests can exercise reparse-point
/// resolution as a normal developer account.
#[cfg(windows)]
#[allow(dead_code)]
pub fn junction(link: &std::path::Path, target: &std::path::Path) {
    use std::os::windows::process::CommandExt as _;
    use std::process::Stdio;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run mklink /J");
    assert!(
        status.success(),
        "create junction {} -> {}",
        link.display(),
        target.display()
    );
    assert!(link.exists(), "junction {} is missing", link.display());
}

#[allow(dead_code)]
pub fn setup(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let port = mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-core-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock Provider"
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
    .unwrap();
    let data_dir = dir.join("data");
    (config_path, dir, data_dir)
}

/// setup() + the mock server's request-body log (model-input assertions)
#[allow(dead_code)]
pub fn setup_with_log(
    name: &str,
) -> (
    PathBuf,
    PathBuf,
    PathBuf,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock Provider"
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
    .unwrap();
    let data_dir = dir.join("data");
    (config_path, dir, data_dir, log)
}

#[allow(dead_code)]
pub async fn new_session(agent: &pig_core::AgentHandle, cwd: PathBuf) -> String {
    agent
        .ops
        .send(Op::NewSession {
            cwd,
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            permissions: None,
            plan_enabled: None,
        })
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), agent.events.recv()).await
        else {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for SessionConfigured"
            );
            continue;
        };
        if let Event::SessionConfigured { session_id, .. } = event {
            return session_id;
        }
    }
}

#[allow(dead_code)]
pub async fn recv_until(
    events: &async_channel::Receiver<Event>,
    deadline: Duration,
    mut done: impl FnMut(&Event) -> bool,
) -> Vec<Event> {
    let start = Instant::now();
    let mut collected = Vec::new();
    while start.elapsed() < deadline {
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv()).await;
        let Ok(Ok(event)) = event else { continue };
        let finished = done(&event);
        collected.push(event);
        if finished {
            return collected;
        }
    }
    panic!(
        "Timed out after {}s waiting for event, received: {collected:#?}",
        deadline.as_secs()
    );
}
