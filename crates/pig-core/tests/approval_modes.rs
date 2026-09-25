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
            images: vec![],
            mode,
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "等待回合结束超时: {collected:#?}"
        );
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
        let done = matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. }
        );
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
    let (events, dir, agent, session_id) = run_scenario_b(
        ExecMode::ConfirmBeforeEdit,
        Some(ApprovalDecision::Allow),
        "allow",
    )
    .await;

    assert_eq!(
        approvals(&events),
        ["Write", "Edit", "Bash"],
        "三处审批按序出现"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TurnComplete { .. })),
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
        "Bash 输出含标记: {ends:?}"
    );

    let changes = file_changes(&events);
    assert_eq!(
        changes.len(),
        2,
        "Write+Edit 各一次 FileChanged: {changes:?}"
    );
    let (path, diff, adds, dels) = changes[0];
    assert_eq!(path, mock::SCENARIO_B_FILE);
    assert_eq!((adds, dels), (3, 0), "新建文件全是新增行: {diff}");
    assert!(diff.contains("+line2"));
    let (_, diff2, adds2, dels2) = changes[1];
    assert_eq!(
        (adds2, dels2),
        (3, 0),
        "diff 始终是原始（文件不存在）→当前: {diff2}"
    );
    assert!(
        diff2.contains("+LINE2") && !diff2.contains("-line2"),
        "diff 是原始→当前: {diff2}"
    );

    let file = dir.join(mock::SCENARIO_B_FILE);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "hello\nLINE2\nline3\n",
        "文件内容被 Write+Edit 正确修改"
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
    let (events, dir, agent, _) = run_scenario_b(
        ExecMode::ConfirmBeforeEdit,
        Some(ApprovalDecision::Reject),
        "deny",
    )
    .await;

    assert_eq!(approvals(&events), ["Write", "Edit", "Bash"]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TurnComplete { .. })),
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

    assert_eq!(approvals(&events), ["Bash"], "AutoEdit 只有 Bash 需审批");
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
    assert_eq!(
        blocked,
        3,
        "三个修改类工具都被计划模式拦截: {:?}",
        tool_ends(&events)
    );
    assert!(!dir.join(mock::SCENARIO_B_FILE).exists());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TurnComplete { .. }))
    );
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
            images: vec![],
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
                    .send(Op::Interrupt {
                        session_id: session_id.clone(),
                    })
                    .await
                    .unwrap();
            }
            Event::TurnAborted { .. } => aborted = true,
            Event::TurnComplete { .. } => panic!("审批中打断不应 TurnComplete"),
            _ => {}
        }
    }
    assert!(
        interrupted && aborted,
        "审批等待中 Interrupt 应解除阻塞并中止"
    );
    agent.shutdown();
}

// ---------- 危险命令强制审批（黑名单命中 = 弹窗，所有模式） ----------

/// 危险命令场景驱动：每个审批弹窗都按 decision 回复。
async fn run_danger(
    mode: ExecMode,
    decision: ApprovalDecision,
    cwd_name: &str,
) -> (Vec<Event>, PathBuf, pig_core::AgentHandle) {
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
            session_id,
            content: format!("{} 执行危险命令", mock::SCENARIO_DANGER_TRIGGER),
            files: vec![],
            images: vec![],
            mode,
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "等待回合结束超时: {collected:#?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event {
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision,
                })
                .await
                .unwrap();
        }
        let done = matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. }
        );
        collected.push(event);
        if done {
            break;
        }
    }
    (collected, dir, agent)
}

fn approval_details(events: &[Event]) -> Vec<(&str, &str)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ApprovalRequested { tool, detail, .. } => Some((tool.as_str(), detail.as_str())),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn danger_full_access_allow_executes() {
    // FullAccess 本来不问 Bash，但危险命令必须弹；mock 的 mkfs 命中黑名单且执行无害
    //（无此命令 exit 127 / 无参数只打印用法）——exit 非 0 也足以证明走了执行路径
    let (events, _dir, agent) = run_danger(
        ExecMode::FullAccess,
        ApprovalDecision::Allow,
        "danger-allow",
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(details.len(), 2, "两条危险命令都应弹窗: {details:?}");
    assert!(
        details
            .iter()
            .all(|(tool, d)| *tool == "Bash" && d.contains("高风险命令") && d.contains("mkfs")),
        "detail 应带高风险前缀与命令全文: {details:?}"
    );
    let ends = tool_ends(&events);
    assert!(
        ends.iter()
            .any(|(_, out, err)| !err && out.contains("[exit code:")),
        "Allow 后应真实执行: {ends:?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn danger_full_access_reject() {
    let (events, dir, agent) = run_danger(
        ExecMode::FullAccess,
        ApprovalDecision::Reject,
        "danger-reject",
    )
    .await;
    let ends = tool_ends(&events);
    assert!(
        ends.iter()
            .any(|(_, out, err)| *err && out.contains("拒绝了该高风险命令")),
        "Reject 给模型的文案: {ends:?}"
    );
    assert!(
        !ends.iter().any(|(_, out, _)| out.contains("[exit code:")),
        "拒绝路径不应执行: {ends:?}"
    );
    // 无文件副作用（场景只有 Bash）
    assert!(file_changes(&events).is_empty());
    let _ = dir;
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn danger_confirm_before_edit_detail_prefixed() {
    let (events, _dir, agent) = run_danger(
        ExecMode::ConfirmBeforeEdit,
        ApprovalDecision::Allow,
        "danger-cbe",
    )
    .await;
    let details = approval_details(&events);
    assert!(!details.is_empty(), "ConfirmBeforeEdit 下 Bash 本就审批");
    assert!(
        details.iter().all(|(_, d)| d.contains("⚠️ 高风险命令")),
        "危险命令弹窗应带警示前缀: {details:?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn danger_always_allow_not_remembered() {
    // 危险命令上点 AlwaysAllow 不记入 always_allowed：同会话第二条危险命令仍弹窗
    let (events, _dir, agent) = run_danger(
        ExecMode::FullAccess,
        ApprovalDecision::AlwaysAllow,
        "danger-always",
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        2,
        "第二条危险命令仍应弹窗（不记忆）: {details:?}"
    );
    agent.shutdown();
}

// ---------- Yolo（无管制全自动） ----------

/// "Yolo" 字符串 serde 往返（sessions 表按变体名存取，旧数据天然兼容）
#[test]
fn exec_mode_yolo_serde_roundtrip() {
    let json = serde_json::to_string(&ExecMode::Yolo).unwrap();
    assert_eq!(json, "\"Yolo\"");
    let back: ExecMode = serde_json::from_str("\"Yolo\"").unwrap();
    assert_eq!(back, ExecMode::Yolo);
    // 未知变体名回退默认（store.rs mode_from_row 同口径）
    let fallback: ExecMode = serde_json::from_str("\"NotAMode\"").unwrap_or_default();
    assert_eq!(fallback, ExecMode::ConfirmBeforeEdit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_danger_no_dialog_executes() {
    // Yolo：危险命令也不弹窗，直接执行（mkfs 命中黑名单但无害，见 mock 注释）
    let (events, _dir, agent) =
        run_danger(ExecMode::Yolo, ApprovalDecision::Allow, "yolo-danger").await;
    assert!(
        approval_details(&events).is_empty(),
        "Yolo 下不应有任何审批弹窗"
    );
    let ends = tool_ends(&events);
    assert_eq!(
        ends.iter()
            .filter(|(_, out, err)| !err && out.contains("[exit code:"))
            .count(),
        2,
        "两条危险命令都直接执行: {ends:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::DANGER_MARKER)
        )),
        "回合正常收尾"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_normal_flow_no_dialogs() {
    let (events, dir, agent, _session_id) = run_scenario_b(ExecMode::Yolo, None, "yolo-b").await;
    assert!(
        approvals(&events).is_empty(),
        "Yolo 下 Write/Edit/Bash 都不弹窗"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TurnComplete { .. })),
        "回合正常完成"
    );
    // Write+Edit 生效
    let file = dir.join(mock::SCENARIO_B_FILE);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "hello\nLINE2\nline3\n"
    );
    // Bash 执行成功
    let ends = tool_ends(&events);
    assert!(
        ends.iter().any(|(id, out, err)| id.contains("call_b_bash")
            && out.contains(mock::SCENARIO_B_BASH_MARKER)
            && !err),
        "Bash 输出含标记: {ends:?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_sensitive_file_still_blocked() {
    // 敏感文件防护在工具 execute 层，与模式无关（Yolo 下也无条件生效）：
    // tool::execute 不感知模式，直接验证 .env 读取仍被拒
    let dir = std::env::temp_dir().join(format!("pig-core-yolo-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();

    let mut tracker = pig_core::tool::ChangeTracker::default();
    let state = pig_core::task::SessionToolState::for_test();
    let call = pig_core::provider::ToolCall {
        id: "t1".into(),
        name: "Read".into(),
        arguments: serde_json::json!({"path": ".env"}).to_string(),
    };
    let (out, is_error, _, _, _) = pig_core::tool::execute(
        &call,
        pig_core::tool::ToolContext {
            cwd: &dir,
            tracker: &mut tracker,
            state: &state,
        },
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("敏感文件"), "Yolo 下 .env 仍不可读: {out}");
    assert!(!out.contains("SECRET=1"), "内容不泄露: {out}");
}

// ---------- AlwaysAllow subject 粒度 / AutoEdit 只读直通 / TaskStop 豁免 ----------

/// 通用触发场景驱动：每个审批弹窗都按 decision 回复（None = 不回复，出现弹窗会卡到超时）。
/// `permissions` 非空时在 new_session 前写入 .pigcode/permissions.toml（规则随会话加载）。
async fn run_trigger(
    mode: ExecMode,
    decision: Option<ApprovalDecision>,
    trigger: &str,
    cwd_name: &str,
    permissions: Option<&str>,
) -> (Vec<Event>, PathBuf, pig_core::AgentHandle) {
    let (config_path, dir, data_dir) = setup(cwd_name);
    if let Some(permissions) = permissions {
        std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
        std::fs::write(dir.join(".pigcode/permissions.toml"), permissions).unwrap();
    }
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
            session_id,
            content: format!("{trigger} 开始"),
            files: vec![],
            images: vec![],
            mode,
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "等待回合结束超时: {collected:#?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event {
            if let Some(decision) = decision {
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
        let done = matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. }
        );
        collected.push(event);
        if done {
            break;
        }
    }
    (collected, dir, agent)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn always_allow_is_per_subject() {
    // 场景：Write a → Write a（同 subject，免弹）→ Write b（异 subject，弹）→
    // Bash echo → Bash echo（同首词，免弹）→ Bash ls（异首词，弹）
    let (events, dir, agent) = run_trigger(
        ExecMode::ConfirmBeforeEdit,
        Some(ApprovalDecision::AlwaysAllow),
        mock::SCENARIO_SUBJECT_TRIGGER,
        "subject",
        None,
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        4,
        "a 首/b 首/echo 首/ls 首各弹一次，同 subject 第二次免弹: {details:?}"
    );
    assert!(details[0].1.contains(mock::SUBJECT_FILE_A), "{details:?}");
    assert!(details[1].1.contains(mock::SUBJECT_FILE_B), "{details:?}");
    assert!(details[2].1.contains("echo SUBJ_3"), "{details:?}");
    assert_eq!(details[3].1.trim(), "ls", "{details:?}");
    // 回合完成且工具都真实执行了
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::SUBJECT_MARKER)
        )),
        "回合收尾"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(mock::SUBJECT_FILE_A)).unwrap(),
        "v1\n",
        "同 subject 的第二次 Write 免弹直接执行"
    );
    assert!(dir.join(mock::SUBJECT_FILE_B).exists());
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_edit_readonly_bash_passthrough() {
    // AutoEdit + 白名单只读命令（ls）：不弹窗直接执行（decision=None 若弹窗会卡超时）
    let (events, _dir, agent) = run_trigger(
        ExecMode::AutoEdit,
        None,
        mock::SCENARIO_READONLY_TRIGGER,
        "readonly",
        None,
    )
    .await;
    assert!(approval_details(&events).is_empty(), "只读命令不应弹窗");
    let ends = tool_ends(&events);
    assert!(
        ends.iter()
            .any(|(_, out, err)| !err && out.contains("[exit code: 0]")),
        "ls 真实执行: {ends:?}"
    );
    agent.shutdown();
}

#[test]
fn approval_subject_extracts() {
    let call = |name: &str, args: serde_json::Value| pig_core::provider::ToolCall {
        id: "t1".into(),
        name: name.into(),
        arguments: args.to_string(),
    };
    assert_eq!(
        pig_core::tool::approval_subject(&call(
            "Bash",
            serde_json::json!({"command": "cargo build --release"})
        )),
        "cargo"
    );
    assert_eq!(
        pig_core::tool::approval_subject(&call(
            "Bash",
            serde_json::json!({"command": "  ls  -la"})
        )),
        "ls",
        "前导空白后取首词"
    );
    assert_eq!(
        pig_core::tool::approval_subject(&call("Bash", serde_json::json!({"command": ""}))),
        ""
    );
    assert_eq!(
        pig_core::tool::approval_subject(&call(
            "Write",
            serde_json::json!({"path": "src/a.rs", "content": "x"})
        )),
        "src/a.rs"
    );
    assert_eq!(
        pig_core::tool::approval_subject(&call(
            "Edit",
            serde_json::json!({"path": "b.md", "old_string": "a", "new_string": "b"})
        )),
        "b.md"
    );
    // 其余工具 → 空串（工具级记忆）
    assert_eq!(
        pig_core::tool::approval_subject(&call("TaskStop", serde_json::json!({"task_id": "b1"}))),
        ""
    );
}

#[test]
fn task_stop_read_only_exempt_from_approval() {
    let tools = pig_core::tool::all();
    let stop = tools
        .iter()
        .find(|t| t.name() == "TaskStop")
        .expect("TaskStop 存在");
    assert!(stop.read_only(), "TaskStop 应标记只读");
    assert!(
        !pig_core::tool::requires_approval(stop.as_ref(), ExecMode::ConfirmBeforeEdit),
        "ConfirmBeforeEdit 下也免批"
    );
    assert!(
        !pig_core::tool::requires_approval(stop.as_ref(), ExecMode::Plan),
        "Plan 下允许（Plan 只硬拒非只读）"
    );
}

// ---------- 项目级权限规则（优先级链路） / ExitPlanMode ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_deny_hard_rejects_even_in_full_access() {
    // FullAccess + deny Bash(mkfs*)：连危险弹窗都不发，项目规则直接硬拒
    let (events, _dir, agent) = run_trigger(
        ExecMode::FullAccess,
        Some(ApprovalDecision::Allow),
        mock::SCENARIO_DANGER_TRIGGER,
        "perm-deny",
        Some("deny = [\"Bash(mkfs*)\"]\n"),
    )
    .await;
    assert!(approval_details(&events).is_empty(), "deny 优先于危险弹窗");
    let ends = tool_ends(&events);
    assert_eq!(ends.len(), 2, "两条 mkfs 都被拒: {ends:?}");
    assert!(
        ends.iter().all(|(_, out, err)| *err
            && out.contains("项目规则禁止执行")
            && out.contains("Bash(mkfs*)")),
        "deny 文案含规则原文: {ends:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::DANGER_MARKER)
        )),
        "回合正常收尾"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_allow_skips_approval() {
    // ConfirmBeforeEdit 下 ls 本来要弹窗；allow 命中 → 免弹直接执行
    let (events, _dir, agent) = run_trigger(
        ExecMode::ConfirmBeforeEdit,
        None,
        mock::SCENARIO_READONLY_TRIGGER,
        "perm-allow",
        Some("allow = [\"Bash(ls)\"]\n"),
    )
    .await;
    assert!(approval_details(&events).is_empty(), "allow 免审批");
    assert!(
        tool_ends(&events)
            .iter()
            .any(|(_, out, err)| !err && out.contains("[exit code: 0]")),
        "ls 真实执行"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_allow_does_not_exempt_danger() {
    // allow 命中危险命令仍弹窗（危险判定在 allow 之前）
    let (events, _dir, agent) = run_trigger(
        ExecMode::ConfirmBeforeEdit,
        Some(ApprovalDecision::Allow),
        mock::SCENARIO_DANGER_TRIGGER,
        "perm-danger-allow",
        Some("allow = [\"Bash(mkfs*)\"]\n"),
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(details.len(), 2, "危险命令照样弹: {details:?}");
    assert!(
        details.iter().all(|(_, d)| d.contains("高风险命令")),
        "{details:?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_plan_mode_allow_switches_and_executes() {
    // Plan + ExitPlanMode + Allow：弹窗 → 切到变更前确认 → 后续 Write 在新模式下审批执行
    let (events, dir, agent) = run_trigger(
        ExecMode::Plan,
        Some(ApprovalDecision::Allow),
        mock::SCENARIO_PLAN_EXIT_TRIGGER,
        "plan-exit-allow",
        None,
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        2,
        "ExitPlanMode + Write 各弹一次: {details:?}"
    );
    assert_eq!(details[0].0, "ExitPlanMode");
    assert!(details[0].1.contains("结束计划模式"), "{details:?}");
    assert!(
        details[0].1.contains("第一步"),
        "plan 摘要进 detail: {details:?}"
    );
    assert_eq!(details[1].0, "Write", "切模式后 Write 走正常审批");
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ExecModeChanged { mode, .. } if *mode == ExecMode::ConfirmBeforeEdit
        )),
        "UI 收到模式更新事件"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(mock::PLAN_EXIT_FILE)).unwrap(),
        "executed\n",
        "Write 在新模式下真实执行"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_plan_mode_reject_stays_plan() {
    let (events, dir, agent) = run_trigger(
        ExecMode::Plan,
        Some(ApprovalDecision::Reject),
        mock::SCENARIO_PLAN_EXIT_TRIGGER,
        "plan-exit-reject",
        None,
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(details.len(), 1, "只有 ExitPlanMode 一次弹窗: {details:?}");
    assert!(
        tool_ends(&events)
            .iter()
            .any(|(_, out, err)| *err && out.contains("拒绝退出计划模式")),
        "拒绝文案"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::ExecModeChanged { .. })),
        "模式不变"
    );
    assert!(
        !dir.join(mock::PLAN_EXIT_FILE).exists(),
        "Reject 后 Write 未发生"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_plan_mode_outside_plan_errors() {
    let (events, _dir, agent) = run_trigger(
        ExecMode::AutoEdit,
        None,
        mock::SCENARIO_PLAN_EXIT_TRIGGER,
        "plan-exit-nonplan",
        None,
    )
    .await;
    assert!(approval_details(&events).is_empty(), "非 Plan 不弹窗");
    assert!(
        tool_ends(&events)
            .iter()
            .any(|(_, out, err)| *err && out.contains("仅在计划模式下可用")),
        "非 Plan 报错"
    );
    agent.shutdown();
}

// ---------- EnterPlanMode（与 ExitPlanMode 配对） ----------

fn mode_changes(events: &[Event]) -> Vec<ExecMode> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ExecModeChanged { mode, .. } => Some(*mode),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enter_plan_switches_without_dialog_and_exit_restores_original() {
    // AutoEdit → EnterPlanMode（不弹窗，直接切 Plan）→ Write 被 Plan 硬拒 →
    // ExitPlanMode（弹窗 Allow）→ 恢复 AutoEdit → Write 免审批执行
    let (events, dir, agent) = run_trigger(
        ExecMode::AutoEdit,
        Some(ApprovalDecision::Allow),
        mock::SCENARIO_PLAN_ENTER_TRIGGER,
        "plan-enter",
        None,
    )
    .await;

    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        1,
        "只有 ExitPlanMode 弹窗（EnterPlanMode 不弹）: {details:?}"
    );
    assert_eq!(details[0].0, "ExitPlanMode");

    assert_eq!(
        mode_changes(&events),
        [ExecMode::Plan, ExecMode::AutoEdit],
        "进入 Plan → 恢复原模式（AutoEdit，不是默认的变更前确认）"
    );

    let ends = tool_ends(&events);
    assert!(
        ends.iter().any(|(id, out, err)| id.contains("call_pn_1")
            && !err
            && out.contains("已切换到计划模式")),
        "EnterPlanMode 幂等成功: {ends:?}"
    );
    assert!(
        ends.iter()
            .any(|(id, out, err)| id.contains("call_pn_2") && *err && out.contains("计划模式")),
        "Plan 下 Write 被硬拒: {ends:?}"
    );
    assert!(
        ends.iter()
            .any(|(id, _, err)| id.contains("call_pn_4") && !err),
        "恢复后 Write 执行: {ends:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(mock::PLAN_ENTER_FILE)).unwrap(),
        "executed\n"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enter_plan_mode_idempotent_when_already_plan() {
    // Plan 下再调 EnterPlanMode：幂等提示，无模式事件；后续 ExitPlanMode 仍弹窗
    let (events, _dir, agent) = run_trigger(
        ExecMode::Plan,
        Some(ApprovalDecision::Reject),
        mock::SCENARIO_PLAN_ENTER_TRIGGER,
        "plan-enter-idem",
        None,
    )
    .await;
    let ends = tool_ends(&events);
    assert!(
        ends.iter()
            .any(|(id, out, err)| id.contains("call_pn_1") && !err && out.contains("已在计划模式")),
        "幂等提示: {ends:?}"
    );
    assert!(mode_changes(&events).is_empty(), "幂等路径不发模式事件");
    let details = approval_details(&events);
    assert_eq!(details.len(), 1, "只有 ExitPlanMode 一次弹窗: {details:?}");
    agent.shutdown();
}
