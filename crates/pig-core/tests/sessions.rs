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
        replay.iter().any(|e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "Read")),
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
            Event::ToolCallBegin { session_id, tool, .. } if tool == "Read" => {
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

/// 重启持久化：待办（todos 表）、文件改动（file_changes 表）、
/// 原始快照（file_originals 表 → 跨重启 diff 基线 + revert）。
/// 两个会话分别跑两种场景：mock 按请求体子串分发，同会话串场景会互相干扰。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_restores_todos_changes_and_revert() {
    let (config_path, cwd, data_dir) = setup("m5-persist");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();

    // 会话 A：文件改动（Write → Edit → Bash）
    let session_a = new_session(&agent, cwd.clone()).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_a.clone(),
            content: format!("{} 改个文件", mock::SCENARIO_B_TRIGGER),
            files: vec![],
            mode: ExecMode::FullAccess,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    // 会话 B：待办写入（TodoList）
    let session_b = new_session(&agent, cwd.clone()).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_b.clone(),
            content: format!("{} 建两条待办", mock::TODO_SCENARIO_TRIGGER),
            files: vec![],
            mode: ExecMode::FullAccess,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    agent.shutdown();

    // 模拟重启：同一 data_dir 起新 manager
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events2 = agent2.events.clone();

    // 重开会话 A：文件改动从 DB 恢复（按路径一条当前态）
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_a.clone(),
        })
        .await
        .unwrap();
    let replay_a = recv_until(&events2, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let file_changes: Vec<_> = replay_a
        .iter()
        .filter(|e| matches!(e, Event::FileChanged { path, .. } if path.ends_with("hello.txt")))
        .collect();
    assert_eq!(
        file_changes.len(),
        1,
        "同路径只回放一条当前态: {file_changes:?}"
    );

    // 跨重启 revert：原始快照已恢复 → revert 成功且新建文件被删除
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
        ev.iter().any(|e| matches!(e, Event::FileReverted { path, .. } if path == mock::SCENARIO_B_FILE)),
        "跨重启 revert 应成功（基线来自 file_originals 表）: {ev:#?}"
    );
    assert!(
        !cwd.join(mock::SCENARIO_B_FILE).exists(),
        "revert 后新建文件应被删除"
    );

    // 重开会话 B：待办从 DB 恢复（JSONL 无待办记录的新会话也能还原）
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
    let items =
        todo_snapshot.unwrap_or_else(|| panic!("回放应含 DB 恢复的待办快照: {replay_b:#?}"));
    assert_eq!(items.len(), 2, "{items:?}");
    assert!(
        items.iter().any(|i| i.content.contains(mock::TODO_SCENARIO_ITEM)
            && i.status == pig_protocol::TodoStatus::InProgress),
        "进行中的待办应还原: {items:?}"
    );
    agent2.shutdown();
}

/// 会话级模型/模式/思考等级持久化：
/// ① 新会话继承工作区最近活跃会话的值；② 重启后重开恢复。
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

    // 会话 A 设置模式 + 模型 + 思考等级（写穿 sessions 表）
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
            provider_id: "default".into(),
            model_id: "mock-model".into(),
            reasoning_level: Some("high".into()),
        })
        .await
        .unwrap();
    // 等写穿完成（op 按序处理，发一个 ListSessions 当栅栏）
    agent.ops.send(Op::ListSessions).await.unwrap();
    recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::SessionList { .. })
    })
    .await;

    // ① 同工作区新建会话：模型/模式未指定 → 继承 A 的种子；
    // 思考等级模拟 UI hero 种子下达后显式传入（core 按原样采用）
    agent
        .ops
        .send(Op::NewSession {
            cwd: cwd.clone(),
            provider_id: None,
            model_id: None,
            reasoning_level: Some("high".into()),
            exec_mode: None,
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
        panic!("应有 SessionConfigured")
    };
    assert_eq!(
        (provider_id.as_deref(), model_id.as_deref(), reasoning_level.as_deref(), *exec_mode),
        (Some("default"), Some("mock-model"), Some("high"), ExecMode::FullAccess),
        "新会话应继承工作区最近活跃会话的模型/模式/思考等级"
    );
    agent.shutdown();

    // ② 模拟重启后重开 A：值应从 sessions 表恢复
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_a.clone(),
        })
        .await
        .unwrap();
    // A 没有回合记录，回放不会发 TurnComplete；以重开路径必发的 TaskListChanged 为终点
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
    }) = replay.iter().find(|e| matches!(e, Event::SessionConfigured { .. }))
    else {
        panic!("回放应有 SessionConfigured: {replay:#?}")
    };
    assert_eq!(
        (provider_id.as_deref(), model_id.as_deref(), reasoning_level.as_deref(), *exec_mode),
        (Some("default"), Some("mock-model"), Some("high"), ExecMode::FullAccess),
        "重启后重开应恢复模型/模式/思考等级"
    );
    agent2.shutdown();
}

/// 改完设置不对话、在两个会话间来回切换：值必须跟随各自会话，不丢不串。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switch_preserves_mode_without_turn() {
    let (config_path, cwd, data_dir) = setup("m5-switch");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_a = new_session(&agent, cwd.clone()).await;
    let session_b = new_session(&agent, cwd.clone()).await;

    // A 上改模式/模型/思考等级（不对话）
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
            provider_id: "default".into(),
            model_id: "mock-model".into(),
            reasoning_level: Some("high".into()),
        })
        .await
        .unwrap();
    // 栅栏：op 按序处理，等一次 ListSessions 回来即完成写穿
    agent.ops.send(Op::ListSessions).await.unwrap();
    recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::SessionList { .. })
    })
    .await;

    // 切到 B：应是 B 自己的默认值，不是 A 的
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
    }) = ev_b.iter().find(|e| matches!(e, Event::SessionConfigured { .. }))
    else {
        panic!("B 应有 SessionConfigured: {ev_b:#?}")
    };
    assert_eq!(
        (provider_id.as_deref(), reasoning_level.as_deref(), *exec_mode),
        (None, None, ExecMode::ConfirmBeforeEdit),
        "B 不应带上 A 的值"
    );

    // B 上只改思考等级、不选模型（无模型覆盖也要持久化）
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

    // 切回 A：应恢复刚才改的值（虽然没对话）
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
    }) = ev_a.iter().find(|e| matches!(e, Event::SessionConfigured { .. }))
    else {
        panic!("A 应有 SessionConfigured: {ev_a:#?}")
    };
    assert_eq!(
        (provider_id.as_deref(), model_id.as_deref(), reasoning_level.as_deref(), *exec_mode),
        (Some("default"), Some("mock-model"), Some("high"), ExecMode::FullAccess),
        "切回应恢复 A 改过的值"
    );

    // 再切回 B：只改思考等级的值也应在
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
    }) = ev_b2.iter().find(|e| matches!(e, Event::SessionConfigured { .. }))
    else {
        panic!("B 应有 SessionConfigured: {ev_b2:#?}")
    };
    assert_eq!(
        (provider_id.as_deref(), reasoning_level.as_deref()),
        (None, Some("max")),
        "无模型覆盖的思考等级切换后应保留"
    );
    agent.shutdown();
}
