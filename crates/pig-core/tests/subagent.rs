//! Agent 子代理工具（A2a 前台同步）集成测试：happy path / 工具收窄 / 审批门 /
//! max_turns / 严格模型失败 / 结果截断落盘 / Plan 模式拒绝。

mod common;

use common::{new_session, setup};
use pig_core::mock;
use pig_protocol::{ApprovalDecision, Event, ExecMode, Op};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 发一条消息并收集到回合结束；`approve` 给定时自动按该决策回复审批请求。
async fn run_turn(
    agent: &pig_core::AgentHandle,
    sid: &str,
    text: &str,
    mode: ExecMode,
    approve: Option<ApprovalDecision>,
) -> Vec<Event> {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.into(),
            content: text.into(),
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
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event {
            if let Some(decision) = approve {
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
    collected
}

/// Agent 工具卡片的 (output, is_error)（item_id 含子代理调用的固定 id）
fn agent_end(events: &[Event]) -> Option<(&str, bool)> {
    events.iter().find_map(|e| match e {
        Event::ToolCallEnd {
            item_id,
            output,
            is_error,
            ..
        } if item_id.contains("call_agent_1") => Some((output.as_str(), *is_error)),
        _ => None,
    })
}

/// 从 Agent 结果文本里解析 agent_id 行
fn parse_agent_id(output: &str) -> &str {
    output
        .lines()
        .find_map(|line| line.strip_prefix("agent_id: "))
        .expect("结果应有 agent_id 行")
}

/// 子代理上下文 JSONL 路径：{data}/sessions/{sid}.agents/{agent_id}.jsonl
fn agent_log_path(data_dir: &Path, sid: &str, agent_id: &str) -> PathBuf {
    data_dir
        .join("sessions")
        .join(format!("{sid}.agents"))
        .join(format!("{agent_id}.jsonl"))
}

/// happy path：explore 委派（子调 Grep → 子文本收尾）。
/// Agent 结果带回子结论 + agent_id + resume_hint + status: completed；
/// 父 rollout 只有 Agent 一条工具记录；子代理上下文落盘；进度事件走 SubagentProgress。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_happy_path() {
    let (config_path, cwd, data_dir) = setup("subagent-happy");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;

    // Agent 工具卡片：成功、带回子结论与模板字段
    let (output, is_error) = agent_end(&collected).expect("应有 Agent ToolCallEnd");
    assert!(!is_error, "happy path 不应失败: {output}");
    assert!(
        output.contains(mock::SUBAGENT_CHILD_DONE),
        "应含子结论: {output}"
    );
    assert!(
        output.contains("status: completed"),
        "应 completed: {output}"
    );
    assert!(output.contains("turns: 2"), "两步（Grep+收尾）: {output}");
    assert!(
        output.contains("resume_hint: 用 Agent(resume=\""),
        "应带 resume_hint: {output}"
    );
    let agent_id = parse_agent_id(output).to_string();

    // 父 rollout 只有一条 Agent 工具记录（子工具调用不进父 rollout）
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl"))).unwrap();
    let tool_records: Vec<&str> = rollout
        .lines()
        .filter(|line| line.contains("\"type\":\"tool_call\""))
        .collect();
    assert_eq!(
        tool_records.len(),
        1,
        "父 rollout 应只有 Agent 一条: {rollout}"
    );
    assert!(tool_records[0].contains("\"tool\":\"Agent\""));
    assert!(
        !rollout.contains("\"tool\":\"Grep\""),
        "子工具不应进父 rollout: {rollout}"
    );

    // 子代理上下文 JSONL：首行 meta，≥3 条 msg（system/user/assistant(Grep)/tool/assistant = 5）
    let log_path = agent_log_path(&data_dir, &sid, &agent_id);
    let log = std::fs::read_to_string(&log_path)
        .unwrap_or_else(|e| panic!("子代理上下文应存在 {}: {e}", log_path.display()));
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines.len() >= 4, "meta + 至少 3 条 msg: {log}");
    assert!(lines[0].contains("\"type\":\"meta\""), "首行 meta: {log}");
    assert!(lines[0].contains(&agent_id));
    assert!(lines[0].contains("\"profile\":\"explore\""));
    let msg_count = lines
        .iter()
        .filter(|line| line.contains("\"type\":\"msg\""))
        .count();
    assert!(msg_count >= 3, "至少 3 条 msg: {log}");

    // 事件流：有 SubagentProgress；子工具（Grep）不发顶层 ToolCallBegin
    let progress: Vec<&str> = collected
        .iter()
        .filter_map(|e| match e {
            Event::SubagentProgress { item_id, note, .. } if item_id.contains("call_agent_1") => {
                Some(note.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(
        progress.iter().any(|note| note.contains("第 1 步")),
        "应有步进度: {progress:?}"
    );
    assert!(
        progress.iter().any(|note| note.contains('·')),
        "工具执行也应有进度行: {progress:?}"
    );
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "Agent")),
        "父时间线应有 Agent 卡片"
    );
    assert!(
        collected
            .iter()
            .all(|e| !matches!(e, Event::ToolCallBegin { tool, .. } if tool != "Agent")),
        "子工具不应发 ToolCallBegin: {collected:#?}"
    );
    agent.shutdown();
}

/// 工具收窄：explore 子代理调 Write（不在其工具集）→ 收到「未知工具」错误后可收尾，
/// 文件不落地，最终 completed。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_tool_narrowing_blocks_write() {
    let (config_path, cwd, data_dir) = setup("subagent-narrow");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd.clone()).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} WRITE", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;

    let (output, is_error) = agent_end(&collected).expect("应有 Agent ToolCallEnd");
    assert!(!is_error, "未知工具后子代理应可收尾: {output}");
    assert!(
        output.contains(mock::SUBAGENT_CHILD_DONE),
        "应含子结论: {output}"
    );
    assert!(output.contains("status: completed"), "{output}");
    assert!(
        !cwd.join("child_write.txt").exists(),
        "收窄掉的 Write 不应执行"
    );
    agent.shutdown();
}

/// 审批门：ConfirmBeforeEdit 下 general-purpose 子代理的 Bash（非白名单命令）
/// 弹审批；批准后命令真执行（文件落地）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_child_tools_pass_approval_gate() {
    let (config_path, cwd, data_dir) = setup("subagent-approval");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd.clone()).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} BASH", mock::SUBAGENT_TRIGGER),
        ExecMode::ConfirmBeforeEdit,
        Some(ApprovalDecision::Allow),
    )
    .await;

    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ApprovalRequested { tool, .. } if tool == "Bash")),
        "子代理 Bash 应弹审批: {collected:#?}"
    );
    assert!(
        cwd.join("child_bash.txt").exists(),
        "批准后命令应真执行（文件落地）"
    );
    let (output, is_error) = agent_end(&collected).expect("应有 Agent ToolCallEnd");
    assert!(!is_error, "{output}");
    assert!(output.contains("status: completed"), "{output}");
    agent.shutdown();
}

/// max_turns：项目级档案 maxTurns: 2 + 子代理每步都给 tool_call → 轮次用尽收尾。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_max_turns_exhausted() {
    let (config_path, cwd, data_dir) = setup("subagent-maxturns");
    std::fs::create_dir_all(cwd.join(".pigcode/agents")).unwrap();
    std::fs::write(
        cwd.join(".pigcode/agents/loop.md"),
        "---\nname: loop\ndescription: 循环测试子代理\nmaxTurns: 2\n---\n循环档案正文。",
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} LOOP loop", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;

    let (output, is_error) = agent_end(&collected).expect("应有 Agent ToolCallEnd");
    assert!(is_error, "轮次用尽且无结论文本应 is_error: {output}");
    assert!(output.contains("已达最大轮次 2"), "{output}");
    agent.shutdown();
}

/// 严格模型解析失败：档案 model 指向不存在的供应商 → 结果 is_error 且列可用模型。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_model_resolve_failure_is_strict() {
    let (config_path, cwd, data_dir) = setup("subagent-badmodel");
    std::fs::create_dir_all(cwd.join(".pigcode/agents")).unwrap();
    std::fs::write(
        cwd.join(".pigcode/agents/badmodel.md"),
        "---\nname: badmodel\ndescription: 坏模型测试子代理\nmodel: p9/m9\n---\n坏模型档案正文。",
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP badmodel", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;

    let (output, is_error) = agent_end(&collected).expect("应有 Agent ToolCallEnd");
    assert!(is_error, "模型解析失败应 is_error: {output}");
    assert!(output.contains("p9"), "应点名坏供应商: {output}");
    assert!(
        output.contains("default/mock-model"),
        "应列可用 providerId/modelId: {output}"
    );
    agent.shutdown();
}

/// 结果截断：子代理最终文本 >32K 字符 → Agent 结果截断并指向全文落盘文件。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_result_truncated_and_spilled() {
    let (config_path, cwd, data_dir) = setup("subagent-truncate");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd.clone()).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} LONG", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;

    let (output, is_error) = agent_end(&collected).expect("应有 Agent ToolCallEnd");
    assert!(!is_error, "截断不是错误: {output}");
    assert!(output.contains("status: completed"), "{output}");
    assert!(
        output.contains("[结果过长已截断，全文: "),
        "应有截断指引: {output}"
    );
    // 全文落盘：{cwd}/.pigcode/tool-results/agent-{agent_id}.md，内容原样完整
    let agent_id = parse_agent_id(output);
    let spill = cwd
        .join(".pigcode")
        .join("tool-results")
        .join(format!("agent-{agent_id}.md"));
    let full = std::fs::read_to_string(&spill)
        .unwrap_or_else(|e| panic!("全文应落盘 {}: {e}", spill.display()));
    let expected = format!("子代理长结果开头。{}", "密".repeat(33_000));
    assert_eq!(full, expected, "落盘应为未截断全文");
    agent.shutdown();
}

/// Plan 模式拒绝：计划模式下委派子代理 → 「计划模式下不可委派」。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_plan_mode_rejected() {
    let (config_path, cwd, data_dir) = setup("subagent-plan");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SetExecMode {
            session_id: sid.clone(),
            mode: ExecMode::Plan,
        })
        .await
        .unwrap();
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP", mock::SUBAGENT_TRIGGER),
        ExecMode::Plan,
        None,
    )
    .await;

    let (output, is_error) = agent_end(&collected).expect("应有 Agent ToolCallEnd");
    assert!(is_error, "Plan 委派应失败: {output}");
    assert!(output.contains("计划模式下不可委派"), "{output}");
    agent.shutdown();
}

/// 按调用 id 取 Agent 工具卡片的 (output, is_error)
fn agent_end_with<'a>(events: &'a [Event], call_id: &str) -> Option<(&'a str, bool)> {
    events.iter().find_map(|e| match e {
        Event::ToolCallEnd {
            item_id,
            output,
            is_error,
            ..
        } if item_id.contains(call_id) => Some((output.as_str(), *is_error)),
        _ => None,
    })
}

/// 子代理上下文 JSONL 的 msg 行数
fn agent_log_msg_count(data_dir: &Path, sid: &str, agent_id: &str) -> usize {
    let log = std::fs::read_to_string(agent_log_path(data_dir, sid, agent_id)).unwrap();
    log.lines()
        .filter(|line| line.contains("\"type\":\"msg\""))
        .count()
}

/// 后台全链路：run_in_background=true → 父立即收 running+task_id；父 turn 结束后
/// 等到合成 <task-notification> user 消息与其后 TurnComplete；任务面板含「子代理」条目。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_background_full_link() {
    let (config_path, cwd, data_dir) = setup("subagent-bg-full");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} BG", mock::SUBAGENT_TRIGGER),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();

    let mut running_output: Option<String> = None;
    let mut notification: Option<String> = None;
    let mut completes = 0usize;
    let mut saw_agent_task = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "等待后台全链路超时");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::ToolCallEnd {
                item_id,
                output,
                is_error,
                ..
            } if item_id.contains("call_agent_bg") => {
                assert!(!is_error, "后台委派不应失败: {output}");
                running_output = Some(output.clone());
            }
            Event::UserMessage { text, .. } if text.contains("<task-notification>") => {
                notification = Some(text.clone());
            }
            Event::TaskListChanged { tasks, .. } => {
                saw_agent_task |= tasks.iter().any(|t| t.command.contains("子代理"));
            }
            Event::TurnComplete { .. } => completes += 1,
            _ => {}
        }
        if completes >= 2 && notification.is_some() {
            break;
        }
    }

    // 父立即收 running 回执（task_id 可查）
    let running = running_output.expect("应有后台 Agent ToolCallEnd");
    assert!(running.contains("status: running"), "{running}");
    assert!(running.contains("task_id: b"), "{running}");
    let agent_id = parse_agent_id(&running).to_string();

    // 完成通知：合成 user 消息送达（含 agent_id 与子结论），其后唤醒回合收尾
    let notification = notification.expect("应有 <task-notification> 合成消息");
    assert!(notification.contains(&agent_id), "{notification}");
    assert!(
        notification.contains(mock::SUBAGENT_CHILD_DONE),
        "通知应含子结论: {notification}"
    );
    assert!(notification.contains("已完成"), "{notification}");
    assert!(saw_agent_task, "任务面板应含子代理条目");
    agent.shutdown();
}

/// resume 续跑：先前台跑一次，再 resume（新 prompt）→ jsonl 增长、结果含新结论、
/// resume_hint 的 agent_id 不变。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_continues_context() {
    let (config_path, cwd, data_dir) = setup("subagent-resume");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;
    let (output1, is_error1) = agent_end(&collected).expect("首次 Agent ToolCallEnd");
    assert!(!is_error1, "{output1}");
    let agent_id = parse_agent_id(output1).to_string();
    let msgs_before = agent_log_msg_count(&data_dir, &sid, &agent_id);

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;
    let (output2, is_error2) =
        agent_end_with(&collected, "call_agent_2").expect("resume Agent ToolCallEnd");
    assert!(!is_error2, "resume 应成功: {output2}");
    assert!(
        output2.contains(mock::SUBAGENT_RESUMED_DONE),
        "应含续跑新结论: {output2}"
    );
    assert!(output2.contains("status: completed"), "{output2}");
    assert_eq!(
        parse_agent_id(output2),
        agent_id,
        "resume_hint 的 agent_id 应不变"
    );
    let msgs_after = agent_log_msg_count(&data_dir, &sid, &agent_id);
    assert!(
        msgs_after >= msgs_before + 2,
        "resume 应追加 ≥2 条消息（新 prompt + 新结论）: {msgs_before} → {msgs_after}"
    );
    agent.shutdown();
}

/// resume 未知 id：报错列可用 id。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_unknown_id_lists_available() {
    let (config_path, cwd, data_dir) = setup("subagent-resume-unknown");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    // 先跑一次拿到真实 agent_id
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;
    let (output1, _) = agent_end(&collected).expect("首次 Agent ToolCallEnd");
    let real_id = parse_agent_id(output1).to_string();

    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME_UNKNOWN", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;
    let (output2, is_error2) =
        agent_end_with(&collected, "call_agent_2").expect("resume Agent ToolCallEnd");
    assert!(is_error2, "未知 id 应失败: {output2}");
    assert!(output2.contains("a0-999"), "应点名未知 id: {output2}");
    assert!(output2.contains("不存在"), "{output2}");
    assert!(output2.contains(&real_id), "应列可用 agent_id: {output2}");
    agent.shutdown();
}

/// resume 运行中冲突：后台子代理还在跑就 resume → 报「仍在运行」。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_running_conflict() {
    let (config_path, cwd, data_dir) = setup("subagent-resume-running");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    // 第一条：后台 LOOP 子代理（一直跑）
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME_RUNNING", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;
    let (output1, is_error1) =
        agent_end_with(&collected, "call_agent_bg").expect("后台 Agent ToolCallEnd");
    assert!(!is_error1, "{output1}");
    assert!(output1.contains("status: running"), "{output1}");

    // 第二条：立刻 resume 同 id → 冲突
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME_RUNNING", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;
    let (output2, is_error2) =
        agent_end_with(&collected, "call_agent_2").expect("resume Agent ToolCallEnd");
    assert!(is_error2, "运行中 resume 应失败: {output2}");
    assert!(output2.contains("仍在运行"), "{output2}");
    agent.shutdown();
}

/// TaskStop 后台子代理：注册表置 Killed，且没有 wake 合成消息送达。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_background_taskstop_no_wake() {
    let (config_path, cwd, data_dir) = setup("subagent-bg-stop");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    let mut collected = run_turn(
        &agent,
        &sid,
        &format!("{} BGSTOP", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;
    // 父模型确实调了 TaskStop 且成功
    let (stop_output, stop_error) =
        agent_end_with(&collected, "call_taskstop").expect("应有 TaskStop ToolCallEnd");
    assert!(!stop_error, "TaskStop 应成功: {stop_output}");

    // 再等一个窗口（驱动被取消后还会补一次 TaskListChanged）：
    // 注册表应见 Killed；事件流不应出现 <task-notification> 合成消息
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), agent.events.recv()).await
        else {
            continue;
        };
        collected.push(event);
    }
    let killed = collected.iter().any(|e| {
        matches!(e, Event::TaskListChanged { tasks, .. } if tasks
            .iter()
            .any(|t| t.command.contains("子代理") && t.status == pig_protocol::TaskStatus::Killed))
    });
    assert!(killed, "注册表应有 Killed 子代理任务: {collected:#?}");
    assert!(
        !collected.iter().any(|e| matches!(
            e,
            Event::UserMessage { text, .. } if text.contains("<task-notification>")
        )),
        "TaskStop 杀掉的子代理不应唤醒父会话: {collected:#?}"
    );
    agent.shutdown();
}

/// 后台审批：ConfirmBeforeEdit 下后台子代理的写操作仍走 pending 审批门——
/// ApprovalRequested 出现 → 批准 → 命令执行 → 完成通知送达。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_background_approval_gate() {
    let (config_path, cwd, data_dir) = setup("subagent-bg-approval");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} BASHBG", mock::SUBAGENT_TRIGGER),
            files: vec![],
            images: vec![],
            mode: ExecMode::ConfirmBeforeEdit,
        })
        .await
        .unwrap();

    let mut saw_approval = false;
    let mut notification: Option<String> = None;
    let mut completes = 0usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "等待后台审批链路超时");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::ApprovalRequested {
                request_id, tool, ..
            } => {
                assert_eq!(tool, "Bash", "后台子代理的 Bash 应弹审批");
                saw_approval = true;
                agent
                    .ops
                    .send(Op::ApprovalReply {
                        request_id: request_id.clone(),
                        decision: ApprovalDecision::Allow,
                    })
                    .await
                    .unwrap();
            }
            Event::UserMessage { text, .. } if text.contains("<task-notification>") => {
                notification = Some(text.clone());
            }
            Event::TurnComplete { .. } => completes += 1,
            _ => {}
        }
        if completes >= 2 && notification.is_some() {
            break;
        }
    }

    assert!(saw_approval, "后台子代理应触发审批门");
    assert!(
        cwd.join("child_bash.txt").exists(),
        "批准后后台命令应真执行（文件落地）"
    );
    let notification = notification.expect("应有完成通知");
    assert!(
        notification.contains(mock::SUBAGENT_CHILD_DONE),
        "{notification}"
    );
    agent.shutdown();
}
