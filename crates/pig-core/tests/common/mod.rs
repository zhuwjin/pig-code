use std::path::PathBuf;
use std::time::{Duration, Instant};

use pig_core::mock;
use pig_protocol::{Event, Op};

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
            "[provider]
base_url = \"http://127.0.0.1:{port}/v1\"
api_key = \"mock-key\"
model = \"mock-model\"
"
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    (config_path, dir, data_dir)
}

#[allow(dead_code)]
pub async fn new_session(agent: &pig_core::AgentHandle, cwd: PathBuf) -> String {
    agent.ops.send(Op::NewSession { cwd }).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), agent.events.recv()).await
        else {
            assert!(Instant::now() < deadline, "等待 SessionConfigured 超时");
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
    panic!("等待事件超时（{}s），已收到: {collected:#?}", deadline.as_secs());
}
