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

fn test_dir(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "pig-core-{name}-{}-{nanos}-{sequence}",
        std::process::id()
    ))
}

#[allow(dead_code)]
pub fn native_sandbox_usable() -> bool {
    #[cfg(windows)]
    {
        return pig_core::task::sandbox_readiness().usable();
    }
    #[cfg(not(windows))]
    true
}

#[allow(dead_code)]
pub fn native_sandbox_refusal(output: &str) -> bool {
    output.contains("process security environment")
        || output.contains("cannot be sandboxed")
        || output.contains("sandbox helper")
}

#[allow(dead_code)]
pub async fn assert_native_sandbox_rejection() {
    let (config_path, cwd, data_dir) = setup("native-sandbox-rejection");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let session_id = new_session(&agent, cwd).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: mock::SCENARIO_SLOW_TRIGGER.to_string(),
            files: vec![],
            images: vec![],
            permissions: pig_protocol::ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .expect("send sandbox probe");
    recv_until(&agent.events, Duration::from_secs(30), |event| {
        matches!(event, Event::ToolCallEnd { output, .. } if native_sandbox_refusal(output))
    })
    .await;
    assert!(!native_sandbox_usable());
    agent.shutdown();
}

#[allow(dead_code)]
pub fn outside_dir(name: &str) -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .expect("home for outside-workspace test fixture");
    home.join(".pigcode-test-tmp").join(
        test_dir(name)
            .file_name()
            .expect("test directory name")
            .to_string_lossy()
            .into_owned(),
    )
}

#[allow(dead_code)]
pub fn setup(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let port = mock::start_mock_server();
    let dir = test_dir(name);
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
    let dir = test_dir(name);
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
    let deadline = Instant::now() + Duration::from_secs(30);
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
