mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{ApiFormat, Event, ExecMode, ModelConfig, Op, ProviderConfig};
use std::time::Duration;

fn v2_config(port: u16, format: ApiFormat) -> String {
    let format_str = match format {
        ApiFormat::OpenAiChat => "OpenAiChat",
        ApiFormat::AnthropicMessages => "AnthropicMessages",
    };
    format!(
        r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock 供应商"
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

/// 旧格式 [provider] 自动迁移到新格式
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_config_migration() {
    // setup() 写的是旧格式
    let (config_path, cwd, data_dir) = setup("m8-migrate");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path.clone()), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;
    let _ = sid;

    let raw = std::fs::read_to_string(&config_path).unwrap();
    assert!(raw.contains("[[providers]]"), "应迁移为新格式: {raw}");
    assert!(raw.contains("默认供应商"), "{raw}");

    // 迁移后可正常对话
    let (config_path2, cwd2, data_dir2) = (config_path.clone(), agent, events);
    let _ = (config_path2, cwd2, data_dir2);
}

/// config v2 roundtrip：GetConfig / SaveConfig
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
    assert_eq!(config.providers[0].name, "Mock 供应商");
    assert_eq!(config.providers[0].models[0].context_window, 128000);

    // 加一个供应商并保存
    let mut config = config.clone();
    config.providers.push(ProviderConfig {
        id: "second".into(),
        name: "第二个".into(),
        base_url: "http://127.0.0.1:1".into(),
        api_key: "${TEST_NONEXISTENT_KEY}".into(),
        api_format: ApiFormat::AnthropicMessages,
        enabled: true,
        models: vec![ModelConfig::new("claude-mock", 200_000, 8_192)],
    });
    agent
        .ops
        .send(Op::SaveConfig {
            config: config.clone(),
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::ConfigSnapshot { config, .. } if config.providers.len() == 2)
    })
    .await;
    assert!(
        matches!(collected.last(), Some(Event::ConfigSnapshot { config, .. }) if config.providers[1].name == "第二个"),
        "保存后应回发新快照"
    );
    let raw = std::fs::read_to_string(&config_path).unwrap();
    assert!(raw.contains("第二个"), "落盘: {raw}");
    assert!(raw.contains("${TEST_NONEXISTENT_KEY}"), "api_key 不应被展开: {raw}");
    agent.shutdown();
}

/// Anthropic 格式完整 turn：read_file 工具调用 → 结果 → 文本（走 /v1/messages + Anthropic SSE）
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_full_turn() {
    let port = mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!("pig-core-m8-anthropic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, v2_config(port, ApiFormat::AnthropicMessages)).unwrap();
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path),
        dir.clone(),
        dir.join("data"),
    );
    let events = agent.events.clone();
    let sid = new_session(&agent, dir).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid,
            content: "读一下 mock 文件并总结".into(),
            files: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    assert!(
        collected.iter().any(|e| matches!(e, Event::ReasoningDelta { .. })),
        "Anthropic thinking_delta → reasoning"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. } if output.contains("已知文件")
        )),
        "tool_use 工具链: {collected:#?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::MOCK_REPLY_MARKER)
        )),
        "text_delta 文本"
    );
    assert!(
        collected.iter().any(|e| matches!(e, Event::ContextUsage { used: 142, .. })),
        "usage 汇总 100+42"
    );
    agent.shutdown();
}

/// reasoning_params merge：SetModel 带推理等级 → 请求体应包含对应 JSON
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
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path),
        dir.clone(),
        dir.join("data"),
    );
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
            content: "读一下 mock 文件并总结".into(),
            files: vec![],
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
        bodies.iter().any(|body| body.contains("\"reasoning_effort\":\"high\"")),
        "请求体应 merge reasoning_params: {:?}",
        bodies.last()
    );
    agent.shutdown();
}

/// TestProvider：对 mock 成功，对死端口失败
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_provider_ok_and_fail() {
    let port = mock::start_mock_server();
    let ok = pig_core::provider::test_provider(
        &format!("http://127.0.0.1:{port}/v1"),
        "mock-key",
        ApiFormat::OpenAiChat,
        "mock-model",
    )
    .await;
    assert!(ok.is_ok(), "{ok:?}");

    let ok = pig_core::provider::test_provider(
        &format!("http://127.0.0.1:{port}/v1"),
        "mock-key",
        ApiFormat::AnthropicMessages,
        "mock-model",
    )
    .await;
    assert!(ok.is_ok(), "Anthropic ping: {ok:?}");

    let fail = pig_core::provider::test_provider(
        "http://127.0.0.1:1",
        "x",
        ApiFormat::OpenAiChat,
        "x",
    )
    .await;
    assert!(fail.is_err());
}

/// 指数退避重试：首个请求被 mock 返回 500 → 自动重试 → 正常完成，不冒 Error 事件
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
            content: "FAIL_ONCE_500 读一下 mock 文件".into(),
            files: vec![],
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
        "500 后应自动重试并完成: {collected:#?}"
    );
    assert!(
        !collected.iter().any(|e| matches!(e, Event::Error { .. })),
        "可重试的错误不应冒出 Error 事件: {collected:#?}"
    );
    agent.shutdown();
}

/// Anthropic thinking 模式：续轮请求的 assistant 历史必须回传 thinking 块
///（DeepSeek /anthropic 端点缺了会 400：content[].thinking must be passed back）
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
            content: "读一下 mock 文件并总结".into(),
            files: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    let bodies = log.lock().expect("log");
    // 带工具结果的续轮请求：assistant 历史里必须有 thinking 块，且在 tool_use 之前
    let continuation = bodies
        .iter()
        .find(|body| body.contains("tool_result"))
        .unwrap_or_else(|| panic!("应有带 tool_result 的续轮请求: {bodies:?}"));
    let thinking_pos = continuation.find("\"type\":\"thinking\"");
    let tool_use_pos = continuation.find("\"type\":\"tool_use\"");
    assert!(
        thinking_pos.is_some(),
        "续轮请求缺少 thinking 块: {continuation}"
    );
    assert!(
        continuation.contains(mock::MOCK_REASONING),
        "thinking 块应包含思考原文: {continuation}"
    );
    assert!(
        thinking_pos < tool_use_pos,
        "thinking 必须在 tool_use 之前: {continuation}"
    );
    agent.shutdown();
}
