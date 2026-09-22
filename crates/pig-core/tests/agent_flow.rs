mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_turn_with_tool_call() {
    let (config_path, cwd, data_dir) = setup("m2-full");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "读一下 mock 文件并总结".into(),
            files: vec![mock::MOCK_FILE_NAME.into()],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();

    let events = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    assert!(
        events.iter().any(|e| matches!(e, Event::ReasoningDelta { delta, .. } if !delta.is_empty())),
        "应有 reasoning delta"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::TextDelta { .. })),
        "应有 text delta"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ToolCallBegin { tool, input_summary, .. }
                if tool == "Read" && input_summary.contains(mock::MOCK_FILE_NAME)
        )),
        "应有 Read 工具调用: {events:#?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. } if output.contains("已知文件")
        )),
        "工具输出应含文件内容"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::MOCK_REPLY_MARKER)
        )),
        "最终文本应含 mock 标记"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::ContextUsage { used: 142, .. })),
        "应有上下文用量事件"
    );
    assert!(
        events.iter().all(|e| match e {
            Event::TurnStarted { session_id: sid, .. }
            | Event::ReasoningDelta { session_id: sid, .. }
            | Event::TextDelta { session_id: sid, .. }
            | Event::TextDone { session_id: sid, .. }
            | Event::ToolCallBegin { session_id: sid, .. }
            | Event::ToolCallEnd { session_id: sid, .. }
            | Event::ContextUsage { session_id: sid, .. }
            | Event::TurnComplete { session_id: sid, .. } => sid == &session_id,
            _ => true,
        }),
        "事件 session_id 应一致"
    );

    // rollout 文件应已落盘
    let rollout = data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let content = std::fs::read_to_string(&rollout).expect("rollout 存在");
    assert!(content.contains("\"type\":\"meta\""), "首行 meta");
    assert!(content.contains("\"type\":\"user\""));
    assert!(content.contains("\"type\":\"text\""));
    assert!(content.contains("\"type\":\"tool_call\""));

    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_during_stream() {
    let (config_path, cwd, data_dir) = setup("m2-interrupt");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "说点什么".into(),
            files: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();

    recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::ReasoningDelta { .. } | Event::TextDelta { .. })
    })
    .await;
    agent
        .ops
        .send(Op::Interrupt { session_id })
        .await
        .unwrap();

    let events = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnAborted { .. } | Event::TurnComplete { .. })
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(e, Event::TurnAborted { .. })),
        "应收到 TurnAborted: {events:#?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_config_is_empty_not_error() {
    let dir = std::env::temp_dir().join(format!("pig-core-m2-nocfg-{}", std::process::id()));
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(dir.join("nonexistent.toml")),
        dir.clone(),
        dir.join("data"),
    );
    let events = agent.events.clone();

    // 配置缺失不应有 Error；GetConfig 返回空快照
    agent.ops.send(Op::GetConfig).await.unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::ConfigSnapshot { .. })
    })
    .await;
    assert!(
        !collected.iter().any(|e| matches!(e, Event::Error { .. })),
        "配置缺失不应报错: {collected:#?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::ConfigSnapshot { config, .. } if config.providers.is_empty()
        )),
        "应返回空配置: {collected:#?}"
    );

    // NewSession 正常，模型名报"未配置模型"；发送时才报引导性错误
    let sid = new_session(&agent, dir).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid,
            content: "hi".into(),
            files: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::Error { .. })
    })
    .await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::Error { message, .. } if message.contains("模型设置")
        )),
        "发送时应报引导性错误: {collected:#?}"
    );
    agent.shutdown();
}

/// 发 SCENARIO_Q 并等待 QuestionRequested，返回 request_id（同时断言问题内容）。
async fn wait_question(
    agent: &pig_core::AgentHandle,
    events: &async_channel::Receiver<Event>,
    session_id: &str,
) -> String {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: format!("{} 帮我决定实现方案", mock::SCENARIO_Q_TRIGGER),
            files: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    let collected = recv_until(events, Duration::from_secs(20), |e| {
        matches!(e, Event::QuestionRequested { .. })
    })
    .await;
    collected
        .iter()
        .find_map(|e| match e {
            Event::QuestionRequested {
                request_id,
                questions,
                ..
            } => {
                assert_eq!(questions.len(), 2);
                assert_eq!(questions[0].question, "选择实现方案");
                assert_eq!(questions[0].options.len(), 2);
                assert_eq!(questions[1].question, "需要跑测试吗");
                assert_eq!(questions[1].options.len(), 2);
                Some(request_id.clone())
            }
            _ => None,
        })
        .expect("应收到 QuestionRequested")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_user_question_answer() {
    let (config_path, cwd, data_dir) = setup("question-answer");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    let request_id = wait_question(&agent, &events, &session_id).await;
    agent
        .ops
        .send(Op::QuestionReply {
            request_id,
            answers: Some(vec![vec!["方案 A".to_string()], vec!["要".to_string()]]),
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. }
                if output.contains("用户已回答")
                    && output.contains("方案 A")
                    && output.contains("需要跑测试吗：要")
        )),
        "工具输出应含两题答案: {collected:#?}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::MOCK_Q_MARKER)
        )),
        "最终文本应含 marker: {collected:#?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_user_question_skip() {
    let (config_path, cwd, data_dir) = setup("question-skip");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    let request_id = wait_question(&agent, &events, &session_id).await;
    agent
        .ops
        .send(Op::QuestionReply {
            request_id,
            answers: None,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { output, is_error: false, .. } if output.contains("自行决定")
        )),
        "跳过应提示自行决定: {collected:#?}"
    );
    agent.shutdown();
}
