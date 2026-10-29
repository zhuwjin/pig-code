//! Workspace memory integration (ZCode's model-managed memory, ported):
//! the system prompt carries usage instructions + the MEMORY.md INDEX (never
//! fact content) gated by the settings switch, and the memory directory is a
//! read-write root for the plain Write tool (approval-free, exercised via
//! the mock's MEMORY_WRITE scenario).

use crate::common;

use common::{new_session, recv_until, setup, setup_with_log};
use pig_core::memory;
use pig_protocol::{Event, ExecMode, Op};
use std::path::Path;
use std::time::Duration;

/// Append a [memory] section to the setup config before the agent spawns
fn enable_memory(config_path: &Path) {
    let mut config = std::fs::read_to_string(config_path).unwrap();
    config.push_str("\n[memory]\nenabled = true\n");
    std::fs::write(config_path, config).unwrap();
}

fn send(
    _events: &async_channel::Receiver<Event>,
    agent: &pig_core::AgentHandle,
    sid: &str,
    text: &str,
) {
    agent
        .ops
        .send_blocking(Op::SendMessage {
            session_id: sid.into(),
            content: text.into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .unwrap();
}

async fn wait_turn(events: &async_channel::Receiver<Event>) -> Vec<Event> {
    recv_until(events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. } | Event::TurnAborted { .. })
    })
    .await
}

/// Seed the workspace's memory index (creates the directory + MEMORY.md)
fn seed_index(data_dir: &Path, cwd: &Path, index: &str) {
    let path = memory::workspace_memory_dir(data_dir, cwd).join("MEMORY.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, index).unwrap();
}

/// Enabled memory: the first model request carries the ZCode-style
/// instructions + the index (pointer lines, never fact content), and no
/// dedicated Remember tool exists (the plain Write tool IS the write path)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_section_carries_instructions_and_index() {
    let (config_path, cwd, data_dir, log) = setup_with_log("mem-inject");
    enable_memory(&config_path);
    seed_index(
        &data_dir,
        &cwd,
        "# Memory\n\n- [Prefers concise replies](prefers-concise.md) — reply style\n",
    );
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;
    send(&events, &agent, &sid, "Read the mock file and summarize");
    wait_turn(&events).await;
    let bodies = log.lock().expect("log lock").clone();
    let first_streaming = bodies
        .iter()
        .find(|body| !body.contains("\"stream\":false"))
        .expect("a streaming conversation request");
    assert!(
        first_streaming.contains("## Workspace memory"),
        "memory section missing from the first request"
    );
    // Instructions (ZCode's wording anchors) + the index pointer line
    assert!(first_streaming.contains("This directory already exists"));
    assert!(first_streaming.contains("type: user | feedback | project | reference"));
    assert!(first_streaming.contains("- [Prefers concise replies](prefers-concise.md)"));
    assert!(first_streaming.contains("MEMORY.md"));
    // No dedicated memory tool: the plain tools are the write path
    assert!(
        !first_streaming.contains("\"name\":\"Remember\""),
        "no Remember tool — the model writes memory with the plain Write tool"
    );
}

/// Disabled memory: no section injected
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_disabled_no_section() {
    let (config_path, cwd, data_dir, log) = setup_with_log("mem-off");
    seed_index(
        &data_dir,
        &cwd,
        "# Memory\n\n- [Prefers concise replies](prefers-concise.md) — reply style\n",
    );
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;
    send(&events, &agent, &sid, "Read the mock file and summarize");
    wait_turn(&events).await;
    let bodies = log.lock().expect("log lock").clone();
    assert!(
        bodies
            .iter()
            .all(|body| !body.contains("## Workspace memory")),
        "disabled memory must not be injected"
    );
}

/// The model-driven write path (ZCode's): the mock Writes a fact file INTO
/// the memory directory (outside the workspace) with the plain Write tool —
/// the RW-root exemption lets it through with no approval popup
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_writes_memory_file_with_plain_write() {
    let (config_path, cwd, data_dir) = setup("mem-write");
    enable_memory(&config_path);
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd.clone()).await;
    let target = memory::workspace_memory_dir(&data_dir, &cwd).join("mock-note.md");
    send(
        &events,
        &agent,
        &sid,
        &format!(
            "{} {}",
            pig_provider::mock::MEMORY_WRITE_TRIGGER,
            target.display()
        ),
    );
    let turn_events = wait_turn(&events).await;
    // No approval may be requested: the memory directory is an RW root
    assert!(
        turn_events
            .iter()
            .all(|e| !matches!(e, Event::ApprovalRequested { .. })),
        "writing into the memory directory must not pop an approval"
    );
    let content = std::fs::read_to_string(&target).expect("the Write landed in the memory dir");
    assert_eq!(content, pig_provider::mock::MOCK_MEMORY_NOTE);
}
