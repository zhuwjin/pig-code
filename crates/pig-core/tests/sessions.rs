mod common;

use common::{new_session, recv_until, setup};
use pig_protocol::{Event, ExecMode, Op};
use pig_provider::mock;
use std::time::Duration;

/// resume: rollout persisted → OpenSession on a new in-process agent → history rebuilt, the model receives the prior history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_rebuilds_history() {
    let (config_path, cwd, data_dir) = setup("m4-resume");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Read the mock file and summarize it".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    agent.shutdown();

    // Simulate a restart: new manager on the same data_dir
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events2 = agent2.events.clone();

    agent2.ops.send(Op::ListSessions).await.unwrap();
    let list = recv_until(&events2, Duration::from_secs(5), |e| {
        matches!(e, Event::SessionList { .. })
    })
    .await;
    let Some(Event::SessionList { sessions }) = list.last() else {
        panic!("expected SessionList");
    };
    assert!(
        sessions.iter().any(|s| s.id == session_id
            // Title seeded from the first message, or already replaced by the
            // auto-naming sidecar (either one, depending on timing)
            && (s.title.contains("Read the mock") || s.title == mock::MOCK_TITLE)),
        "index should contain the session with a title from the first message or auto-naming: {sessions:?}"
    );

    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let replay = recv_until(&events2, Duration::from_secs(10), |e| {
        matches!(e, Event::TextDone { full_text, .. } if full_text.contains(mock::MOCK_REPLY_MARKER))
    })
    .await;
    assert!(
        replay.iter().any(
            |e| matches!(e, Event::UserMessage { text, .. } if text.contains("Read the mock"))
        ),
        "replay should include the user message"
    );
    assert!(
        replay
            .iter()
            .any(|e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "Read")),
        "replay should include a tool call"
    );

    // The replay ends with a TurnComplete of duration_ms=0; drain replay events first to avoid interfering with the recv_until below
    while tokio::time::timeout(Duration::from_millis(200), events2.recv())
        .await
        .is_ok()
    {}

    // Continue the conversation: the model should receive the rebuilt history (system+user+assistant+tool_call+tool_result+new user = 6 entries)
    agent2
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "ECHO_HISTORY report the message count".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    let events = recv_until(&events2, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let count = events.iter().find_map(|e| match e {
        Event::TextDone { full_text, .. } => full_text
            .strip_prefix("HISTORY_COUNT:")
            .and_then(|n| n.parse::<usize>().ok()),
        _ => None,
    });
    assert_eq!(
        count,
        Some(6),
        "history should be fully rebuilt after resume: {events:#?}"
    );
    agent2.shutdown();
}

/// Context watermark restored after resume: takes the used of the last
/// step_usage (142), not the turn total from turn_stats (60+10 + 100+42 = 212).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_restores_context_watermark() {
    let (config_path, cwd, data_dir) = setup("m4-watermark");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Read the mock file and summarize it".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    agent.shutdown();

    // Every step with usage should persist one step_usage record (the default scenario has 2 steps: 70 and 142)
    let rollout_path = data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let content = std::fs::read_to_string(&rollout_path).unwrap();
    assert_eq!(
        content.matches("\"type\":\"step_usage\"").count(),
        2,
        "each step's usage should get its own record: {content}"
    );

    // Simulate a restart: OpenSession replay should re-emit the watermark, taking used from the last step_usage
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let replay = recv_until(&events2, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextUsage { .. })
    })
    .await;
    assert!(
        replay
            .iter()
            .any(|e| matches!(e, Event::ContextUsage { used: 142, .. })),
        "watermark after replay should be the last step's 142, not the turn total 212: {replay:#?}"
    );
    agent2.shutdown();
}

/// Pin/archive on the sessions table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pin_and_archive_update_index() {
    let (config_path, cwd, data_dir) = setup("m4-meta");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::UpdateSessionMeta {
            session_id: session_id.clone(),
            pinned: Some(true),
            archived: None,
            title: Some("Important task".into()),
        })
        .await
        .unwrap();
    let collected = recv_until(
        &events,
        Duration::from_secs(5),
        |e| matches!(e, Event::SessionList { sessions, .. } if sessions.iter().any(|s| s.pinned)),
    )
    .await;
    let Some(Event::SessionList { sessions }) = collected.last() else {
        panic!()
    };
    let meta = sessions.iter().find(|s| s.id == session_id).unwrap();
    assert!(meta.pinned && !meta.archived && meta.title == "Important task");

    agent
        .ops
        .send(Op::UpdateSessionMeta {
            session_id: session_id.clone(),
            pinned: Some(false),
            archived: Some(true),
            title: None,
        })
        .await
        .unwrap();
    let collected = recv_until(
        &events,
        Duration::from_secs(5),
        |e| matches!(e, Event::SessionList { sessions, .. } if sessions.iter().any(|s| s.archived)),
    )
    .await;
    let Some(Event::SessionList { sessions }) = collected.last() else {
        panic!()
    };
    let meta = sessions.iter().find(|s| s.id == session_id).unwrap();
    assert!(!meta.pinned && meta.archived);

    // Persistence check: reopen the store and read back the archived flag
    let store = pig_core::store::Store::open(&data_dir).unwrap();
    let meta = store.get_session(&session_id).unwrap();
    assert!(meta.archived && !meta.pinned, "{meta:?}");
    agent.shutdown();
}

/// Two sessions send messages in parallel; events are routed correctly by session_id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_sessions() {
    let (config_path, cwd, data_dir) = setup("m4-parallel");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_a = new_session(&agent, cwd.clone()).await;
    let session_b = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_a.clone(),
            content: "ECHO_HISTORY A".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_b.clone(),
            content: "Read the mock file and summarize it".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();

    let mut complete_a = false;
    let mut complete_b = false;
    let mut all = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !(complete_a && complete_b) {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            assert!(
                std::time::Instant::now() < deadline,
                "parallel turns timed out"
            );
            continue;
        };
        match &event {
            Event::TurnComplete { session_id, .. } if *session_id == session_a => complete_a = true,
            Event::TurnComplete { session_id, .. } if *session_id == session_b => complete_b = true,
            _ => {}
        }
        all.push(event);
    }
    // A's text events all belong to A; B's tool calls all belong to B
    for event in &all {
        match event {
            Event::TextDelta {
                session_id, delta, ..
            } if delta.contains("HISTORY_COUNT") => {
                assert_eq!(session_id, &session_a, "A's text should not leak into B")
            }
            Event::ToolCallBegin {
                session_id, tool, ..
            } if tool == "Read" => {
                assert_eq!(
                    session_id, &session_b,
                    "B's tool call should not leak into A"
                )
            }
            _ => {}
        }
    }
    agent.shutdown();
}

/// AGENTS.md two-layer injection (the English system prompt exceeds the mock's
/// 3000-char echo cap, so assert via the request-body log that both layers of
/// rules entered the request sent to the model).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_md_injected() {
    let (port, log) = pig_provider::mock::start_mock_server_with_log();
    let cwd = std::env::temp_dir().join(format!("pig-core-m4-agents-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join(pig_provider::mock::MOCK_FILE_NAME),
        pig_provider::mock::MOCK_FILE_CONTENT,
    )
    .unwrap();
    let data_dir = cwd.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(data_dir.join("AGENTS.md"), "GLOBAL_RULE_X1: global rule").unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "WORKSPACE_RULE_Y2: workspace rule").unwrap();
    let config_path = cwd.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock provider"
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

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: "Just chatting".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    let bodies = log.lock().unwrap().join("\n");
    assert!(
        bodies.contains("GLOBAL_RULE_X1"),
        "should include the global AGENTS.md"
    );
    assert!(
        bodies.contains("WORKSPACE_RULE_Y2"),
        "should include the workspace AGENTS.md"
    );
    agent.shutdown();
}

/// Compact: history gets shorter and carries the compact marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_shortens_history() {
    let (config_path, cwd, data_dir) = setup("m4-compact");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd.clone()).await;

    // Three conversation rounds to accumulate history (the first with a tool chain, later rounds plain text)
    for text in [
        "Read the mock file and summarize it",
        "Summarize again",
        "Go on",
    ] {
        agent
            .ops
            .send(Op::SendMessage {
                session_id: session_id.clone(),
                content: text.into(),
                files: vec![],
                images: vec![],
                mode: ExecMode::AutoEdit,
            })
            .await
            .unwrap();
        recv_until(&events, Duration::from_secs(20), |e| {
            matches!(e, Event::TurnComplete { .. })
        })
        .await;
    }

    agent
        .ops
        .send(Op::Compact {
            session_id: session_id.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let compacted = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    let Some(Event::ContextCompacted { omitted, note, .. }) = compacted.last() else {
        panic!("expected ContextCompacted")
    };
    assert!(*omitted > 0);
    assert!(note.contains("Earlier context compacted"), "{note}");

    // History is shorter after compact
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "ECHO_HISTORY".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let count = collected.iter().find_map(|e| match e {
        Event::TextDone { full_text, .. } => full_text
            .strip_prefix("HISTORY_COUNT:")
            .and_then(|n| n.parse::<usize>().ok()),
        _ => None,
    });
    // Since M5 compact keeps the last 4 entries: system + summary + 4 entries + new user = 7
    assert_eq!(
        count,
        Some(7),
        "history should be shorter after compact: {collected:#?}"
    );
    agent.shutdown();
}

/// @ file search.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_files_finds_real_files() {
    let (config_path, cwd, data_dir) = setup("m4-search");
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::write(cwd.join("src/hello.rs"), "fn main() {}").unwrap();

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SearchFiles {
            session_id,
            query: "hello".into(),
            cwd: None,
        })
        .await
        .unwrap();
    let events = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::FileSearchResults { .. })
    })
    .await;
    let Some(Event::FileSearchResults { results, .. }) = events.last() else {
        panic!("expected FileSearchResults")
    };
    assert!(
        results.iter().any(|r| r == "src/hello.rs"),
        "should find the real file: {results:?}"
    );
    assert!(
        !results.iter().any(|r| r.contains("config.toml")),
        "the data directory outside cwd should not appear: {results:?}"
    );
    agent.shutdown();
}

/// Restart persistence: todos (todos table), file changes (file_changes table),
/// original snapshots (file_originals table → cross-restart diff baseline +
/// revert).
/// Two sessions run the two scenarios separately: the mock dispatches on
/// request-body substrings, so chaining scenarios in one session would
/// interfere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_restores_todos_changes_and_revert() {
    let (config_path, cwd, data_dir) = setup("m5-persist");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();

    // Session A: file changes (Write → Edit → Bash)
    let session_a = new_session(&agent, cwd.clone()).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_a.clone(),
            content: format!("{} edit a file", mock::SCENARIO_B_TRIGGER),
            files: vec![],
            images: vec![],
            mode: ExecMode::FullAccess,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    // Session B: todo writes (TodoList)
    let session_b = new_session(&agent, cwd.clone()).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_b.clone(),
            content: format!("{} create two todos", mock::TODO_SCENARIO_TRIGGER),
            files: vec![],
            images: vec![],
            mode: ExecMode::FullAccess,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    agent.shutdown();

    // Simulate a restart: new manager on the same data_dir
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events2 = agent2.events.clone();

    // Reopen session A: file changes restored from the DB (one current-state entry per path)
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_a.clone(),
        })
        .await
        .unwrap();
    let replay_a = recv_until(&events2, Duration::from_secs(10), |e| {
        // Replay-end sentinel: the TurnComplete with stats=None. If the turn
        // had usage, the TurnComplete carrying TurnStats (stats=Some) is
        // replayed first — do not stop early there
        matches!(e, Event::TurnComplete { stats: None, .. })
    })
    .await;
    let file_changes: Vec<_> = replay_a
        .iter()
        .filter(|e| matches!(e, Event::FileChanged { path, .. } if path.ends_with("hello.txt")))
        .collect();
    assert_eq!(
        file_changes.len(),
        1,
        "only one current-state entry per path in the replay: {file_changes:?}"
    );

    // Cross-restart revert: original snapshots restored → revert succeeds and the created file is deleted
    while tokio::time::timeout(Duration::from_millis(200), events2.recv())
        .await
        .is_ok()
    {}
    agent2
        .ops
        .send(Op::RevertFile {
            session_id: session_a.clone(),
            path: mock::SCENARIO_B_FILE.to_string(),
        })
        .await
        .unwrap();
    let ev = recv_until(&events2, Duration::from_secs(5), |e| {
        matches!(e, Event::FileReverted { .. } | Event::Error { .. })
    })
    .await;
    assert!(
        ev.iter().any(
            |e| matches!(e, Event::FileReverted { path, .. } if path == mock::SCENARIO_B_FILE)
        ),
        "cross-restart revert should succeed (baseline from the file_originals table): {ev:#?}"
    );
    assert!(
        !cwd.join(mock::SCENARIO_B_FILE).exists(),
        "the created file should be deleted after revert"
    );

    // Reopen session B: todos restored from the DB (even for a session whose JSONL has no todo records)
    while tokio::time::timeout(Duration::from_millis(200), events2.recv())
        .await
        .is_ok()
    {}
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_b.clone(),
        })
        .await
        .unwrap();
    let replay_b = recv_until(&events2, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let todo_snapshot = replay_b.iter().find_map(|e| match e {
        Event::TodoListChanged { items, .. } if !items.is_empty() => Some(items),
        _ => None,
    });
    let items = todo_snapshot.unwrap_or_else(|| {
        panic!("replay should include the todo snapshot restored from the DB: {replay_b:#?}")
    });
    assert_eq!(items.len(), 2, "{items:?}");
    assert!(
        items
            .iter()
            .any(|i| i.content.contains(mock::TODO_SCENARIO_ITEM)
                && i.status == pig_protocol::TodoStatus::InProgress),
        "the in-progress todo should be restored: {items:?}"
    );
    agent2.shutdown();
}

/// Session-level model/mode/reasoning-level persistence:
/// (1) a new session inherits the values of the workspace's most recently
/// active session; (2) reopening after a restart restores them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_model_mode_persist_and_inherit() {
    let (config_path, cwd, data_dir) = setup("m5-mode-persist");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let session_a = new_session(&agent, cwd.clone()).await;

    // Session A sets mode + model + reasoning level (written through to the sessions table)
    agent
        .ops
        .send(Op::SetExecMode {
            session_id: session_a.clone(),
            mode: ExecMode::FullAccess,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SetModel {
            session_id: session_a.clone(),
            provider_id: "mock".into(),
            model_id: "mock-model".into(),
            reasoning_level: Some("high".into()),
        })
        .await
        .unwrap();
    // Wait for the write-through to finish (ops are processed in order; send a ListSessions as a barrier)
    agent.ops.send(Op::ListSessions).await.unwrap();
    recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::SessionList { .. })
    })
    .await;

    // (1) New session in the same workspace: model/mode unspecified → inherits
    // A's seed; the reasoning level simulates the UI hero seeding it explicitly
    // (core adopts it as-is)
    agent
        .ops
        .send(Op::NewSession {
            cwd: cwd.clone(),
            provider_id: None,
            model_id: None,
            reasoning_level: Some("high".into()),
            exec_mode: None,
            plan_enabled: None,
        })
        .await
        .unwrap();
    let configured = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::SessionConfigured { .. })
    })
    .await;
    let Some(Event::SessionConfigured {
        provider_id,
        model_id,
        reasoning_level,
        exec_mode,
        ..
    }) = configured.last()
    else {
        panic!("expected SessionConfigured")
    };
    assert_eq!(
        (
            provider_id.as_deref(),
            model_id.as_deref(),
            reasoning_level.as_deref(),
            *exec_mode
        ),
        (
            Some("mock"),
            Some("mock-model"),
            Some("high"),
            ExecMode::FullAccess
        ),
        "new session should inherit the model/mode/reasoning level of the workspace's most recently active session"
    );
    agent.shutdown();

    // (2) Simulate a restart and reopen A: values should be restored from the sessions table
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_a.clone(),
        })
        .await
        .unwrap();
    // A has no turn records so the replay emits no TurnComplete; use TaskListChanged (always emitted on the reopen path) as the endpoint
    let replay = recv_until(&events2, Duration::from_secs(10), |e| {
        matches!(e, Event::TaskListChanged { .. })
    })
    .await;
    let Some(Event::SessionConfigured {
        provider_id,
        model_id,
        reasoning_level,
        exec_mode,
        ..
    }) = replay
        .iter()
        .find(|e| matches!(e, Event::SessionConfigured { .. }))
    else {
        panic!("replay should have SessionConfigured: {replay:#?}")
    };
    assert_eq!(
        (
            provider_id.as_deref(),
            model_id.as_deref(),
            reasoning_level.as_deref(),
            *exec_mode
        ),
        (
            Some("mock"),
            Some("mock-model"),
            Some("high"),
            ExecMode::FullAccess
        ),
        "reopen after restart should restore model/mode/reasoning level"
    );
    agent2.shutdown();
}

/// Change settings without chatting, then switch back and forth between two sessions: values must follow their own session, never lost or crossed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switch_preserves_mode_without_turn() {
    let (config_path, cwd, data_dir) = setup("m5-switch");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_a = new_session(&agent, cwd.clone()).await;
    let session_b = new_session(&agent, cwd.clone()).await;

    // Change mode/model/reasoning level on A (no chatting)
    agent
        .ops
        .send(Op::SetExecMode {
            session_id: session_a.clone(),
            mode: ExecMode::FullAccess,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SetModel {
            session_id: session_a.clone(),
            provider_id: "mock".into(),
            model_id: "mock-model".into(),
            reasoning_level: Some("high".into()),
        })
        .await
        .unwrap();
    // Barrier: ops are processed in order; once a ListSessions comes back the write-through is done
    agent.ops.send(Op::ListSessions).await.unwrap();
    recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::SessionList { .. })
    })
    .await;

    // Switch to B: should be B's own defaults, not A's
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_b.clone(),
        })
        .await
        .unwrap();
    let ev_b = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::TaskListChanged { .. })
    })
    .await;
    let Some(Event::SessionConfigured {
        provider_id,
        reasoning_level,
        exec_mode,
        ..
    }) = ev_b
        .iter()
        .find(|e| matches!(e, Event::SessionConfigured { .. }))
    else {
        panic!("B should have SessionConfigured: {ev_b:#?}")
    };
    assert_eq!(
        (
            provider_id.as_deref(),
            reasoning_level.as_deref(),
            *exec_mode
        ),
        (None, None, ExecMode::ConfirmBeforeEdit),
        "B should not carry A's values"
    );

    // Change only the reasoning level on B, no model selected (persists even without a model override)
    agent
        .ops
        .send(Op::SetReasoning {
            session_id: session_b.clone(),
            reasoning_level: Some("max".into()),
        })
        .await
        .unwrap();
    agent.ops.send(Op::ListSessions).await.unwrap();
    recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::SessionList { .. })
    })
    .await;

    // Switch back to A: the values just changed should be restored (even though no chatting happened)
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_a.clone(),
        })
        .await
        .unwrap();
    let ev_a = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::TaskListChanged { .. })
    })
    .await;
    let Some(Event::SessionConfigured {
        provider_id,
        model_id,
        reasoning_level,
        exec_mode,
        ..
    }) = ev_a
        .iter()
        .find(|e| matches!(e, Event::SessionConfigured { .. }))
    else {
        panic!("A should have SessionConfigured: {ev_a:#?}")
    };
    assert_eq!(
        (
            provider_id.as_deref(),
            model_id.as_deref(),
            reasoning_level.as_deref(),
            *exec_mode
        ),
        (
            Some("mock"),
            Some("mock-model"),
            Some("high"),
            ExecMode::FullAccess
        ),
        "switching back should restore A's changed values"
    );

    // Switch back to B again: the reasoning-level-only change should be there too
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_b.clone(),
        })
        .await
        .unwrap();
    let ev_b2 = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::TaskListChanged { .. })
    })
    .await;
    let Some(Event::SessionConfigured {
        provider_id,
        reasoning_level,
        ..
    }) = ev_b2
        .iter()
        .find(|e| matches!(e, Event::SessionConfigured { .. }))
    else {
        panic!("B should have SessionConfigured: {ev_b2:#?}")
    };
    assert_eq!(
        (provider_id.as_deref(), reasoning_level.as_deref()),
        (None, Some("max")),
        "reasoning level changed without a model override should persist"
    );
    agent.shutdown();
}

/// Regression: after resume, new messages must keep being persisted.
/// `Session::load` once set `rollout: None`, so new content in a reopened
/// session was lost on restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_keeps_appending() {
    let (config_path, cwd, data_dir) = setup("m4-resume-append");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd.clone()).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Round one message".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    agent.shutdown();

    // Simulate a restart: reopen the old session and append a second round
    let agent2 = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    // The replay ends with a TurnComplete of duration_ms=0; drain, then send the new message
    recv_until(&events2, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    while tokio::time::timeout(Duration::from_millis(200), events2.recv())
        .await
        .is_ok()
    {}
    agent2
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Round two message".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    recv_until(&events2, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    agent2.shutdown();

    // The persisted file should contain both rounds' user messages
    let rollout_path = data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let content = std::fs::read_to_string(&rollout_path).unwrap();
    assert!(
        content.contains("Round one message"),
        "round one should be kept: {content}"
    );
    assert!(
        content.contains("Round two message"),
        "new messages after resume should be persisted: {content}"
    );

    // Simulate one more restart: the replay should contain both rounds' messages
    let agent3 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events3 = agent3.events.clone();
    agent3
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let replay = recv_until(&events3, Duration::from_secs(10), |e| {
        // Same as above: wait for the replay end (TurnComplete with stats=None); do not stop at the earlier TurnStats-carrying completion
        matches!(e, Event::TurnComplete { stats: None, .. })
    })
    .await;
    for round in ["Round one message", "Round two message"] {
        assert!(
            replay
                .iter()
                .any(|e| matches!(e, Event::UserMessage { text, .. } if text.contains(round))),
            "replay should contain '{round}': {replay:#?}"
        );
    }
    agent3.shutdown();
}

/// Model-config default reasoning level: a new session with no level specified
/// adopts the default tier; an explicit level is unaffected; a default tier
/// not in the level list is ignored
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_reasoning_level_on_new_session() {
    let port = mock::start_mock_server();
    let dir =
        std::env::temp_dir().join(format!("pig-core-m9-default-level-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock provider"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
reasoning_levels = ["low", "high", "max"]
default_reasoning_level = "high"
"#
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let events = agent.events.clone();

    let create = |level: Option<String>| Op::NewSession {
        cwd: dir.clone(),
        provider_id: None,
        model_id: None,
        reasoning_level: level,
        exec_mode: None,
        plan_enabled: None,
    };
    async fn wait_configured(events: &async_channel::Receiver<Event>) -> Vec<Event> {
        recv_until(events, Duration::from_secs(5), |e| {
            matches!(e, Event::SessionConfigured { .. })
        })
        .await
    }

    // No level specified → adopt the model's default tier high
    agent.ops.send(create(None)).await.unwrap();
    let evs = wait_configured(&events).await;
    let level = evs.iter().find_map(|e| match e {
        Event::SessionConfigured {
            reasoning_level, ..
        } => Some(reasoning_level.clone()),
        _ => None,
    });
    assert_eq!(
        level,
        Some(Some("high".to_string())),
        "unspecified level should adopt the default tier"
    );

    // Explicitly specified → not overridden by the default tier
    agent.ops.send(create(Some("low".into()))).await.unwrap();
    let evs = wait_configured(&events).await;
    let level = evs.iter().find_map(|e| match e {
        Event::SessionConfigured {
            reasoning_level, ..
        } => Some(reasoning_level.clone()),
        _ => None,
    });
    assert_eq!(
        level,
        Some(Some("low".to_string())),
        "explicit level should not be overridden"
    );

    // Explicit off (None's off semantics equal unspecified): see the comments
    // on NewSession — the default-tier semantics take priority; here we
    // re-verify behavior when the default tier is invalid
    agent.ops.send(create(None)).await.unwrap();
    let _ = wait_configured(&events).await;
    agent.shutdown();

    // Default tier not in the level list → ignored, stays unspecified (off)
    let mut config: pig_protocol::AppConfig =
        toml::from_str(&std::fs::read_to_string(dir.join("config.toml")).unwrap()).unwrap();
    config.providers[0].models[0].default_reasoning_level = Some("ultra".into());
    std::fs::write(dir.join("config.toml"), toml::to_string(&config).unwrap()).unwrap();
    let agent2 =
        pig_core::spawn_agent_with_data_dir(Some(dir.join("config.toml")), dir.clone(), data_dir);
    let events2 = agent2.events.clone();
    agent2.ops.send(create(None)).await.unwrap();
    let evs = wait_configured(&events2).await;
    let level = evs.iter().find_map(|e| match e {
        Event::SessionConfigured {
            reasoning_level, ..
        } => Some(reasoning_level.clone()),
        _ => None,
    });
    assert_eq!(level, Some(None), "invalid default tier should be ignored");
    agent2.shutdown();
}

/// Crash recovery: a turn whose process died before the wrap-up has durable
/// per-request StepUsage records but no TurnStats. Reopening the session sums
/// the tail turn's steps into the statistics, appends a synthesized TurnStats
/// as the "counted" marker (replay restores the footer through the normal
/// path), and a second reopen sees the marker and does not count again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_interrupted_turn_usage_recovered_on_reopen() {
    let (config_path, cwd, data_dir) = setup("m4-crash-recovery");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd.clone()).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Read the mock file and summarize it".into(),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    agent.shutdown();

    // Simulate a hard kill before the turn wrap-up: the turn_stats record
    // never landed, but the per-request step_usage records did
    let rollout_path = data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let original = std::fs::read_to_string(&rollout_path).expect("rollout exists");
    assert!(
        original.contains("\"type\":\"turn_stats\""),
        "a completed turn persists its stats: {original}"
    );
    let stripped: String = original
        .lines()
        .filter(|line| !line.contains("\"type\":\"turn_stats\""))
        .map(|line| format!("{line}\n"))
        .collect();
    std::fs::write(&rollout_path, stripped).unwrap();

    // Reopen: recovery re-appends the marker and replay restores the footer
    let agent2 = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    // Replay is followed by a ContextUsage re-emit (the model is configured)
    let replayed = recv_until(&events2, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextUsage { .. })
    })
    .await;
    assert!(
        replayed.iter().any(|e| matches!(
            e,
            Event::TurnComplete {
                stats: Some(stats),
                ..
            } if stats.input > 0
        )),
        "replay should restore the recovered turn's footer stats: {replayed:#?}"
    );
    let recovered = std::fs::read_to_string(&rollout_path).unwrap();
    assert_eq!(
        recovered.matches("\"type\":\"turn_stats\"").count(),
        1,
        "recovery appends exactly one synthesized marker: {recovered}"
    );
    agent2.shutdown();

    // Idempotent: a second reopen sees the marker and does not count again
    let agent3 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events3 = agent3.events.clone();
    agent3
        .ops
        .send(Op::OpenSession { session_id })
        .await
        .unwrap();
    recv_until(&events3, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextUsage { .. })
    })
    .await;
    let again = std::fs::read_to_string(&rollout_path).unwrap();
    assert_eq!(
        again.matches("\"type\":\"turn_stats\"").count(),
        1,
        "the marker prevents double counting: {again}"
    );
    agent3.shutdown();
}
