mod common;

use common::{new_session, setup};
use pig_core::mock;
use pig_protocol::{ApprovalDecision, Event, ExecMode, Op};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 收集一个完整 turn 的事件；`approval` 为 Some 时自动按给定决策回复审批。
async fn run_scenario_b(
    mode: ExecMode,
    approval: Option<ApprovalDecision>,
    cwd_name: &str,
) -> (Vec<Event>, PathBuf, pig_core::AgentHandle, String) {
    let (config_path, dir, data_dir) = setup(cwd_name);
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;

    agent
        .ops
        .send(Op::SetExecMode {
            session_id: session_id.clone(),
            mode,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{} 创建并修改文件，然后跑个命令", mock::SCENARIO_B_TRIGGER),
            files: vec![],
            mode,
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "等待回合结束超时: {collected:#?}");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event {
            if let Some(decision) = approval {
                agent
                    .ops
                    .send(Op::ApprovalReply {
                        request_id: request_id.clone(),
                        decision,
                    })
                    .await
                    .unwrap();
            }
        }
        let done = matches!(event, Event::TurnComplete { .. } | Event::TurnAborted { .. });
        collected.push(event);
        if done {
            break;
        }
    }
    (collected, dir, agent, session_id)
}

fn approvals(events: &[Event]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ApprovalRequested { tool, .. } => Some(tool.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_ends(events: &[Event]) -> Vec<(&str, &str, bool)> {
    // (item_id, output, is_error) — item_id 里含工具调用 id
    events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCallEnd {
                item_id,
                output,
                is_error,
                ..
            } => Some((item_id.as_str(), output.as_str(), *is_error)),
            _ => None,
        })
        .collect()
}

fn file_changes(events: &[Event]) -> Vec<(&str, &str, u32, u32)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::FileChanged {
                path,
                unified_diff,
                additions,
                deletions,
                ..
            } => Some((path.as_str(), unified_diff.as_str(), *additions, *deletions)),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_b_allow_all_then_revert() {
    let (events, dir, agent, session_id) =
        run_scenario_b(ExecMode::ConfirmBeforeEdit, Some(ApprovalDecision::Allow), "allow").await;

    assert_eq!(
        approvals(&events),
        ["write_file", "edit", "bash"],
        "三处审批按序出现"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::TurnComplete { .. })),
        "回合正常完成"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::SCENARIO_B_MARKER)
        )),
        "最终文本含场景 B 标记"
    );

    let ends = tool_ends(&events);
    assert!(
        ends.iter().any(|(id, out, err)| id.contains("call_b_bash")
            && out.contains(mock::SCENARIO_B_BASH_MARKER)
            && !err),
        "bash 输出含标记: {ends:?}"
    );

    let changes = file_changes(&events);
    assert_eq!(changes.len(), 2, "write+edit 各一次 FileChanged: {changes:?}");
    let (path, diff, adds, dels) = changes[0];
    assert_eq!(path, mock::SCENARIO_B_FILE);
    assert_eq!((adds, dels), (3, 0), "新建文件全是新增行: {diff}");
    assert!(diff.contains("+line2"));
    let (_, diff2, adds2, dels2) = changes[1];
    assert_eq!((adds2, dels2), (3, 0), "diff 始终是原始（文件不存在）→当前: {diff2}");
    assert!(diff2.contains("+LINE2") && !diff2.contains("-line2"), "diff 是原始→当前: {diff2}");

    let file = dir.join(mock::SCENARIO_B_FILE);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "hello\nLINE2\nline3\n",
        "文件内容被 write+edit 正确修改"
    );

    agent
        .ops
        .send(Op::RevertFile {
            session_id,
            path: mock::SCENARIO_B_FILE.into(),
        })
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            assert!(Instant::now() < deadline, "等待 FileReverted 超时");
            continue;
        };
        if matches!(&event, Event::FileReverted { path, .. } if path == mock::SCENARIO_B_FILE) {
            break;
        }
    }
    assert!(!file.exists(), "新建文件撤销后应被删除");
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_b_deny_all() {
    let (events, dir, agent, _) =
        run_scenario_b(ExecMode::ConfirmBeforeEdit, Some(ApprovalDecision::Reject), "deny").await;

    assert_eq!(approvals(&events), ["write_file", "edit", "bash"]);
    assert!(
        events.iter().any(|e| matches!(e, Event::TurnComplete { .. })),
        "拒绝后回合仍继续到结束"
    );
    let denied = tool_ends(&events)
        .iter()
        .filter(|(_, out, err)| out.contains("用户拒绝了该操作") && *err)
        .count();
    assert_eq!(denied, 3, "三个工具都被拒绝: {:?}", tool_ends(&events));
    assert!(file_changes(&events).is_empty(), "无文件变更");
    assert!(!dir.join(mock::SCENARIO_B_FILE).exists(), "文件未创建");
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_edit_only_bash_needs_approval() {
    let (events, dir, agent, _) =
        run_scenario_b(ExecMode::AutoEdit, Some(ApprovalDecision::Allow), "auto").await;

    assert_eq!(approvals(&events), ["bash"], "AutoEdit 只有 bash 需审批");
    assert!(dir.join(mock::SCENARIO_B_FILE).exists());
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_blocks_writes_without_approval() {
    let (events, dir, agent, _) = run_scenario_b(ExecMode::Plan, None, "plan").await;

    assert!(approvals(&events).is_empty(), "计划模式无审批卡");
    let blocked = tool_ends(&events)
        .iter()
        .filter(|(_, out, _)| out.contains("计划模式"))
        .count();
    assert_eq!(blocked, 3, "三个修改类工具都被计划模式拦截: {:?}", tool_ends(&events));
    assert!(!dir.join(mock::SCENARIO_B_FILE).exists());
    assert!(events.iter().any(|e| matches!(e, Event::TurnComplete { .. })));
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_access_no_approvals() {
    let (events, dir, agent, _) = run_scenario_b(ExecMode::FullAccess, None, "full").await;

    assert!(approvals(&events).is_empty(), "完全访问无审批卡");
    assert!(dir.join(mock::SCENARIO_B_FILE).exists());
    assert!(
        tool_ends(&events)
            .iter()
            .any(|(_, out, err)| out.contains(mock::SCENARIO_B_BASH_MARKER) && !err)
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_during_approval() {
    let (config_path, dir, data_dir) = setup("interrupt-approval");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{} 走修改链", mock::SCENARIO_B_TRIGGER),
            files: vec![],
            mode: ExecMode::ConfirmBeforeEdit,
        })
        .await
        .unwrap();

    let mut interrupted = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut aborted = false;
    while Instant::now() < deadline && !aborted {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        match event {
            Event::ApprovalRequested { .. } if !interrupted => {
                interrupted = true;
                agent
                    .ops
                    .send(Op::Interrupt { session_id: session_id.clone() })
                    .await
                    .unwrap();
            }
            Event::TurnAborted { .. } => aborted = true,
            Event::TurnComplete { .. } => panic!("审批中打断不应 TurnComplete"),
            _ => {}
        }
    }
    assert!(interrupted && aborted, "审批等待中 Interrupt 应解除阻塞并中止");
    agent.shutdown();
}
