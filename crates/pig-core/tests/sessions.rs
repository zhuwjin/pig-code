mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

/// resume：rollout 落盘 → 新 agent 进程内 OpenSession → 历史重建，模型能收到之前的历史。
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
    agent.shutdown();

    // 模拟重启：同一 data_dir 起新 manager
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events2 = agent2.events.clone();

    agent2.ops.send(Op::ListSessions).await.unwrap();
    let list = recv_until(&events2, Duration::from_secs(5), |e| {
        matches!(e, Event::SessionList { .. })
    })
    .await;
    let Some(Event::SessionList { sessions }) = list.last() else {
        panic!("应有 SessionList");
    };
    assert!(
        sessions.iter().any(|s| s.id == session_id && s.title.contains("读一下")),
        "索引里应有会话且 title 取自首条消息: {sessions:?}"
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
        replay.iter().any(|e| matches!(e, Event::UserMessage { text, .. } if text.contains("读一下"))),
        "重放应含用户消息"
    );
    assert!(
        replay.iter().any(|e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "read_file")),
        "重放应含工具调用"
    );

    // 回放以 duration_ms=0 的 TurnComplete 收尾；先排空回放事件，避免干扰下面的 recv_until
    while tokio::time::timeout(Duration::from_millis(200), events2.recv())
        .await
        .is_ok()
    {}

    // 继续对话：模型应收到重建后的历史（system+user+assistant+tool_call+tool_result+新user = 6 条）
    agent2
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "ECHO_HISTORY 报一下消息数".into(),
            files: vec![],
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
    assert_eq!(count, Some(6), "resume 后历史应完整重建: {events:#?}");
    agent2.shutdown();
}

/// sessions 表的 pin/archive。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pin_and_archive_update_index() {
    let (config_path, cwd, data_dir) = setup("m4-meta");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::UpdateSessionMeta {
            session_id: session_id.clone(),
            pinned: Some(true),
            archived: None,
            title: Some("重要任务".into()),
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::SessionList { sessions, .. } if sessions.iter().any(|s| s.pinned))
    })
    .await;
    let Some(Event::SessionList { sessions }) = collected.last() else {
        panic!()
    };
    let meta = sessions.iter().find(|s| s.id == session_id).unwrap();
    assert!(meta.pinned && !meta.archived && meta.title == "重要任务");

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
    let collected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::SessionList { sessions, .. } if sessions.iter().any(|s| s.archived))
    })
    .await;
    let Some(Event::SessionList { sessions }) = collected.last() else {
        panic!()
    };
    let meta = sessions.iter().find(|s| s.id == session_id).unwrap();
    assert!(!meta.pinned && meta.archived);

    // 落盘验证：重开 store 读出归档标记
    let store = pig_core::store::Store::open(&data_dir).unwrap();
    let meta = store.get_session(&session_id).unwrap();
    assert!(meta.archived && !meta.pinned, "{meta:?}");
    agent.shutdown();
}

/// 两个会话并行发消息，事件按 session_id 正确分流。
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
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_b.clone(),
            content: "读一下 mock 文件并总结".into(),
            files: vec![],
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
            assert!(std::time::Instant::now() < deadline, "并行回合超时");
            continue;
        };
        match &event {
            Event::TurnComplete { session_id, .. } if *session_id == session_a => complete_a = true,
            Event::TurnComplete { session_id, .. } if *session_id == session_b => complete_b = true,
            _ => {}
        }
        all.push(event);
    }
    // A 的 text 事件都属于 A；B 的工具调用都属于 B
    for event in &all {
        match event {
            Event::TextDelta { session_id, delta, .. } if delta.contains("HISTORY_COUNT") => {
                assert_eq!(session_id, &session_a, "A 的文本不应串到 B")
            }
            Event::ToolCallBegin { session_id, tool, .. } if tool == "read_file" => {
                assert_eq!(session_id, &session_b, "B 的工具调用不应串到 A")
            }
            _ => {}
        }
    }
    agent.shutdown();
}

/// AGENTS.md 双层注入。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_md_injected() {
    let (config_path, cwd, data_dir) = setup("m4-agents");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(data_dir.join("AGENTS.md"), "GLOBAL_RULE_X1: 全局规则").unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "WORKSPACE_RULE_Y2: 工作区规则").unwrap();

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: "ECHO_SYSTEM 回显系统提示词".into(),
            files: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    let events = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let text = events.iter().find_map(|e| match e {
        Event::TextDone { full_text, .. } => Some(full_text.as_str()),
        _ => None,
    });
    let text = text.expect("应有文本回复");
    assert!(text.contains("GLOBAL_RULE_X1"), "应含全局 AGENTS.md: {text}");
    assert!(text.contains("WORKSPACE_RULE_Y2"), "应含工作区 AGENTS.md: {text}");
    agent.shutdown();
}

/// Compact：历史变短且带压缩标记。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_shortens_history() {
    let (config_path, cwd, data_dir) = setup("m4-compact");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd.clone()).await;

    // 三轮对话积累历史（第一轮带工具链，后续轮为纯文本）
    for text in ["读一下 mock 文件并总结", "再总结一下", "继续"] {
        agent
            .ops
            .send(Op::SendMessage {
                session_id: session_id.clone(),
                content: text.into(),
                files: vec![],
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
        })
        .await
        .unwrap();
    let compacted = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    let Some(Event::ContextCompacted { omitted, note, .. }) = compacted.last() else {
        panic!("应有 ContextCompacted")
    };
    assert!(*omitted > 0);
    assert!(note.contains("前文已压缩"), "{note}");

    // compact 后历史变短
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "ECHO_HISTORY".into(),
            files: vec![],
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
    // M5 起 compact 保留最近 4 条：system + 摘要 + 4 条 + 新 user = 7
    assert_eq!(count, Some(7), "compact 后历史应变短: {collected:#?}");
    agent.shutdown();
}

/// @文件搜索。
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
        })
        .await
        .unwrap();
    let events = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::FileSearchResults { .. })
    })
    .await;
    let Some(Event::FileSearchResults { results, .. }) = events.last() else {
        panic!("应有 FileSearchResults")
    };
    assert!(
        results.iter().any(|r| r == "src/hello.rs"),
        "应找到真实文件: {results:?}"
    );
    assert!(
        !results.iter().any(|r| r.contains("config.toml")),
        "cwd 外的 data 目录不应出现: {results:?}"
    );
    agent.shutdown();
}
