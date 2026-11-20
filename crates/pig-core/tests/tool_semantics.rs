//! Tool-semantics on the real core request path: the Anthropic
//! `tool_result.is_error` flag comes from the actual tool outcome and survives
//! rollout rebuild, and the typed tool policy from config reaches all three
//! wire formats.

use crate::common;

use common::{new_session, recv_until};
use pig_protocol::{ApiFormat, Event, ExecMode, Op};
use pig_provider::mock;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type RequestLog = Arc<Mutex<Vec<String>>>;

fn setup(
    name: &str,
    format: ApiFormat,
    extra_model_config: &str,
    with_mock_file: bool,
) -> (PathBuf, PathBuf, PathBuf, RequestLog) {
    let (port, log) = mock::start_mock_server_with_log();
    let dir =
        std::env::temp_dir().join(format!("pig-tool-semantics-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    if with_mock_file {
        std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    }
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
api_format = "{format:?}"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
{extra_model_config}
"#
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    (config_path, dir, data_dir, log)
}

async fn send(agent: &pig_core::AgentHandle, session_id: &str, content: &str) {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: content.to_string(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
}

/// Streaming (model turn) request bodies only: the title/compaction sidecars
/// are non-streaming and would otherwise interleave in the log.
fn stream_bodies(log: &RequestLog) -> Vec<Value> {
    log.lock()
        .expect("log")
        .iter()
        .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
        .filter(|body| body["stream"] == Value::Bool(true))
        .collect()
}

/// Every Anthropic `tool_result` block across the request's messages.
fn tool_results(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
                .filter(|block| block["type"] == "tool_result")
                .collect()
        })
        .unwrap_or_default()
}

async fn run_turn(
    config_path: &Path,
    cwd: &Path,
    data_dir: &Path,
    content: &str,
) -> (pig_core::AgentHandle, String) {
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.to_path_buf()),
        cwd.to_path_buf(),
        data_dir.to_path_buf(),
    );
    let session_id = new_session(&agent, cwd.to_path_buf()).await;
    send(&agent, &session_id, content).await;
    recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    (agent, session_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_tool_result_is_error_reflects_the_real_outcome() {
    // Success: the mock file exists, both tools succeed, no is_error.
    let (config_path, cwd, data_dir, log) = setup("ok", ApiFormat::AnthropicMessages, "", true);
    let (agent, _) = run_turn(&config_path, &cwd, &data_dir, "Read the mock file").await;
    agent.shutdown();
    let bodies = stream_bodies(&log);
    let follow_up = bodies
        .iter()
        .find(|body| !tool_results(body).is_empty())
        .expect("the step after the tool call carries a tool_result");
    let results = tool_results(follow_up);
    assert_eq!(results.len(), 1, "{follow_up}");
    assert!(
        results[0].get("is_error").is_none(),
        "a successful result omits is_error: {follow_up}"
    );

    // Failure: the file is missing, the same call fails, is_error is true.
    let (config_path, cwd, data_dir, log) = setup("err", ApiFormat::AnthropicMessages, "", false);
    let (agent, session_id) = run_turn(&config_path, &cwd, &data_dir, "Read the mock file").await;
    let bodies = stream_bodies(&log);
    let live = bodies
        .iter()
        .find(|body| !tool_results(body).is_empty())
        .expect("failure result reaches the next request")
        .clone();
    let live_result = tool_results(&live)
        .into_iter()
        .find(|block| block["tool_use_id"] == "call_mock_1")
        .expect("mock tool result");
    assert_eq!(
        live_result["is_error"],
        Value::Bool(true),
        "a failed tool sets tool_result.is_error: {live}"
    );

    // Rollout restart: the rebuilt history sends the same semantic block.
    agent.shutdown();
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    while tokio::time::timeout(Duration::from_millis(200), agent.events.recv())
        .await
        .is_ok()
    {}

    let before = log.lock().expect("log").len();
    send(&agent, &session_id, "continue").await;
    recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    let resumed_body = {
        let log = log.lock().expect("log");
        log[before..]
            .iter()
            .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
            .find(|body| body["stream"] == Value::Bool(true))
            .expect("the resumed turn sends a model request")
    };
    let resumed_result = tool_results(&resumed_body)
        .into_iter()
        .find(|block| block["tool_use_id"] == "call_mock_1")
        .expect("rebuilt tool result");
    assert_eq!(
        resumed_result, live_result,
        "the rebuilt request must keep the live is_error semantics"
    );
    agent.shutdown();
}

/// A stopped call (user interrupt) is a failure for the model: the "Stopped"
/// receipt carries is_error on the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_stopped_tool_result_is_error() {
    let (config_path, cwd, data_dir, log) = setup("stop", ApiFormat::AnthropicMessages, "", true);
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let session_id = new_session(&agent, cwd.clone()).await;
    send(&agent, &session_id, mock::SCENARIO_SLOW_TRIGGER).await;
    recv_until(
        &agent.events,
        Duration::from_secs(20),
        |event| matches!(event, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;
    agent
        .ops
        .send(Op::Interrupt {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnAborted { .. })
    })
    .await;

    let before = log.lock().expect("log").len();
    send(&agent, &session_id, "continue after the stop").await;
    recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    let body = {
        let log = log.lock().expect("log");
        log[before..]
            .iter()
            .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
            .find(|body| body["stream"] == Value::Bool(true))
            .expect("the follow-up turn sends the rebuilt history")
    };
    let stopped = tool_results(&body)
        .into_iter()
        .find(|block| block["content"] == pig_provider::STOPPED_TOOL_OUTPUT)
        .unwrap_or_else(|| panic!("stopped receipt missing: {body}"));
    assert_eq!(
        stopped["is_error"],
        Value::Bool(true),
        "a stopped tool result is an error: {body}"
    );
    agent.shutdown();
}

/// Model configuration only carries the advanced parallel override; every
/// step uses the provider's default auto choice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_tool_policy_reaches_the_core_request() {
    let extra = "parallel_tool_calls = false";
    for format in [
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
    ] {
        let (config_path, cwd, data_dir, log) = setup("policy", format, extra, true);
        let (agent, _) = run_turn(&config_path, &cwd, &data_dir, "Read the mock file").await;
        agent.shutdown();
        let bodies = stream_bodies(&log);
        assert_eq!(bodies.len(), 2, "{format:?}: {bodies:#?}");
        for body in &bodies {
            match format {
                ApiFormat::OpenAiChat => {
                    assert!(body.get("tool_choice").is_none(), "{body}");
                    assert_eq!(body["parallel_tool_calls"], Value::Bool(false), "{body}");
                }
                ApiFormat::OpenAiResponses => {
                    assert_eq!(body["tool_choice"], Value::String("auto".into()), "{body}");
                    assert_eq!(body["parallel_tool_calls"], Value::Bool(false), "{body}");
                }
                ApiFormat::AnthropicMessages => {
                    assert_eq!(
                        body["tool_choice"],
                        serde_json::json!({"type": "auto", "disable_parallel_tool_use": true}),
                        "{body}"
                    );
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_group_cancellation_persists_every_stopped_receipt() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}/slow");
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut sockets = Vec::new();
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.expect("accept FetchURL");
            sockets.push(socket);
        }
        for socket in &mut sockets {
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
        }
        let _ = started_tx.send(());
        tokio::time::sleep(Duration::from_secs(30)).await;
        for socket in &mut sockets {
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .await;
        }
    });

    let (config_path, cwd, data_dir, log) =
        setup("parallel-stop", ApiFormat::AnthropicMessages, "", true);
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let session_id = new_session(&agent, cwd.clone()).await;
    send(
        &agent,
        &session_id,
        &format!("{} {url}", mock::SCENARIO_PARALLEL_SLOW_FOLLOWUP_TRIGGER),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), started_rx.recv())
        .await
        .expect("both parallel FetchURL requests started")
        .expect("start notification");
    agent
        .ops
        .send(Op::Interrupt {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let aborted = recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnAborted { .. })
    })
    .await;
    server.abort();

    assert!(
        !cwd.join("cancel-parallel-followup-must-not-exist.txt")
            .exists(),
        "cancelled turn executed a later write: {aborted:#?}"
    );
    let ends: Vec<(bool, bool)> = aborted
        .iter()
        .filter_map(|event| match event {
            Event::ToolCallEnd {
                output,
                is_error,
                stopped,
                ..
            } if output == "Stopped" => Some((*is_error, *stopped)),
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec![(true, true); 3], "{aborted:#?}");
    assert_eq!(
        aborted
            .iter()
            .filter(|event| matches!(event, Event::ToolCallBegin { .. }))
            .count(),
        3,
        "{aborted:#?}"
    );

    let rollout = data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let records = pig_core::rollout::Rollout::load(&rollout).expect("load rollout");
    let stopped: Vec<(&str, bool, bool)> = records
        .iter()
        .filter_map(|record| match record {
            pig_core::rollout::RolloutRecord::ToolCall {
                tool,
                output,
                is_error,
                stopped,
                ..
            } if output == "Stopped" => Some((tool.as_str(), *is_error, *stopped)),
            _ => None,
        })
        .collect();
    assert_eq!(
        stopped,
        vec![
            ("FetchURL", true, true),
            ("FetchURL", true, true),
            ("Write", true, true),
        ],
        "{records:#?}"
    );

    let before = log.lock().expect("log").len();
    send(&agent, &session_id, "continue").await;
    recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    let (body, live_results) = {
        let log = log.lock().expect("log");
        let body = log[before..]
            .iter()
            .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
            .find(|body| body["stream"] == Value::Bool(true))
            .expect("live follow-up request");
        let results = tool_results(&body)
            .into_iter()
            .filter(|block| block["content"] == pig_provider::STOPPED_TOOL_OUTPUT)
            .collect::<Vec<_>>();
        (body, results)
    };
    assert_eq!(live_results.len(), 3, "{live_results:#?}; body: {body:#?}");
    agent.shutdown();

    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    while tokio::time::timeout(Duration::from_millis(200), agent.events.recv())
        .await
        .is_ok()
    {}
    let before = log.lock().expect("log").len();
    send(&agent, &session_id, "continue after restart").await;
    recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    let resumed_results = {
        let log = log.lock().expect("log");
        let body = log[before..]
            .iter()
            .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
            .find(|body| body["stream"] == Value::Bool(true))
            .expect("resumed follow-up request");
        tool_results(&body)
            .into_iter()
            .filter(|block| block["content"] == pig_provider::STOPPED_TOOL_OUTPUT)
            .collect::<Vec<_>>()
    };
    assert_eq!(resumed_results, live_results);
    agent.shutdown();
}
