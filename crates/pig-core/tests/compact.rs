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
    // CompactStarted 必须先于 ContextCompacted 到达（UI「正在压缩」态配对）
    let started_pos = collected.iter().position(|e| {
        matches!(
            e,
            Event::CompactStarted {
                automatic: false,
                ..
            }
        )
    });
    assert!(
        started_pos.is_some() && started_pos < collected.len().checked_sub(1),
        "CompactStarted 应在 ContextCompacted 之前: {collected:#?}"
    );
    let Some(Event::ContextCompacted {
        omitted,
        note,
        automatic,
        ..
    }) = collected.last()
    else {
        panic!("应有 ContextCompacted")
    };
    // 尾部切到 user 边界后保留 2 条（窗口 [T,A,u,A] 裁掉前两条）
    assert_eq!(*omitted, 4);
    assert!(!automatic, "手动 compact");
    assert!(note.contains("模型摘要"), "应走模型摘要: {note}");
    assert!(note.contains(mock::SUMMARY_MARKER), "应含摘要文本: {note}");

    // rollout 应有 compact 记录
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(rollout.contains("\"type\":\"compact\""), "{rollout}");
    assert!(rollout.contains(mock::SUMMARY_MARKER));
    assert!(rollout.contains("\"used_after\""), "{rollout}");

    // compact 后历史 = system + 摘要 + user 边界后 2 条 + 新 user = 5
    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    let count = collected.iter().find_map(|e| match e {
        Event::TextDone { full_text, .. } => full_text
            .strip_prefix("HISTORY_COUNT:")
            .and_then(|n| n.parse::<usize>().ok()),
        _ => None,
    });
    assert_eq!(count, Some(5), "{collected:#?}");
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

/// 压缩后水位重置：ContextUsage 立即降为估算值；下一回合不再因旧高水位
/// 触发重复自动压缩；重启回放补发压缩条与新水位。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_usage_resets_and_replays() {
    let (config_path, cwd, data_dir) = setup("m5-usage-reset");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd.clone()).await;

    send(&events, &agent, &sid, "读一下 mock 文件并总结");
    wait_turn(&events).await;
    // mock 报告 usage=120000（阈值 106808 之上），水位抬高
    send(&events, &agent, &sid, "ECHO_USAGE 120000");
    let high = wait_turn(&events).await;
    assert!(
        high.iter()
            .any(|e| matches!(e, Event::ContextUsage { used: 120000, .. })),
        "高水位应已生效: {high:#?}"
    );

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
    // 压缩后应立即补发估算水位（远小于 120000）
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ContextUsage { used, .. } if *used < 120000)),
        "压缩后应补发估算水位: {collected:#?}"
    );

    // 下一回合：水位已重置，不得再触发自动压缩（旧逻辑会拿 120000 再压一次，
    // 历史 ≤5 条时早退并多发一条「历史很短」ContextCompacted）
    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    assert!(
        !collected
            .iter()
            .any(|e| matches!(e, Event::ContextCompacted { .. })),
        "水位重置后不得重复自动压缩: {collected:#?}"
    );
    // 本回合的真实用量 = 回放后的最终水位（压缩后的 step_usage 覆盖 used_after，
    // 更近的真实值优先）
    let Some(last_used) = collected.iter().find_map(|e| match e {
        Event::ContextUsage { used, .. } => Some(*used),
        _ => None,
    }) else {
        panic!("回合应有 ContextUsage: {collected:#?}")
    };
    agent.shutdown();

    // 模拟重启：回放应补发「上下文已压缩」分隔条事件与压缩后的水位
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let replay = recv_until(&events2, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextUsage { .. })
    })
    .await;
    assert!(
        replay
            .iter()
            .any(|e| matches!(e, Event::ContextCompacted { omitted: 4, .. })),
        "回放应含压缩条事件: {replay:#?}"
    );
    assert!(
        replay
            .iter()
            .any(|e| matches!(e, Event::ContextUsage { used, .. } if *used == last_used)),
        "回放后水位应为压缩后回合的真实值 {last_used}（而非压缩前的 120000）: {replay:#?}"
    );
    agent2.shutdown();
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
