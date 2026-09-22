mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

/// 一轮写改（场景 B：Write + Edit 同一文件）结束后，应产出本轮改动事件：
/// 文件按「本轮首次写前 → 当前」净额统计（新建 = 全量新增），并随 rollout 回放恢复。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_file_changes_emitted_and_replayed() {
    let (config_path, cwd, data_dir) = setup("turn-changes");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path.clone()), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();

    let session_id = new_session(&agent, cwd.clone()).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{} 改个文件", mock::SCENARIO_B_TRIGGER),
            files: vec![],
            mode: ExecMode::FullAccess,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let files = collected.iter().find_map(|e| match e {
        Event::TurnFileChanges { files, .. } => Some(files.clone()),
        _ => None,
    });
    let files = files.expect("回合结束应产出 TurnFileChanges");
    assert_eq!(files.len(), 1, "场景 B 只动一个文件: {files:?}");
    let change = &files[0];
    assert_eq!(change.path, mock::SCENARIO_B_FILE);
    // 本轮首次写前不存在 → 最终 3 行全为新增（Write 3 行 + Edit 改行不增行数）
    assert_eq!((change.additions, change.deletions), (3, 0), "净额口径: {}", change.unified_diff);
    agent.shutdown();

    // 模拟重启重开会话：每轮改动面板从 rollout 回放恢复
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let collected = recv_until(&events2, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { duration_ms: 0, .. })
    })
    .await;
    let replayed = collected.iter().find_map(|e| match e {
        Event::TurnFileChanges { files, .. } => Some(files.clone()),
        _ => None,
    });
    let replayed = replayed.expect("回放应恢复 TurnFileChanges");
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].path, mock::SCENARIO_B_FILE);
    assert_eq!((replayed[0].additions, replayed[0].deletions), (3, 0));
    agent2.shutdown();
}
