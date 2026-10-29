//! Anthropic `pause_turn` continuation: the paused assistant step is durable,
//! the same turn continues without a new user message, local tools execute
//! before the next request, the replay/prefix survives restart, and an
//! exhausted continuation budget fails explicitly.

use crate::common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use pig_protocol::{Event, ExecMode, Op};
use pig_provider::mock;

fn config_window(base_url: &str, context_window: u64, max_output_tokens: u64) -> String {
    format!(
        r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "{base_url}"
api_key = "mock-key"
api_format = "AnthropicMessages"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = {context_window}
max_output_tokens = {max_output_tokens}
cap_web_search = true
"#
    )
}

fn setup(name: &str) -> (PathBuf, PathBuf, PathBuf, mock::RequestLog) {
    setup_window(name, 128_000, 8_192)
}

fn setup_window(
    name: &str,
    context_window: u64,
    max_output_tokens: u64,
) -> (PathBuf, PathBuf, PathBuf, mock::RequestLog) {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-pause-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        config_window(
            &format!("http://127.0.0.1:{port}/v1"),
            context_window,
            max_output_tokens,
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    (config_path, dir, data_dir, log)
}

fn assert_no_empty_assistant(body: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return;
    };
    for message in value["messages"].as_array().into_iter().flatten() {
        if message["role"] != "assistant" {
            continue;
        }
        let content = &message["content"];
        let non_empty = match content {
            serde_json::Value::String(text) => !text.is_empty(),
            serde_json::Value::Array(blocks) => !blocks.is_empty(),
            _ => false,
        };
        assert!(
            non_empty,
            "empty assistant content on the Anthropic wire: {message}"
        );
    }
}

async fn send_collect(agent: &pig_core::AgentHandle, session_id: &str, text: &str) -> Vec<Event> {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: text.into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. } | Event::Error { .. }
        )
    })
    .await
}

async fn open_session(agent: &pig_core::AgentHandle, session_id: &str) -> Vec<Event> {
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.to_string(),
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::TurnComplete {
                duration_ms: 0,
                stats: None,
                ..
            }
        )
    })
    .await
}

fn main_finishes(data_dir: &Path, session_id: &str) -> Vec<(String, i64)> {
    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).unwrap();
    let mut stmt = db
        .prepare(
            "SELECT finish, COALESCE(http_status, 0) FROM api_usage
             WHERE session_id = ?1 AND source = 'main' ORDER BY rowid",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .unwrap();
    rows.filter_map(Result::ok).collect()
}

fn subagent_finishes(data_dir: &Path, session_id: &str) -> Vec<(String, i64)> {
    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).unwrap();
    let mut stmt = db
        .prepare(
            "SELECT finish, COALESCE(http_status, 0) FROM api_usage
             WHERE session_id = ?1 AND source = 'subagent' ORDER BY rowid",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .unwrap();
    rows.filter_map(Result::ok).collect()
}

fn text_dones(events: &[Event]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::TextDone {
                item_id, full_text, ..
            } => Some((item_id.clone(), full_text.clone())),
            _ => None,
        })
        .collect()
}

fn sources(events: &[Event]) -> Vec<(String, Vec<pig_protocol::Source>)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Sources {
                item_id, sources, ..
            } => Some((item_id.clone(), sources.clone())),
            _ => None,
        })
        .collect()
}

fn request_bodies(log: &mock::RequestLog) -> Vec<String> {
    log.lock().expect("request log").clone()
}

fn streaming_bodies(log: &mock::RequestLog) -> Vec<String> {
    request_bodies(log)
        .into_iter()
        .filter(|body| body.contains("\"stream\":true"))
        .collect()
}

fn assert_single_turn_boundary(events: &[Event]) {
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::TurnStarted { .. }))
            .count(),
        1,
        "one TurnStarted per user turn: {events:#?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::UserMessage { .. }))
            .count(),
        1,
        "pause continuation never invents a user message: {events:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_once_continues_in_the_same_turn() {
    let (config_path, dir, data_dir, log) = setup("once");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = send_collect(&agent, &session_id, mock::ANTHROPIC_PAUSE_ONCE_TRIGGER).await;

    assert_single_turn_boundary(&events);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::TurnComplete { .. }))
            .count(),
        1,
        "the resumed turn completes exactly once: {events:#?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::Error { .. } | Event::TurnAborted { .. } | Event::CompactStarted { .. }
        )),
        "pause is not an error/incomplete/compact path: {events:#?}"
    );
    let texts = text_dones(&events);
    assert_eq!(texts.len(), 2, "paused answer + final answer: {texts:#?}");
    assert!(
        texts
            .iter()
            .any(|(_, text)| text == mock::ANTHROPIC_PAUSE_ONCE_CONTENT)
    );
    assert!(
        texts
            .iter()
            .any(|(_, text)| text == mock::ANTHROPIC_PAUSE_ONCE_DONE)
    );

    let finishes = main_finishes(&data_dir, &session_id);
    assert_eq!(
        finishes
            .iter()
            .map(|(finish, _)| finish.as_str())
            .collect::<Vec<_>>(),
        ["pause", "stop"],
        "pause API call is durable and distinct from the final stop"
    );
    let trace = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.model-io.jsonl")),
    )
    .unwrap();
    assert!(trace.contains("\"finish\":\"pause\""), "{trace}");
    assert!(trace.contains("\"finish\":\"stop\""), "{trace}");

    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    assert_eq!(
        rollout.matches("\"type\":\"assistant\"").count(),
        2,
        "both paused and final assistant steps are durable"
    );
    assert_eq!(
        rollout.matches("\"type\":\"user\"").count(),
        1,
        "no duplicated user record"
    );
    assert!(rollout.contains("anthropic_server_tool_use"), "{rollout}");
    assert!(rollout.contains("anthropic_web_search_result"), "{rollout}");
    assert!(rollout.contains("sig-pause-1"), "{rollout}");
    assert!(rollout.contains("encrypted-pause-1"), "{rollout}");

    // Sources attach to the exact paused text item, not to the final answer.
    let paused_item = texts
        .iter()
        .find(|(_, text)| text == mock::ANTHROPIC_PAUSE_ONCE_CONTENT)
        .map(|(item, _)| item.clone())
        .expect("paused text item");
    let sources = sources(&events);
    assert_eq!(sources.len(), 1, "one paused source set: {sources:#?}");
    assert_eq!(sources[0].0, paused_item);
    assert_eq!(sources[0].1[0].url, mock::ANTHROPIC_PAUSE_SOURCE_URL);

    // The continuation request carries the paused assistant once and does not
    // resend the user message.
    let streaming = streaming_bodies(&log);
    assert_eq!(streaming.len(), 2, "pause + continuation: {streaming:#?}");
    assert_eq!(
        streaming[1]
            .matches(mock::ANTHROPIC_PAUSE_ONCE_CONTENT)
            .count(),
        1,
        "paused assistant appears once in the continuation"
    );
    assert_eq!(
        streaming[1]
            .matches(mock::ANTHROPIC_PAUSE_ONCE_TRIGGER)
            .count(),
        1,
        "the original user message is not duplicated"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consecutive_pauses_resume_and_keep_item_ids() {
    let (config_path, dir, data_dir, log) = setup("twice");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = send_collect(&agent, &session_id, mock::ANTHROPIC_PAUSE_TWICE_TRIGGER).await;
    assert_single_turn_boundary(&events);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::TurnComplete { .. }))
            .count(),
        1,
        "two pauses then a single normal stop: {events:#?}"
    );
    let texts = text_dones(&events);
    assert_eq!(
        texts
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>(),
        [
            mock::ANTHROPIC_PAUSE_TWICE_CONTENT_1,
            mock::ANTHROPIC_PAUSE_TWICE_CONTENT_2,
            mock::ANTHROPIC_PAUSE_TWICE_DONE,
        ]
    );
    let finishes = main_finishes(&data_dir, &session_id);
    assert_eq!(
        finishes
            .iter()
            .map(|(finish, _)| finish.as_str())
            .collect::<Vec<_>>(),
        ["pause", "pause", "stop"]
    );
    let sources = sources(&events);
    assert_eq!(sources.len(), 2, "one source set per paused step");
    assert_ne!(sources[0].0, sources[1].0, "distinct text item IDs");
    for (item_id, _) in &sources {
        assert!(
            texts.iter().any(|(text_item, text)| {
                text_item == item_id
                    && (text == mock::ANTHROPIC_PAUSE_TWICE_CONTENT_1
                        || text == mock::ANTHROPIC_PAUSE_TWICE_CONTENT_2)
            }),
            "sources attach to a paused text item: {item_id}"
        );
    }
    let streaming = streaming_bodies(&log);
    assert_eq!(streaming.len(), 3);
    let last = streaming.last().expect("continuation");
    assert_eq!(
        last.matches(mock::ANTHROPIC_PAUSE_TWICE_CONTENT_1).count(),
        1
    );
    assert_eq!(
        last.matches(mock::ANTHROPIC_PAUSE_TWICE_CONTENT_2).count(),
        1
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_limit_fails_with_english_error() {
    let (config_path, dir, data_dir, _log) = setup("limit");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = send_collect(&agent, &session_id, mock::ANTHROPIC_PAUSE_ALWAYS_TRIGGER).await;
    assert_single_turn_boundary(&events);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "an exhausted pause budget does not produce TurnComplete: {events:#?}"
    );
    let error = events
        .iter()
        .find_map(|event| match event {
            Event::Error { error, .. } => Some(error),
            _ => None,
        })
        .expect("pause limit error");
    let detail = match error {
        pig_protocol::CoreError::Internal { detail } => detail,
        other => panic!("unexpected error kind: {other:?}"),
    };
    assert!(
        detail.contains("Pause continuation limit reached") && detail.contains("more than 3 times"),
        "English typed pause-limit detail: {detail}"
    );
    let finishes = main_finishes(&data_dir, &session_id);
    assert_eq!(
        finishes.len(),
        4,
        "4 paused steps, 3 continuations: {finishes:?}"
    );
    assert!(
        finishes.iter().all(|(finish, _)| finish == "pause"),
        "every paused step has a durable usage row: {finishes:?}"
    );
    assert_eq!(
        text_dones(&events).len(),
        4,
        "each paused step forwarded its own text"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_restart_preserves_the_live_prefix() {
    let (config_path, dir, data_dir, log) = setup("restart");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = send_collect(&agent, &session_id, mock::ANTHROPIC_PAUSE_ONCE_TRIGGER).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "{events:#?}"
    );
    let live_streaming = streaming_bodies(&log);
    let live_continuation: serde_json::Value =
        serde_json::from_str(&live_streaming[1]).expect("live continuation body");
    let live_messages = live_continuation["messages"].clone();
    assert_eq!(
        live_messages.as_array().map(Vec::len),
        Some(2),
        "user + paused assistant (the system prompt is a top-level field)"
    );
    agent.shutdown();

    // Native blocks must be durable even though this compatible endpoint sends
    // text-only replay on the wire.
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    assert!(rollout.contains("anthropic_server_tool_use"), "{rollout}");
    assert!(rollout.contains("anthropic_web_search_result"), "{rollout}");
    let assistant_count = rollout.matches("\"type\":\"assistant\"").count();

    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let replay = open_session(&agent, &session_id).await;
    let replayed_pause = replay.iter().find_map(|event| match event {
        Event::TextDone {
            item_id, full_text, ..
        } if full_text == mock::ANTHROPIC_PAUSE_ONCE_CONTENT => Some(item_id.clone()),
        _ => None,
    });
    let Some(replayed_pause) = replayed_pause else {
        panic!("restart replay loses the paused assistant text: {replay:#?}");
    };
    assert!(
        replay.iter().any(|event| matches!(
            event,
            Event::Sources { item_id, sources, .. }
                if item_id == &replayed_pause
                    && sources.iter().any(|source| source.url == mock::ANTHROPIC_PAUSE_SOURCE_URL)
        )),
        "restart replay re-derives the paused sources: {replay:#?}"
    );
    let events = send_collect(&agent, &session_id, "continue after pause").await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "{events:#?}"
    );
    let bodies = request_bodies(&log);
    let restart_body = bodies
        .iter()
        .rev()
        .find(|body| body.contains("continue after pause"))
        .expect("post-restart request");
    let restart: serde_json::Value = serde_json::from_str(restart_body).unwrap();
    let restart_messages = restart["messages"].as_array().expect("messages");
    assert!(
        restart_messages.len() >= live_messages.as_array().unwrap().len(),
        "restart keeps the live prefix and appends the new turn"
    );
    assert_eq!(
        &restart_messages[..live_messages.as_array().unwrap().len()],
        live_messages.as_array().unwrap(),
        "restart prefix equals the live continuation prefix"
    );
    assert_eq!(
        restart_body
            .matches(mock::ANTHROPIC_PAUSE_ONCE_CONTENT)
            .count(),
        1,
        "paused assistant is not duplicated after restart"
    );
    assert_eq!(
        restart_body
            .matches(mock::ANTHROPIC_PAUSE_ONCE_TRIGGER)
            .count(),
        1,
        "the user message is not duplicated after restart"
    );
    let after = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    assert_eq!(
        after.matches("\"type\":\"assistant\"").count(),
        assistant_count + 1,
        "restart does not re-materialize old assistant records"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_with_a_local_tool_executes_before_continuing() {
    let (config_path, dir, data_dir, log) = setup("tool");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = send_collect(&agent, &session_id, mock::ANTHROPIC_PAUSE_TOOL_TRIGGER).await;
    assert_single_turn_boundary(&events);
    let tool_end = events
        .iter()
        .find_map(|event| match event {
            Event::ToolCallEnd {
                output, is_error, ..
            } if output.contains("known file") || output.contains(mock::MOCK_FILE_NAME) => {
                Some((output.clone(), *is_error))
            }
            _ => None,
        })
        .expect("the paused step's local Read executed");
    assert!(
        !tool_end.1,
        "the local tool result is a success: {tool_end:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "{events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Error { .. })),
        "{events:#?}"
    );
    let finishes = main_finishes(&data_dir, &session_id);
    assert_eq!(
        finishes
            .iter()
            .map(|(finish, _)| finish.as_str())
            .collect::<Vec<_>>(),
        ["pause", "stop"]
    );
    let streaming = streaming_bodies(&log);
    assert_eq!(streaming.len(), 2);
    assert!(
        streaming[1].contains("call_pause_tool")
            && (streaming[1].contains("\"tool_result\"")
                || streaming[1].contains("\"role\":\"tool\"")),
        "the continuation carries the closed tool pair: {}",
        streaming[1]
    );
    agent.shutdown();
}

/// Subagents reuse the same pause continuation mechanism and cap. The parent
/// uses Chat (so the mock's Agent-delegation scenario runs) while the pinned
/// child model uses Anthropic (so the pause scenario runs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_pause_continues_and_returns_a_result() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-pause-subagent-{}", std::process::id()));
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
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192

[[providers]]
id = "anthropic"
name = "Anthropic Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "AnthropicMessages"
enabled = true

[[providers.models]]
id = "claude-mock"
context_window = 128000
max_output_tokens = 8192
cap_web_search = true
"#
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agents_dir = data_dir.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("pause-profile.md"),
        format!(
            "---\nname: pause-profile\ndescription: pause test profile\ntools: [Read]\nmodel: anthropic/claude-mock\n---\n{}",
            mock::ANTHROPIC_PAUSE_ONCE_TRIGGER
        ),
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = send_collect(
        &agent,
        &session_id,
        &format!("{} GREP pause-profile", mock::SUBAGENT_TRIGGER),
    )
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "parent turn completes after the subagent resumes: {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Error { .. })),
        "subagent pause is not surfaced as an error: {events:#?}"
    );
    let receipt = events
        .iter()
        .find_map(|event| match event {
            Event::ToolCallEnd {
                output, is_error, ..
            } if output.contains(mock::ANTHROPIC_PAUSE_ONCE_DONE) => {
                Some((output.clone(), *is_error))
            }
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "subagent result carries its final text: {events:#?}\nrequests={:#?}",
                request_bodies(&log)
            )
        });
    assert!(!receipt.1, "subagent result is a success: {receipt:?}");
    let finishes = subagent_finishes(&data_dir, &session_id);
    assert_eq!(
        finishes
            .iter()
            .map(|(finish, _)| finish.as_str())
            .collect::<Vec<_>>(),
        ["pause", "stop"],
        "child pause step and resumed step are both recorded"
    );
    agent.shutdown();
}

/// A subagent that keeps pausing returns an explicit English error to the
/// parent rather than treating the pause as a successful stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_pause_limit_returns_an_error_result() {
    let (port, _log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!(
        "pig-core-pause-subagent-limit-{}",
        std::process::id()
    ));
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
name = "Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192

[[providers]]
id = "anthropic"
name = "Anthropic Mock"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "AnthropicMessages"
enabled = true

[[providers.models]]
id = "claude-mock"
context_window = 128000
max_output_tokens = 8192
cap_web_search = true
"#
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agents_dir = data_dir.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("pause-limit.md"),
        format!(
            "---\nname: pause-limit\ndescription: pause limit profile\ntools: [Read]\nmodel: anthropic/claude-mock\n---\n{}",
            mock::ANTHROPIC_PAUSE_ALWAYS_TRIGGER
        ),
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = send_collect(
        &agent,
        &session_id,
        &format!("{} GREP pause-limit", mock::SUBAGENT_TRIGGER),
    )
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the parent turn settles the child receipt: {events:#?}"
    );
    let receipt = events
        .iter()
        .find_map(|event| match event {
            Event::ToolCallEnd {
                output, is_error, ..
            } if output.contains("Pause continuation limit") => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("the subagent pause-limit failure reaches the parent");
    assert!(receipt.1, "the receipt is an error card");
    assert!(
        receipt.0.contains("more than 3 times"),
        "English pause-limit detail: {}",
        receipt.0
    );
    agent.shutdown();
}

/// Compatible Anthropic cannot replay server-only pause blocks; the provider
/// omits the assistant instead of sending an empty one. The next request and a
/// restart must remain valid and the following turn must complete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_blocks_only_pause_never_poisons_the_compatible_wire() {
    let (config_path, dir, data_dir, log) = setup("server-only");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    let events = send_collect(
        &agent,
        &session_id,
        mock::ANTHROPIC_PAUSE_SERVER_ONLY_TRIGGER,
    )
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the resumed turn completes: {events:#?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::TextDone { full_text, .. }
                if full_text.contains(mock::ANTHROPIC_PAUSE_SERVER_ONLY_DONE)
        )),
        "the final answer arrives: {events:#?}"
    );
    let live_bodies = streaming_bodies(&log);
    assert_eq!(
        live_bodies.len(),
        2,
        "pause + continuation: {live_bodies:#?}"
    );
    for body in &live_bodies {
        assert_no_empty_assistant(body);
    }
    // The pause assistant has no text/thinking and compatible replay cannot
    // carry its server blocks, so it is simply absent from the wire.
    assert!(
        !live_bodies[1].contains("\"role\":\"assistant\""),
        "the unreplayable pause assistant is skipped: {}",
        live_bodies[1]
    );
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    assert!(rollout.contains("anthropic_server_tool_use"), "{rollout}");
    assert!(rollout.contains("anthropic_web_search_result"), "{rollout}");
    agent.shutdown();

    // Restart: the durable pause assistant is rebuilt but remains skipped by
    // the compatible projection; the next user turn is valid and completes.
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    open_session(&agent, &session_id).await;
    let events = send_collect(&agent, &session_id, "next user after server-only pause").await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the post-restart turn completes: {events:#?}"
    );
    let bodies = request_bodies(&log);
    let restart = bodies
        .iter()
        .rev()
        .find(|body| body.contains("next user after server-only pause"))
        .expect("post-restart request");
    assert_no_empty_assistant(restart);
    assert!(
        !restart.contains("\"role\":\"assistant\",\"content\":[]"),
        "{restart}"
    );
    agent.shutdown();
}

/// A paused step's usage can exceed the compact trigger; the continuation
/// preflight must preserve the complete paused segment and skip instead of
/// silently restarting from the user message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_preflight_does_not_drop_the_paused_segment() {
    // trigger = 128000 - 100000 - 13000 = 15000; the paused response reports
    // used=120000 and its native result is large enough to exceed the tail
    // budget.
    let (config_path, dir, data_dir, log) = setup_window("preserve", 128_000, 100_000);
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    // A normal first turn creates an older segment and a real usage anchor.
    let first = send_collect(&agent, &session_id, "prelude").await;
    assert!(
        first
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "prelude turn completes: {first:#?}"
    );
    let events = send_collect(&agent, &session_id, mock::ANTHROPIC_PAUSE_PRESERVE_TRIGGER).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::ContextUsage { used: 120000, .. })),
        "the pause step's real usage crossed the trigger: {events:#?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::CompactStarted { .. } | Event::ContextCompacted { .. }
        )),
        "preserve-latest preflight must skip, not compact away the pause: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the continuation still completes: {events:#?}"
    );
    let finishes = main_finishes(&data_dir, &session_id);
    assert_eq!(
        finishes
            .iter()
            .map(|(finish, _)| finish.as_str())
            .collect::<Vec<_>>(),
        ["tool_calls", "stop", "pause", "stop"],
        "no compact request was made between pause and continuation: {finishes:?}"
    );
    for body in streaming_bodies(&log) {
        assert_no_empty_assistant(&body);
    }
    agent.shutdown();
}
