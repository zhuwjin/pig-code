use crate::common;

use std::path::PathBuf;
use std::time::Duration;

use common::{new_session, recv_until};
use pig_protocol::{ApiFormat, Event, ExecMode, FinishReason, Op};
use pig_provider::mock;

fn config(port: u16, format: ApiFormat) -> String {
    let format = match format {
        ApiFormat::OpenAiChat => "OpenAiChat",
        ApiFormat::AnthropicMessages => "AnthropicMessages",
        ApiFormat::OpenAiResponses => "OpenAiResponses",
    };
    format!(
        r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock Provider"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "{format}"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
"#
    )
}

fn workspace(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pig-core-provider-reliability-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    dir
}

async fn send_and_complete(
    agent: &pig_core::AgentHandle,
    events: &async_channel::Receiver<Event>,
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
    recv_until(events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await
}

async fn open_replayed(
    agent: &pig_core::AgentHandle,
    events: &async_channel::Receiver<Event>,
    session_id: &str,
) -> Vec<Event> {
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.to_string(),
        })
        .await
        .unwrap();
    recv_until(events, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::TurnComplete {
                duration_ms: 0,
                stats: None,
                finish_reason: None,
                ..
            }
        )
    })
    .await
}

async fn assert_native_reasoning_replay(format: ApiFormat, label: &str) {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = workspace(label);
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config(port, format)).unwrap();
    let data_dir = dir.join("data");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    send_and_complete(
        &agent,
        &events,
        &session_id,
        mock::NATIVE_REASONING_REPLAY_TRIGGER,
    )
    .await;
    agent.shutdown();

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    open_replayed(&agent, &events, &session_id).await;
    send_and_complete(&agent, &events, &session_id, "continue after restart").await;
    agent.shutdown();

    let bodies = log.lock().expect("request log");
    let result_marker = match format {
        ApiFormat::OpenAiResponses => "function_call_output",
        ApiFormat::AnthropicMessages => "tool_result",
        ApiFormat::OpenAiChat => unreachable!(),
    };
    let continuation = bodies
        .iter()
        .rev()
        .find(|body| {
            body.contains(mock::NATIVE_REASONING_REPLAY_TRIGGER)
                && body.contains(result_marker)
                && body.contains("call_native_read")
        })
        .unwrap_or_else(|| panic!("missing continuation after restart: {bodies:?}"));
    match format {
        ApiFormat::OpenAiResponses => {
            assert!(continuation.contains("\"id\":\"rs_native\""));
            assert!(continuation.contains("\"encrypted_content\":\"encrypted-rs-native\""));
            assert!(continuation.contains("native encrypted reasoning"));
            assert!(continuation.contains("private reasoning content"));
            assert_eq!(
                continuation.matches("\"type\":\"function_call\"").count(),
                1
            );
        }
        ApiFormat::AnthropicMessages => {
            assert!(continuation.contains("\"signature\":\"sig-native\""));
            assert!(continuation.contains("\"type\":\"redacted_thinking\""));
            assert!(continuation.contains("\"data\":\"redacted-native\""));
            assert!(continuation.contains("signed native reasoning"));
            assert_eq!(continuation.matches("\"type\":\"tool_use\"").count(), 1);
        }
        ApiFormat::OpenAiChat => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_native_reasoning_replays_after_restart() {
    assert_native_reasoning_replay(ApiFormat::OpenAiResponses, "responses").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_native_reasoning_replays_after_restart() {
    assert_native_reasoning_replay(ApiFormat::AnthropicMessages, "anthropic").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_tool_only_replay_preserves_call_id_after_restart() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = workspace("tool-only");
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config(port, ApiFormat::OpenAiResponses)).unwrap();
    let data_dir = dir.join("data");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    send_and_complete(&agent, &events, &session_id, mock::TOOL_ONLY_REPLAY_TRIGGER).await;
    agent.shutdown();

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    open_replayed(&agent, &events, &session_id).await;
    send_and_complete(&agent, &events, &session_id, "continue tool-only replay").await;
    agent.shutdown();

    let bodies = log.lock().expect("request log");
    let continuation = bodies
        .iter()
        .rev()
        .find(|body| {
            body.contains(mock::TOOL_ONLY_REPLAY_TRIGGER)
                && body.contains("function_call_output")
                && body.contains("call_native_read")
        })
        .expect("tool-only continuation");
    assert_eq!(
        continuation.matches("\"type\":\"function_call\"").count(),
        1
    );
    assert!(!continuation.contains("replay-"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_length_replay_starts_assistant_before_warning() {
    let port = mock::start_mock_server();
    let dir = workspace("empty-length");
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config(port, ApiFormat::OpenAiResponses)).unwrap();
    let data_dir = dir.join("data");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    send_and_complete(&agent, &events, &session_id, mock::EMPTY_LENGTH_TRIGGER).await;
    agent.shutdown();

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir, data_dir);
    let events = agent.events.clone();
    let replayed = open_replayed(&agent, &events, &session_id).await;
    let started = replayed
        .iter()
        .position(|event| matches!(event, Event::TurnStarted { .. }))
        .expect("assistant starts before warning");
    let warning = replayed
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::TurnComplete {
                    finish_reason: Some(FinishReason::Length),
                    ..
                }
            )
        })
        .expect("length warning is replayed");
    assert!(started < warning);
    agent.shutdown();
}

async fn assert_incomplete_finish(
    format: ApiFormat,
    trigger: &str,
    reason: FinishReason,
    label: &str,
) {
    let port = mock::start_mock_server();
    let dir = workspace(label);
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config(port, format)).unwrap();
    let data_dir = dir.join("data");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    let collected = send_and_complete(&agent, &events, &session_id, trigger).await;

    assert!(collected.iter().any(|event| matches!(
        event,
        Event::TextDone { full_text, .. } if full_text.contains("partial answer before abnormal finish")
    )));
    assert!(collected.iter().any(|event| matches!(
        event,
        Event::TurnComplete { finish_reason: Some(actual), .. } if *actual == reason
    )));
    assert!(
        collected
            .iter()
            .any(|event| matches!(event, Event::ToolInputDiscard { .. }))
    );
    assert!(!collected.iter().any(|event| matches!(
        event,
        Event::ToolCallBegin { .. } | Event::ToolCallEnd { .. }
    )));
    assert!(!dir.join(mock::INCOMPLETE_TOOL_FILE).exists());
    agent.shutdown();

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir, data_dir);
    let events = agent.events.clone();
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let replayed = recv_until(&events, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::TurnComplete { finish_reason: Some(actual), .. } if *actual == reason
        )
    })
    .await;
    assert!(replayed.iter().any(|event| matches!(
        event,
        Event::TurnComplete { stats: None, finish_reason: Some(actual), .. } if *actual == reason
    )));
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn length_finish_is_safe_and_durable_across_formats() {
    for (format, label) in [
        (ApiFormat::OpenAiChat, "chat"),
        (ApiFormat::OpenAiResponses, "responses"),
        (ApiFormat::AnthropicMessages, "anthropic"),
    ] {
        assert_incomplete_finish(
            format,
            mock::FINISH_LENGTH_TRIGGER,
            FinishReason::Length,
            &format!("length-{label}"),
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_filter_finish_is_safe_and_durable_across_formats() {
    for (format, label) in [
        (ApiFormat::OpenAiChat, "chat"),
        (ApiFormat::OpenAiResponses, "responses"),
        (ApiFormat::AnthropicMessages, "anthropic"),
    ] {
        assert_incomplete_finish(
            format,
            mock::FINISH_CONTENT_FILTER_TRIGGER,
            FinishReason::ContentFilter,
            &format!("content-filter-{label}"),
        )
        .await;
    }
}

async fn assert_protocol_failure(format: ApiFormat, trigger: &str, expected: &str, label: &str) {
    let port = mock::start_mock_server();
    let dir = workspace(label);
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config(port, format)).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: trigger.into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(10), |event| {
        matches!(event, Event::Error { .. })
    })
    .await;
    let detail = collected
        .iter()
        .find_map(|event| match event {
            Event::Error { error, .. } => Some(pig_provider::core_error_en(error)),
            _ => None,
        })
        .expect("structured provider error");
    assert!(detail.contains(expected), "unexpected error: {detail}");
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_finish_reason_is_an_error_across_formats() {
    for (format, label) in [
        (ApiFormat::OpenAiChat, "chat"),
        (ApiFormat::OpenAiResponses, "responses"),
        (ApiFormat::AnthropicMessages, "anthropic"),
    ] {
        assert_protocol_failure(
            format,
            mock::MISSING_FINISH_TRIGGER,
            "without",
            &format!("missing-finish-{label}"),
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_json_is_an_error_across_formats() {
    for (format, label) in [
        (ApiFormat::OpenAiChat, "chat"),
        (ApiFormat::OpenAiResponses, "responses"),
        (ApiFormat::AnthropicMessages, "anthropic"),
    ] {
        assert_protocol_failure(
            format,
            mock::MALFORMED_STREAM_TRIGGER,
            "Invalid JSON",
            &format!("malformed-{label}"),
        )
        .await;
    }
}

/// Hosted search: normalized sources come from the durable native replay,
/// attach to the exact TextDone item, and survive a restart even after the
/// model switches to a different API origin (the native block itself is not
/// replayed there, but the UI metadata still is).
async fn assert_sources_roundtrip(
    format: ApiFormat,
    trigger: &str,
    label: &str,
    expected: &[(&str, &str)],
) {
    let (port, _log) = mock::start_mock_server_with_log();
    let dir = workspace(&format!("sources-{label}"));
    let config_path = dir.join("config.toml");
    let format_s = match format {
        ApiFormat::OpenAiChat => "OpenAiChat",
        ApiFormat::OpenAiResponses => "OpenAiResponses",
        ApiFormat::AnthropicMessages => "AnthropicMessages",
    };
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
api_format = "{format_s}"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
cap_web_search = true

[[providers]]
id = "chat"
name = "Chat"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "chat-model"
context_window = 128000
max_output_tokens = 8192
"#
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    let turn = send_and_complete(&agent, &events, &session_id, trigger).await;
    let Some((text_item, _)) = turn.iter().find_map(|event| match event {
        Event::TextDone {
            item_id, full_text, ..
        } => Some((item_id.clone(), full_text.clone())),
        _ => None,
    }) else {
        panic!("{label}: no TextDone: {turn:#?}");
    };
    let sources = turn
        .iter()
        .find_map(|event| match event {
            Event::Sources {
                item_id, sources, ..
            } if *item_id == text_item => Some(sources.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{label}: no Sources for {text_item}: {turn:#?}"));
    for (url, title) in expected {
        let source = sources
            .iter()
            .find(|source| source.url == *url)
            .unwrap_or_else(|| panic!("{label}: missing {url}: {sources:#?}"));
        assert_eq!(source.title, *title, "{label}: {sources:#?}");
    }
    // Source order/identity is stable and never exposes opaque fields (the
    // protocol type has no encrypted slot, but keep the assertion explicit).
    let serialized = serde_json::to_string(&sources).unwrap();
    assert!(!serialized.contains("encrypted"), "{serialized}");

    // Switch to a different origin before restarting: the stored native
    // replay is no longer same-format, yet replay must still derive sources.
    agent
        .ops
        .send(Op::SetModel {
            session_id: session_id.clone(),
            provider_id: "chat".into(),
            model_id: "chat-model".into(),
            reasoning_level: None,
        })
        .await
        .unwrap();
    agent.ops.send(Op::ListSessions).await.unwrap();
    let _ = recv_until(&events, Duration::from_secs(5), |event| {
        matches!(event, Event::SessionList { .. })
    })
    .await;
    agent.shutdown();

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let replay = recv_until(&events, Duration::from_secs(10), |event| {
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
    let replayed_text = replay.iter().find_map(|event| match event {
        Event::TextDone { item_id, .. } => Some(item_id.clone()),
        _ => None,
    });
    let Some(replayed_text) = replayed_text else {
        panic!("{label}: replay has no assistant text: {replay:#?}");
    };
    assert!(
        replay.iter().any(|event| matches!(
            event,
            Event::Sources { item_id, sources, .. }
                if item_id == &replayed_text
                    && expected.iter().all(|(url, _)| sources.iter().any(|source| source.url == *url))
        )),
        "{label}: replay from a different origin keeps the sources: {replay:#?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_sources_survive_replay_across_origins() {
    assert_sources_roundtrip(
        ApiFormat::OpenAiResponses,
        mock::RESPONSES_WEB_SEARCH_TRIGGER,
        "responses",
        &[
            ("https://example.com/action", "Action source"),
            ("https://example.com/cite", "Citation"),
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_sources_survive_replay_across_origins() {
    assert_sources_roundtrip(
        ApiFormat::AnthropicMessages,
        mock::ANTHROPIC_WEB_SEARCH_TRIGGER,
        "anthropic",
        &[
            ("https://example.com/action", "Action source"),
            ("https://example.com/cite", "Citation"),
        ],
    )
    .await;
}
