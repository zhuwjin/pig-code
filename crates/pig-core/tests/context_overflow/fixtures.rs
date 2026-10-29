use std::time::Duration;

use pig_protocol::{ApiFormat, Event, ExecMode, Op};

use crate::common;

fn format_name(format: ApiFormat) -> &'static str {
    match format {
        ApiFormat::OpenAiChat => "OpenAiChat",
        ApiFormat::OpenAiResponses => "OpenAiResponses",
        ApiFormat::AnthropicMessages => "AnthropicMessages",
    }
}

pub(crate) fn config(
    base_url: &str,
    format: ApiFormat,
    context_window: u64,
    max_output: u64,
) -> String {
    format!(
        r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "{base_url}"
api_key = "mock-key"
api_format = "{}"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = {context_window}
max_output_tokens = {max_output}
"#,
        format_name(format)
    )
}

pub(crate) fn workspace(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pig-core-overflow-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Append one complete old user/assistant/tool segment to the rollout while
/// the agent is stopped. The trigger lives only in the old tool result, so no
/// live request ever delivered it to the mock before this turn.
pub(crate) fn seed_old_segment(
    data_dir: &std::path::Path,
    session_id: &str,
    text: &str,
    trigger: &str,
) {
    use std::io::Write as _;
    let path = data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    let user = serde_json::json!({
        "type": "user",
        "text": text,
        "files": [],
        "images": [],
        "sent_text": text,
        "sent_images": [],
    });
    let assistant = serde_json::json!({
        "type": "assistant",
        "text": "",
        "tool_calls": [{
            "id": "seed_call",
            "type": "function",
            "function": {"name": "Read", "arguments": "{\"path\":\"old.txt\"}"},
        }],
    });
    let tool = serde_json::json!({
        "type": "tool_call",
        "tool": "Read",
        "summary": "old.txt",
        "arguments": "{\"path\":\"old.txt\"}",
        "output": trigger,
        "is_error": false,
        "agent_cards": [],
        "images": [],
    });
    writeln!(file, "{user}").unwrap();
    writeln!(file, "{assistant}").unwrap();
    writeln!(file, "{tool}").unwrap();
}

pub(crate) async fn open_session(agent: &pig_core::AgentHandle, session_id: &str) {
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
    .await;
}

pub(crate) async fn send_and_complete(
    agent: &pig_core::AgentHandle,
    session_id: &str,
    text: &str,
) -> Vec<Event> {
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
    common::recv_until(&agent.events, Duration::from_secs(30), |event| {
        matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. } | Event::Error { .. }
        )
    })
    .await
}

pub(crate) fn main_finishes(data_dir: &std::path::Path, session_id: &str) -> Vec<(String, i64)> {
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

pub(crate) fn compact_finishes(data_dir: &std::path::Path, session_id: &str) -> Vec<(String, i64)> {
    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).unwrap();
    let mut stmt = db
        .prepare(
            "SELECT finish, COALESCE(http_status, 0) FROM api_usage
             WHERE session_id = ?1 AND source = 'compact' ORDER BY rowid",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .unwrap();
    rows.filter_map(Result::ok).collect()
}
