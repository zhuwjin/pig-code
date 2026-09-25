mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

fn send(
    events: &async_channel::Receiver<Event>,
    agent: &pig_core::AgentHandle,
    sid: &str,
    text: &str,
) {
    send_with(events, agent, sid, text, ExecMode::AutoEdit)
}

fn send_with(
    _events: &async_channel::Receiver<Event>,
    agent: &pig_core::AgentHandle,
    sid: &str,
    text: &str,
    mode: ExecMode,
) {
    agent
        .ops
        .send_blocking(Op::SendMessage {
            session_id: sid.into(),
            content: text.into(),
            files: vec![],
            images: vec![],
            mode,
        })
        .unwrap();
}

async fn wait_turn(events: &async_channel::Receiver<Event>) -> Vec<Event> {
    recv_until(events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. } | Event::TurnAborted { .. })
    })
    .await
}

/// 模型摘要 compact：历史被摘要替换、rollout 有 compact 记录、续聊历史变短。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_summary_compact() {
    let (config_path, cwd, data_dir) = setup("m5-compact");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    // 两轮对话积累历史（7 条）
    send(&events, &agent, &sid, "读一下 mock 文件并总结");
    wait_turn(&events).await;
    send(&events, &agent, &sid, "再总结一下");
    wait_turn(&events).await;

    agent
        .ops
        .send(Op::Compact {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    let Some(Event::ContextCompacted {
        omitted,
        note,
        automatic,
        ..
    }) = collected.last()
    else {
        panic!("应有 ContextCompacted")
    };
    assert_eq!(*omitted, 2);
    assert!(!automatic, "手动 compact");
    assert!(note.contains("模型摘要"), "应走模型摘要: {note}");
    assert!(note.contains(mock::SUMMARY_MARKER), "应含摘要文本: {note}");

    // rollout 应有 compact 记录
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(rollout.contains("\"type\":\"compact\""), "{rollout}");
    assert!(rollout.contains(mock::SUMMARY_MARKER));

    // compact 后历史 = system + 摘要 + 裁边后3条 + 新 user = 6
    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    let count = collected.iter().find_map(|e| match e {
        Event::TextDone { full_text, .. } => full_text
            .strip_prefix("HISTORY_COUNT:")
            .and_then(|n| n.parse::<usize>().ok()),
        _ => None,
    });
    assert_eq!(count, Some(6), "{collected:#?}");
    agent.shutdown();
}

/// 自动 compact：usage 超阈值 → 下次采样前自动摘要。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_compact_on_high_usage() {
    let (config_path, cwd, data_dir) = setup("m5-auto");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    // 先一轮工具链攒历史（system+4 条）
    send(&events, &agent, &sid, "读一下 mock 文件并总结");
    wait_turn(&events).await;
    // mock 报告 usage=120000（阈值 = 128000-8192-13000 = 106808）
    send(&events, &agent, &sid, "ECHO_USAGE 120000");
    wait_turn(&events).await;

    // 下一轮：采样前应先自动 compact（此时历史 7 条 > 保留 4+1）
    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    let compact_pos = collected
        .iter()
        .position(|e| matches!(e, Event::ContextCompacted { automatic: true, note, .. } if note.contains(mock::SUMMARY_MARKER)));
    let complete_pos = collected
        .iter()
        .position(|e| matches!(e, Event::TurnComplete { .. }));
    assert!(compact_pos.is_some(), "应自动 compact: {collected:#?}");
    assert!(compact_pos < complete_pos, "compact 应在 TurnComplete 之前");
    agent.shutdown();
}

/// 摘要请求失败 → 回退朴素截断，会话不挂。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_failure_falls_back() {
    let (config_path, cwd, data_dir) = setup("m5-fallback");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    // 历史里带上 FAIL_COMPACT 标记 → mock 对摘要请求返回 500
    send(&events, &agent, &sid, "FAIL_COMPACT 读一下 mock 文件");
    wait_turn(&events).await;
    send(&events, &agent, &sid, "再来一轮");
    wait_turn(&events).await;

    agent
        .ops
        .send(Op::Compact {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    let Some(Event::ContextCompacted { omitted, note, .. }) = collected.last() else {
        panic!("应有 ContextCompacted")
    };
    assert!(*omitted > 0);
    assert!(note.contains("前文已压缩"), "{note}");
    assert!(!note.contains("模型摘要"), "摘要失败应回退截断: {note}");

    // 会话仍可继续
    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains("HISTORY_COUNT:")
        )),
        "compact 失败回退后会话应可继续: {collected:#?}"
    );
    agent.shutdown();
}

/// 场景 C：计划模式输出计划文本（无工具调用）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_c_plan_mode() {
    let (config_path, cwd, data_dir) = setup("m5-plan");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SetExecMode {
            session_id: sid.clone(),
            mode: ExecMode::Plan,
        })
        .await
        .unwrap();
    send_with(
        &events,
        &agent,
        &sid,
        "SCENARIO_C 给我一个改造计划",
        ExecMode::Plan,
    );
    let collected = wait_turn(&events).await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::PLAN_MARKER)
        )),
        "应输出计划文本: {collected:#?}"
    );
    assert!(
        !collected
            .iter()
            .any(|e| matches!(e, Event::ToolCallBegin { .. })),
        "计划模式不应有工具调用"
    );
    agent.shutdown();
}

/// 旧配置迁移后默认值生效。
#[test]
fn config_defaults() {
    let dir = std::env::temp_dir().join(format!("pig-core-cfg-default-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "[provider]\nbase_url = \"http://x\"\napi_key = \"k\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let config = pig_core::config::load(&path).unwrap();
    assert_eq!(config.providers.len(), 1);
    assert_eq!(config.providers[0].models[0].context_window, 128_000);
    assert_eq!(config.providers[0].models[0].max_output_tokens, 8_192);
    assert_eq!(config.default_provider, "default");
}
