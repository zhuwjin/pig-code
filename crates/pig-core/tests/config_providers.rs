mod common;

use common::{new_session, recv_until};
use pig_protocol::{ApiFormat, Event, ExecMode, ModelConfig, Op, ProviderConfig};
use pig_provider::mock;
use std::time::Duration;

fn v2_config(port: u16, format: ApiFormat) -> String {
    let format_str = match format {
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
api_format = "{format_str}"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
"#
    )
}

/// config v2 roundtrip: GetConfig / SaveConfig
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_v2_snapshot_and_save() {
    let port = mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-core-m8-v2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, v2_config(port, ApiFormat::OpenAiChat)).unwrap();
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        dir.join("data"),
    );
    let events = agent.events.clone();
    let _sid = new_session(&agent, dir.clone()).await;

    agent.ops.send(Op::GetConfig).await.unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::ConfigSnapshot { .. })
    })
    .await;
    let Some(Event::ConfigSnapshot { config }) = collected.last() else {
        panic!()
    };
    assert_eq!(config.providers.len(), 1);
    assert_eq!(config.providers[0].name, "Mock Provider");
    assert_eq!(config.providers[0].models[0].context_window, 128000);

    // Add a provider and save
    let mut config = config.clone();
    config.providers.push(ProviderConfig {
        id: "second".into(),
        name: "Second provider".into(),
        base_url: "http://127.0.0.1:1".into(),
        api_key: "${TEST_NONEXISTENT_KEY}".into(),
        api_format: ApiFormat::AnthropicMessages,
        enabled: true,
        models: vec![ModelConfig::new("claude-mock", 200_000, 8_192)],
        key_url: None,
    });
    agent
        .ops
        .send(Op::SaveConfig {
            config: config.clone(),
        })
        .await
        .unwrap();
    let collected = recv_until(
        &events,
        Duration::from_secs(5),
        |e| matches!(e, Event::ConfigSnapshot { config, .. } if config.providers.len() == 2),
    )
    .await;
    assert!(
        matches!(collected.last(), Some(Event::ConfigSnapshot { config, .. }) if config.providers[1].name == "Second provider"),
        "new snapshot should be sent back after save"
    );
    let raw = std::fs::read_to_string(&config_path).unwrap();
    assert!(raw.contains("Second provider"), "persisted to disk: {raw}");
    assert!(
        raw.contains("${TEST_NONEXISTENT_KEY}"),
        "api_key must not be expanded: {raw}"
    );
    agent.shutdown();
}

/// A full turn in Anthropic format: Read tool call → result → text (via /v1/messages + Anthropic SSE)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_full_turn() {
    let port = mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-core-m8-anthropic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, v2_config(port, ApiFormat::AnthropicMessages)).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
    let events = agent.events.clone();
    let sid = new_session(&agent, dir).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid,
            content: "Read the mock file and summarize".into(),
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
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ReasoningDelta { .. })),
        "Anthropic thinking_delta → reasoning"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. } if output.contains("known file")
        )),
        "tool_use tool chain: {collected:#?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::MOCK_REPLY_MARKER)
        )),
        "text_delta text"
    );
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ContextUsage { used: 142, .. })),
        "usage aggregated 100+42"
    );
    agent.shutdown();
}

/// reasoning_params merge: SetModel with a reasoning level → the request body should contain the matching JSON
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_params_merged() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-m8-reason-{}", std::process::id()));
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
reasoning_levels = ["low", "high"]

[providers.models.reasoning_params]
low = {{ reasoning_effort = "low" }}
high = {{ reasoning_effort = "high" }}
"#
        ),
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
    let events = agent.events.clone();
    let sid = new_session(&agent, dir).await;

    agent
        .ops
        .send(Op::SetModel {
            session_id: sid.clone(),
            provider_id: "mock".into(),
            model_id: "mock-model".into(),
            reasoning_level: Some("high".into()),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid,
            content: "Read the mock file and summarize".into(),
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

    let bodies = log.lock().expect("log");
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("\"reasoning_effort\":\"high\"")),
        "request body should merge reasoning_params: {:?}",
        bodies.last()
    );
    agent.shutdown();
}

/// TestProvider: succeeds against the mock (Connected with HTTP status), fails against a dead port (Failed)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_provider_ok_and_fail() {
    let port = mock::start_mock_server();
    let ok = pig_provider::test_provider(
        &format!("http://127.0.0.1:{port}/v1"),
        "mock-key",
        ApiFormat::OpenAiChat,
        "mock-model",
    )
    .await;
    assert!(
        matches!(ok, pig_protocol::ConnTestResult::Connected { .. }),
        "{ok:?}"
    );

    let ok = pig_provider::test_provider(
        &format!("http://127.0.0.1:{port}/v1"),
        "mock-key",
        ApiFormat::AnthropicMessages,
        "mock-model",
    )
    .await;
    assert!(
        matches!(ok, pig_protocol::ConnTestResult::Connected { .. }),
        "Anthropic ping: {ok:?}"
    );

    let fail =
        pig_provider::test_provider("http://127.0.0.1:1", "x", ApiFormat::OpenAiChat, "x").await;
    assert!(matches!(fail, pig_protocol::ConnTestResult::Failed { .. }));
}

/// Exponential backoff retry: the first request gets a 500 from the mock → auto-retry → completes normally without surfacing an Error event
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_on_server_error() {
    let port = mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-core-m8-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, v2_config(port, ApiFormat::AnthropicMessages)).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
    let events = agent.events.clone();
    let sid = new_session(&agent, dir).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid,
            content: "FAIL_ONCE_500 read the mock file".into(),
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
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::TextDone { .. })),
        "should auto-retry after 500 and complete: {collected:#?}"
    );
    assert!(
        !collected.iter().any(|e| matches!(e, Event::Error { .. })),
        "retryable errors must not surface an Error event: {collected:#?}"
    );
    agent.shutdown();
}

/// Anthropic thinking mode: continuation-turn requests must pass the thinking
/// block back in the assistant history (DeepSeek's /anthropic endpoint returns
/// 400 without it: content[].thinking must be passed back)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_thinking_echoed() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-thinking-echo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, v2_config(port, ApiFormat::AnthropicMessages)).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
    let events = agent.events.clone();
    let sid = new_session(&agent, dir).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid,
            content: "Read the mock file and summarize".into(),
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

    let bodies = log.lock().expect("log");
    // The continuation request with tool results: the assistant history must contain a thinking block, placed before tool_use
    let continuation = bodies
        .iter()
        .find(|body| body.contains("tool_result"))
        .unwrap_or_else(|| panic!("expected a continuation request with tool_result: {bodies:?}"));
    let thinking_pos = continuation.find("\"type\":\"thinking\"");
    let tool_use_pos = continuation.find("\"type\":\"tool_use\"");
    assert!(
        thinking_pos.is_some(),
        "continuation request is missing the thinking block: {continuation}"
    );
    assert!(
        continuation.contains(mock::MOCK_REASONING),
        "thinking block should contain the original reasoning text: {continuation}"
    );
    assert!(
        thinking_pos < tool_use_pos,
        "thinking must come before tool_use: {continuation}"
    );
    agent.shutdown();
}

/// A full turn in Responses format: reasoning summary → Read function_call →
/// function_call_output replay → text (via /responses + Responses SSE events)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_full_turn() {
    let port = mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-core-resp-turn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, v2_config(port, ApiFormat::OpenAiResponses)).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
    let events = agent.events.clone();
    let sid = new_session(&agent, dir).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid,
            content: "Read the mock file and summarize".into(),
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
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ReasoningDelta { .. })),
        "reasoning_summary_text.delta → reasoning"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. } if output.contains("known file")
        )),
        "function_call tool chain: {collected:#?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::MOCK_REPLY_MARKER)
        )),
        "output_text.delta text"
    );
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ContextUsage { used: 142, .. })),
        "usage aggregated 100+42"
    );
    agent.shutdown();
}

/// Responses continuation requests must replay the turn as input items: the
/// system prompt lands in top-level `instructions`, the tool result becomes a
/// `function_call_output` item, and reasoning is echoed as a plaintext summary
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_continuation_replays_items() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-resp-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, v2_config(port, ApiFormat::OpenAiResponses)).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
    let events = agent.events.clone();
    let sid = new_session(&agent, dir).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid,
            content: "Read the mock file and summarize".into(),
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

    let bodies = log.lock().expect("log");
    let continuation = bodies
        .iter()
        .find(|body| body.contains("function_call_output"))
        .unwrap_or_else(|| {
            panic!("expected a continuation request with function_call_output: {bodies:?}")
        });
    assert!(
        continuation.contains("\"instructions\":"),
        "system prompt hoisted into instructions: {continuation}"
    );
    assert!(
        continuation.contains("\"store\":false"),
        "stateless replay (store:false): {continuation}"
    );
    assert!(
        continuation.contains("\"type\":\"function_call\""),
        "the assistant tool call replays as a function_call item: {continuation}"
    );
    assert!(
        continuation.contains("\"type\":\"reasoning\""),
        "reasoning echoes as a plaintext summary item: {continuation}"
    );
    agent.shutdown();
}
