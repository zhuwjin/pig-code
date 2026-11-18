use crate::common;

use common::{new_session, recv_until, setup_with_log};
use pig_protocol::{ApiFormat, AppConfig, Event, ExecMode, ModelConfig, Op, ProviderConfig};
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
    assert_eq!(
        config.providers[0].icon, None,
        "old configs without icon load as auto-match"
    );
    assert_eq!(
        config.providers[0].models[0].icon, None,
        "old configs without a model icon load as auto-match"
    );

    // Add a provider and save; the model carries an optional display name and
    // an explicit brand icon
    let mut config = config.clone();
    let mut model = ModelConfig::new("claude-mock", 200_000, 8_192);
    model.name = Some("Claude Display".into());
    model.icon = Some("claude".into());
    config.providers.push(ProviderConfig {
        id: "second".into(),
        name: "Second provider".into(),
        base_url: "http://127.0.0.1:1".into(),
        requires_api_key: true,
        api_format: ApiFormat::AnthropicMessages,
        enabled: true,
        models: vec![model],
        icon: Some("anthropic".into()),
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
        matches!(collected.last(), Some(Event::ConfigSnapshot { config, .. }) if config.providers[1].name == "Second provider"
            && config.providers[1].models[0].icon.as_deref() == Some("claude")),
        "new snapshot should be sent back after save, including the model icon"
    );
    let raw = std::fs::read_to_string(&config_path).unwrap();
    assert!(raw.contains("Second provider"), "persisted to disk: {raw}");
    assert!(
        raw.contains(r#"icon = "anthropic""#),
        "provider icon choice persisted to disk: {raw}"
    );
    assert!(
        raw.contains("Claude Display"),
        "model display name persisted to disk: {raw}"
    );
    assert!(
        raw.contains(r#"icon = "claude""#),
        "model icon choice persisted to disk: {raw}"
    );
    assert!(
        raw.contains("requires_api_key = true"),
        "the authentication requirement is persisted without a secret: {raw}"
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
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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

/// Connectivity uses the same credential resolution as real requests: a
/// missing required key is a typed nonfatal error, never an empty-key request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_provider_reports_a_missing_credential() {
    let dir = std::env::temp_dir().join(format!("pig-core-missing-key-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock Provider"
base_url = "http://127.0.0.1:1/v1/"
requires_api_key = true
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
"#,
    )
    .unwrap();
    let agent = pig_core::spawn_agent_with_data_dir_and_credentials(
        Some(config_path),
        dir.clone(),
        dir.join("data"),
        std::sync::Arc::new(pig_core::credentials::CredentialManager::empty_memory()),
    );
    let events = agent.events.clone();
    agent
        .ops
        .send(Op::TestProvider {
            provider_id: "mock".to_string(),
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |event| {
        matches!(event, Event::Error { .. } | Event::TestResult { .. })
    })
    .await;
    assert!(
        collected.iter().any(|event| matches!(
            event,
            Event::Error { error, fatal: false, .. }
                if matches!(error.semantic_error(), pig_protocol::CoreError::ProviderConfig { detail } if detail.contains("credential not found"))
        )),
        "the missing credential is named without exposing a value: {collected:#?}"
    );
    assert!(
        !collected
            .iter()
            .any(|event| matches!(event, Event::TestResult { .. })),
        "no connection attempt is made with a missing key"
    );
    agent.shutdown();
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
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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

/// Title generation on a cap_structured model: the sidecar request carries
/// the native json_schema constraint and the title still lands
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_title_carries_native_schema() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-struct-title-{}", std::process::id()));
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
cap_structured = true
"#
        ),
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
    let events = agent.events.clone();
    let sid = new_session(&agent, dir.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: "Read the mock file and summarize".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::SessionTitleChanged { .. })
    })
    .await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::SessionTitleChanged { title, .. } if title == mock::MOCK_TITLE
        )),
        "the auto title should still land: {collected:#?}"
    );
    let bodies = log.lock().expect("log");
    let title_request = bodies
        .iter()
        .find(|body| body.contains(pig_protocol::TITLE_PROMPT_MARKER))
        .expect("title sidecar request in the log");
    assert!(
        title_request.contains("\"response_format\""),
        "cap_structured rides the native constraint: {title_request}"
    );
    assert!(
        title_request.contains("\"json_schema\"") && title_request.contains("\"session_title\""),
        "the title schema is named: {title_request}"
    );
    assert!(
        title_request.contains("\"strict\":true"),
        "strict mode: {title_request}"
    );
    drop(bodies);
    let db = rusqlite::Connection::open(dir.join("data/store.sqlite")).unwrap();
    let row = db.query_row(
        "SELECT usage_reported, input_tokens, output_tokens, finish, http_status,
                request_id, trace_id, response_id
         FROM api_usage WHERE session_id = ?1 AND source = 'title'",
        rusqlite::params![sid],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        },
    );
    let (reported, input, output, finish, status, request_id, trace_id, response_id) = row.unwrap();
    assert_eq!((reported, input, output), (1, 50, 10));
    assert_eq!(finish, "stop");
    assert_eq!(status, 200);
    assert_eq!(request_id.as_deref(), Some(mock::MOCK_REQUEST_ID));
    assert_eq!(trace_id.as_deref(), Some(mock::MOCK_TRACE_ID));
    assert_eq!(response_id.as_deref(), Some("chatcmpl-mock"));
    agent.shutdown();
}

/// cap_strict_tools on a chat-format model: the streaming request's function
/// tools carry strict + the strictified schema shape
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_tools_ride_the_request() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-strict-tools-{}", std::process::id()));
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
cap_strict_tools = true
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
        .send(Op::SendMessage {
            session_id: sid,
            content: "Read the mock file and summarize".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let bodies = log.lock().expect("log");
    let streaming = bodies
        .iter()
        .find(|body| body.contains("\"stream\":true"))
        .expect("a streaming request in the log");
    let body: serde_json::Value = serde_json::from_str(streaming).expect("request body JSON");
    let tools = body["tools"].as_array().expect("tools array");
    let strict = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["function"]["name"] == name)
            .unwrap_or_else(|| panic!("missing tool {name}"))
            .clone()
    };
    for name in ["Read", "Write", "Edit", "ExecCommand"] {
        let tool = strict(name);
        assert_eq!(
            tool["function"]["strict"],
            serde_json::json!(true),
            "{name} opts into strict"
        );
        assert_eq!(
            tool["function"]["parameters"]["additionalProperties"],
            serde_json::json!(false),
            "{name}'s strictified schema rides along"
        );
    }
    for name in ["Grep", "Glob", "ReadMediaFile", "FetchURL", "TodoList"] {
        let tool = strict(name);
        assert!(
            tool["function"].get("strict").is_none(),
            "undeclared {name} must keep its ordinary schema"
        );
    }
    agent.shutdown();
}

/// Endpoint capability switches are resolved from the exact host, and the
/// pinned model's own capability flags are copied (not hardcoded false).
#[test]
fn resolve_model_copies_endpoint_features_and_pinned_capabilities() {
    let mut model = ModelConfig::new("m", 200_000, 8_192);
    model.input_pdf = true;
    model.cap_structured = true;
    model.cap_strict_tools = true;
    model.parallel_tool_calls = false;
    let provider = |base_url: &str, format: ApiFormat| AppConfig {
        providers: vec![ProviderConfig {
            id: "p".into(),
            name: "P".into(),
            base_url: base_url.into(),
            requires_api_key: true,
            api_format: format,
            enabled: true,
            models: vec![model.clone()],
            icon: None,
            key_url: None,
        }],
        default_provider: "p".into(),
        default_model: "m".into(),
        ..AppConfig::default()
    };
    let resolve = |config: AppConfig| {
        pig_core::config::RuntimeConfig::from_snapshot(
            config,
            std::env::temp_dir().as_path(),
            std::sync::Arc::new(pig_core::credentials::CredentialManager::mock()),
        )
        .resolve_model(None, None)
        .expect("model resolves")
    };
    let anthropic = resolve(provider(
        "https://api.anthropic.com",
        ApiFormat::AnthropicMessages,
    ));
    assert!(anthropic.anthropic_cache_control);
    assert!(anthropic.replay_native_search);
    assert!(anthropic.input_pdf && anthropic.cap_structured && anthropic.cap_strict_tools);
    assert!(!anthropic.tool_settings.parallel_tool_calls);
    assert_eq!(anthropic.prompt_cache_key, None);

    let responses = resolve(provider(
        "https://api.openai.com/v1",
        ApiFormat::OpenAiResponses,
    ));
    assert!(!responses.anthropic_cache_control);
    assert!(responses.replay_native_search);

    let local = resolve(provider("http://127.0.0.1:1/v1", ApiFormat::OpenAiChat));
    assert!(!local.anthropic_cache_control && !local.replay_native_search);
    assert_eq!(local.prompt_cache_key, None);
}

/// With the capability off, no tool declares strict even though the same
/// tools opt in; the request is the ordinary schema (provider downgrade path).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_declaration_downgrades_when_the_capability_is_off() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-strict-off-{}", std::process::id()));
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
cap_strict_tools = false
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
        .send(Op::SendMessage {
            session_id: sid,
            content: "Read the mock file and summarize".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let bodies = log.lock().expect("log");
    let streaming = bodies
        .iter()
        .find(|body| body.contains("\"stream\":true"))
        .expect("a streaming request in the log");
    assert!(
        !streaming.contains("\"strict\":true"),
        "capability off downgrades every declaration: {streaming}"
    );
    agent.shutdown();
}

/// Provider credentials never round-trip through GetConfig or SaveConfig:
/// the secret stays in the credential store while ordinary metadata persists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_key_never_roundtrips_through_get_and_save_config() {
    let secret = "sk-e2e-secret-never-printed";
    let dir = std::env::temp_dir().join(format!("pig-core-key-roundtrip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock Provider"
base_url = "http://127.0.0.1:1/v1/"
requires_api_key = true
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
"#,
    )
    .unwrap();
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        dir.join("data"),
    );
    let events = agent.events.clone();

    agent.ops.send(Op::GetConfig).await.unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |event| {
        matches!(event, Event::ConfigSnapshot { .. })
    })
    .await;
    let Some(Event::ConfigSnapshot { config }) = collected.last() else {
        panic!("config snapshot");
    };
    assert!(config.providers[0].requires_api_key);
    assert_eq!(
        config.providers[0].base_url, "http://127.0.0.1:1/v1/",
        "the stored URL is returned as written; normalization is a runtime step"
    );
    let snapshot = serde_json::to_value(config).unwrap();
    assert!(snapshot["providers"][0].get("api_key").is_none());

    agent
        .ops
        .send(Op::SetProviderApiKey {
            provider_id: "mock".into(),
            secret: pig_protocol::SecretString::new(secret.into()),
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(5), |event| {
        matches!(
            event,
            Event::ProviderCredentials { credentials }
                if credentials.get("mock") == Some(&pig_protocol::ProviderCredentialStatus::Configured)
        )
    })
    .await;
    let raw = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        !raw.lines()
            .any(|line| line.trim_start().starts_with("api_key ="))
    );
    assert!(!raw.contains(secret));

    let snapshot = config.clone();
    agent
        .ops
        .send(Op::SaveConfig { config: snapshot })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(5), |event| {
        matches!(event, Event::ConfigSnapshot { .. })
    })
    .await;
    let raw = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        !raw.lines()
            .any(|line| line.trim_start().starts_with("api_key ="))
    );
    assert!(!raw.contains(secret));

    agent.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// SaveConfig refreshes the Session's stored RuntimeConfig, not just the
/// runner's out-of-hand clone: the next turn resolves from the edited default
/// model even though no session reopen happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_config_refreshes_the_stored_session_runtime() {
    let (config_path, cwd, data_dir, log) = setup_with_log("save-runtime");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    agent.ops.send(Op::GetConfig).await.unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |event| {
        matches!(event, Event::ConfigSnapshot { .. })
    })
    .await;
    let Some(Event::ConfigSnapshot { config }) = collected.last() else {
        panic!("config snapshot");
    };
    let mut config = config.clone();
    config.providers[0]
        .models
        .push(ModelConfig::new("next-model", 128_000, 8_192));
    config.default_model = "next-model".to_string();
    agent
        .ops
        .send(Op::SaveConfig {
            config: config.clone(),
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(5), |event| {
        matches!(event, Event::ConfigSnapshot { .. })
    })
    .await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid,
            content: "use the saved model".to_string(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;

    let requests = log.lock().unwrap();
    let last = requests
        .iter()
        .rev()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .find(|request| {
            request["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
        })
        .expect("conversation request");
    assert_eq!(
        last["model"].as_str(),
        Some("next-model"),
        "SaveConfig must update the Session's stored runtime"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compatible_provider_registration_executes_without_runtime_special_cases() {
    let (config_path, cwd, data_dir, _original_log) = setup_with_log("registered-provider");
    let (port, requests) = mock::start_mock_server_with_log();
    let mut config: AppConfig = toml::from_str(
        &std::fs::read_to_string(&config_path).expect("read initial provider config"),
    )
    .expect("decode initial provider config");
    config.providers.push(ProviderConfig {
        id: "registered-compatible".into(),
        name: "Registered Compatible".into(),
        base_url: format!("http://127.0.0.1:{port}/v1"),
        requires_api_key: false,
        api_format: ApiFormat::OpenAiChat,
        enabled: true,
        models: vec![ModelConfig::new("registered-model", 128_000, 8_192)],
        icon: None,
        key_url: None,
    });
    config.default_provider = "registered-compatible".into();
    config.default_model = "registered-model".into();
    std::fs::write(
        &config_path,
        toml::to_string_pretty(&config).expect("serialize registered provider"),
    )
    .expect("register compatible provider");

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let session_id = new_session(&agent, cwd).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: "say something".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .expect("send through registered provider");
    let events = recv_until(&agent.events, Duration::from_secs(20), |event| {
        matches!(event, Event::TurnComplete { .. })
    })
    .await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Error { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::TextDone { full_text, .. } if full_text.contains(mock::MOCK_REPLY_MARKER)
    )));
    assert!(
        requests
            .lock()
            .expect("provider requests")
            .iter()
            .any(|body| { body.contains("registered-model") })
    );
    agent
        .close()
        .await
        .expect("close registered provider agent");
}
