//! Agent subagent tool (A2a foreground sync) integration tests: happy path /
//! tool narrowing / approval gate / max_turns / strict model failure / result
//! truncation spill / Plan mode rejection.

mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{ApprovalDecision, Event, ExecMode, Op};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Send one message and collect until the turn ends; when `approve` is given, approval requests are answered automatically with that decision.
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
            "timed out waiting for the turn to end: {collected:#?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event
            && let Some(decision) = approve
        {
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision,
                    feedback: None,
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
    collected
}

/// (output, is_error) of the Agent tool card (item_id contains the subagent call's fixed id)
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

/// Parse the agent_id line from the Agent result text
fn parse_agent_id(output: &str) -> &str {
    output
        .lines()
        .find_map(|line| line.strip_prefix("agent_id: "))
        .expect("result should have an agent_id line")
}

/// Subagent context JSONL path: {data}/sessions/{sid}.agents/{agent_id}.jsonl
fn agent_log_path(data_dir: &Path, sid: &str, agent_id: &str) -> PathBuf {
    data_dir
        .join("sessions")
        .join(format!("{sid}.agents"))
        .join(format!("{agent_id}.jsonl"))
}

/// Happy path: explore delegation (child calls Grep → child text wrap-up).
/// The Agent result brings back the child conclusion + agent_id + resume_hint +
/// status: completed; the parent rollout has only the single Agent tool
/// record; the subagent context is persisted; progress events go through
/// SubagentProgress.
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

    // Agent tool card: success, brings back the child conclusion and template fields
    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(!is_error, "happy path should not fail: {output}");
    assert!(
        output.contains(mock::SUBAGENT_CHILD_DONE),
        "should contain the child conclusion: {output}"
    );
    assert!(
        output.contains("status: completed"),
        "should be completed: {output}"
    );
    assert!(
        output.contains("turns: 2"),
        "two steps (Grep + wrap-up): {output}"
    );
    assert!(
        output.contains("resume_hint: continue this subagent with Agent(resume=\""),
        "should carry resume_hint: {output}"
    );
    let agent_id = parse_agent_id(output).to_string();

    // The parent rollout has a single Agent tool record (child tool calls do not enter the parent rollout)
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl"))).unwrap();
    let tool_records: Vec<&str> = rollout
        .lines()
        .filter(|line| line.contains("\"type\":\"tool_call\""))
        .collect();
    assert_eq!(
        tool_records.len(),
        1,
        "parent rollout should contain only the Agent record: {rollout}"
    );
    assert!(tool_records[0].contains("\"tool\":\"Agent\""));
    assert!(
        !rollout.contains("\"tool\":\"Grep\""),
        "child tools should not enter the parent rollout: {rollout}"
    );

    // Subagent context JSONL: first line meta, ≥3 msg entries (system/user/assistant(Grep)/tool/assistant = 5)
    let log_path = agent_log_path(&data_dir, &sid, &agent_id);
    let log = std::fs::read_to_string(&log_path).unwrap_or_else(|e| {
        panic!(
            "subagent context should exist at {}: {e}",
            log_path.display()
        )
    });
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines.len() >= 4, "meta + at least 3 msgs: {log}");
    assert!(
        lines[0].contains("\"type\":\"meta\""),
        "first line is meta: {log}"
    );
    assert!(lines[0].contains(&agent_id));
    assert!(lines[0].contains("\"profile\":\"explore\""));
    let msg_count = lines
        .iter()
        .filter(|line| line.contains("\"type\":\"msg\""))
        .count();
    assert!(msg_count >= 3, "at least 3 msgs: {log}");

    // Full result persisted (written in foreground mode too): {agents_dir}/{agent_id}.result.md
    let result_path = log_path.with_file_name(format!("{agent_id}.result.md"));
    let result_text = std::fs::read_to_string(&result_path)
        .unwrap_or_else(|e| panic!("result file should exist at {}: {e}", result_path.display()));
    assert!(
        result_text.contains(mock::SUBAGENT_CHILD_DONE),
        "result file should hold the full child conclusion: {result_text}"
    );

    // Event stream: has SubagentProgress; the child tool (Grep) emits no top-level ToolCallBegin
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
        progress.iter().any(|note| note.contains("Step 1")),
        "should have step progress: {progress:?}"
    );
    assert!(
        progress.iter().any(|note| note.contains('·')),
        "tool execution should also produce progress lines: {progress:?}"
    );
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "Agent")),
        "parent timeline should have an Agent card"
    );
    assert!(
        collected
            .iter()
            .all(|e| !matches!(e, Event::ToolCallBegin { tool, .. } if tool != "Agent")),
        "child tools should not emit ToolCallBegin: {collected:#?}"
    );
    agent.shutdown();
}

/// Tool narrowing: the explore subagent calls Write (not in its tool set) →
/// can still wrap up after receiving the "unknown tool" error, the file does
/// not land, ultimately completed.
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

    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(
        !is_error,
        "subagent should still wrap up after an unknown tool: {output}"
    );
    assert!(
        output.contains(mock::SUBAGENT_CHILD_DONE),
        "should contain the child conclusion: {output}"
    );
    assert!(output.contains("status: completed"), "{output}");
    assert!(
        !cwd.join("child_write.txt").exists(),
        "the narrowed-out Write should not execute"
    );
    agent.shutdown();
}

/// Approval gate: under ConfirmBeforeEdit the general-purpose subagent's Bash
/// (a non-allowlist command) pops an approval; after approval the command
/// actually executes (file created).
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
        "subagent Bash should prompt for approval: {collected:#?}"
    );
    assert!(
        cwd.join("child_bash.txt").exists(),
        "after approval the command actually executes (file created)"
    );
    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(!is_error, "{output}");
    assert!(output.contains("status: completed"), "{output}");
    agent.shutdown();
}

/// max_turns: project-level profile maxTurns: 2 + the subagent returning a tool_call every step → wraps up when turns run out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_max_turns_exhausted() {
    let (config_path, cwd, data_dir) = setup("subagent-maxturns");
    std::fs::create_dir_all(cwd.join(".pigcode/agents")).unwrap();
    std::fs::write(
        cwd.join(".pigcode/agents/loop.md"),
        "---\nname: loop\ndescription: loop test subagent\nmaxTurns: 2\n---\nLoop profile body.",
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

    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(
        is_error,
        "exhausted turns with no conclusion should be is_error: {output}"
    );
    assert!(
        output.contains("Reached the maximum turn count (2)"),
        "{output}"
    );
    agent.shutdown();
}

/// Strict model resolution failure: the profile's model points to a nonexistent provider → the result is is_error and lists available models.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_model_resolve_failure_is_strict() {
    let (config_path, cwd, data_dir) = setup("subagent-badmodel");
    std::fs::create_dir_all(cwd.join(".pigcode/agents")).unwrap();
    std::fs::write(
        cwd.join(".pigcode/agents/badmodel.md"),
        "---\nname: badmodel\ndescription: bad-model test subagent\nmodel: p9/m9\n---\nBad-model profile body.",
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

    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(
        is_error,
        "model resolve failure should be is_error: {output}"
    );
    assert!(
        output.contains("p9"),
        "should name the bad provider: {output}"
    );
    assert!(
        output.contains("mock/mock-model"),
        "should list available providerId/modelId: {output}"
    );
    agent.shutdown();
}

/// Result truncation: subagent final text >32K chars → the Agent result is truncated and points to the persisted full-text file.
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

    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(!is_error, "truncation is not an error: {output}");
    assert!(output.contains("status: completed"), "{output}");
    assert!(
        output.contains("[Result too long; truncated. Full text: "),
        "should contain the truncation pointer: {output}"
    );
    // Full text persisted: {data}/sessions/{sid}.agents/{agent_id}.result.md
    // (kimi's output.log equivalent), content complete and unmodified; the
    // truncation pointer references it
    let agent_id = parse_agent_id(output);
    assert!(
        output.contains(".result.md"),
        "truncation pointer should reference result.md: {output}"
    );
    let spill =
        agent_log_path(&data_dir, &sid, agent_id).with_file_name(format!("{agent_id}.result.md"));
    let full = std::fs::read_to_string(&spill)
        .unwrap_or_else(|e| panic!("full text should be spilled to {}: {e}", spill.display()));
    let expected = format!("Subagent long result start. {}", "密".repeat(33_000));
    assert_eq!(
        full, expected,
        "spilled file should be the untruncated full text"
    );
    agent.shutdown();
}

/// Plan mode rejection: delegating a subagent with plan mode on → "cannot delegate in plan mode".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_plan_mode_rejected() {
    let (config_path, cwd, data_dir) = setup("subagent-plan");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SetPlanMode {
            session_id: sid.clone(),
            enabled: true,
        })
        .await
        .unwrap();
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;

    let (output, is_error) = agent_end(&collected).expect("should have Agent ToolCallEnd");
    assert!(is_error, "plan-mode delegation should fail: {output}");
    assert!(
        output.contains("Subagents cannot be delegated in plan mode"),
        "{output}"
    );
    agent.shutdown();
}

/// (output, is_error) of the Agent tool card by call id
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

/// msg line count of the subagent context JSONL
fn agent_log_msg_count(data_dir: &Path, sid: &str, agent_id: &str) -> usize {
    let log = std::fs::read_to_string(agent_log_path(data_dir, sid, agent_id)).unwrap();
    log.lines()
        .filter(|line| line.contains("\"type\":\"msg\""))
        .count()
}

/// Background full link: run_in_background=true → the parent immediately
/// receives running+task_id; after the parent turn ends we wait for the
/// synthetic <task-notification> user message and the TurnComplete after it;
/// the task panel contains a subagent entry.
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
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the background full link"
        );
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
                assert!(!is_error, "background delegation should not fail: {output}");
                running_output = Some(output.clone());
            }
            Event::UserMessage { text, .. } if text.contains("<task-notification") => {
                notification = Some(text.clone());
            }
            Event::TaskListChanged { tasks, .. } => {
                saw_agent_task |= tasks.iter().any(|t| t.command.contains("Subagent"));
            }
            Event::TurnComplete { .. } => completes += 1,
            _ => {}
        }
        if completes >= 2 && notification.is_some() {
            break;
        }
    }

    // The parent immediately receives the running receipt (task_id queryable)
    let running = running_output.expect("should have background Agent ToolCallEnd");
    assert!(running.contains("status: running"), "{running}");
    assert!(running.contains("task_id: b"), "{running}");
    let agent_id = parse_agent_id(&running).to_string();

    // Completion notification: the synthetic user message arrives — the body
    // does not inline the full result, only a status line + the result file
    // path + a Read hint (kimi-code style); the full child conclusion lives in
    // result.md
    let notification = notification.expect("should have a synthetic <task-notification> message");
    assert!(notification.contains(&agent_id), "{notification}");
    assert!(
        !notification.contains(mock::SUBAGENT_CHILD_DONE),
        "notification body should not inline the full conclusion: {notification}"
    );
    assert!(notification.contains("completed"), "{notification}");
    assert!(
        notification.contains("took"),
        "body should include the duration: {notification}"
    );
    // The opening tag carries structured attributes (for the UI compact card; description is sanitized)
    assert!(
        notification.contains(&format!("agent_id=\"{agent_id}\"")),
        "opening tag should carry the agent_id attribute: {notification}"
    );
    assert!(
        notification.contains("status=\"completed\""),
        "opening tag should carry status: {notification}"
    );
    assert!(
        notification.contains("description=\"subagent selftest delegation\""),
        "opening tag should carry description: {notification}"
    );
    // Duration and record file path attributes (for the UI status card)
    assert!(
        notification.contains("duration_ms=\""),
        "opening tag should carry duration_ms: {notification}"
    );
    // The record path comes from Path::display(); the separator is platform-dependent (Windows uses \)
    let agents_sep = format!(".agents{}", std::path::MAIN_SEPARATOR);
    assert!(
        notification.contains("record=\"") && notification.contains(&agents_sep),
        "opening tag should carry the subagent context path: {notification}"
    );
    // Result full-text path attribute + body Read hint + the file holds the complete child conclusion
    assert!(
        notification.contains("result=\"") && notification.contains(".result.md"),
        "opening tag should carry the result file path: {notification}"
    );
    let result_path =
        agent_log_path(&data_dir, &sid, &agent_id).with_file_name(format!("{agent_id}.result.md"));
    assert!(
        notification.contains(&result_path.display().to_string()),
        "body should include the result file path: {notification}"
    );
    assert!(
        notification.contains("read it with Read"),
        "body should guide to Read: {notification}"
    );
    let full = std::fs::read_to_string(&result_path)
        .unwrap_or_else(|e| panic!("result file should exist at {}: {e}", result_path.display()));
    assert!(
        full.contains(mock::SUBAGENT_CHILD_DONE),
        "result file should hold the full child conclusion: {full}"
    );
    assert!(saw_agent_task, "task panel should contain a subagent entry");
    agent.shutdown();
}

/// resume continuation: run once in the foreground, then resume (new prompt) →
/// the jsonl grows, the result contains the new conclusion, and the
/// resume_hint's agent_id stays unchanged.
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
    let (output1, is_error1) = agent_end(&collected).expect("first Agent ToolCallEnd");
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
    assert!(!is_error2, "resume should succeed: {output2}");
    assert!(
        output2.contains(mock::SUBAGENT_RESUMED_DONE),
        "should contain the new resumed conclusion: {output2}"
    );
    assert!(output2.contains("status: completed"), "{output2}");
    assert_eq!(
        parse_agent_id(output2),
        agent_id,
        "resume_hint agent_id should stay unchanged"
    );
    let msgs_after = agent_log_msg_count(&data_dir, &sid, &agent_id);
    assert!(
        msgs_after >= msgs_before + 2,
        "resume should append >=2 messages (new prompt + new conclusion): {msgs_before} -> {msgs_after}"
    );
    agent.shutdown();
}

/// resume with unknown id: errors and lists available ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_unknown_id_lists_available() {
    let (config_path, cwd, data_dir) = setup("subagent-resume-unknown");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    // Run once first to obtain a real agent_id
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} GREP", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;
    let (output1, _) = agent_end(&collected).expect("first Agent ToolCallEnd");
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
    assert!(is_error2, "unknown id should fail: {output2}");
    assert!(
        output2.contains("a0-999"),
        "should name the unknown id: {output2}"
    );
    assert!(output2.contains("does not exist"), "{output2}");
    assert!(
        output2.contains(&real_id),
        "should list available agent_id: {output2}"
    );
    agent.shutdown();
}

/// resume conflict while running: resuming while the background subagent is still running → reports "still running".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_resume_running_conflict() {
    let (config_path, cwd, data_dir) = setup("subagent-resume-running");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let sid = new_session(&agent, cwd).await;

    // First message: a background LOOP subagent (runs forever)
    let collected = run_turn(
        &agent,
        &sid,
        &format!("{} RESUME_RUNNING", mock::SUBAGENT_TRIGGER),
        ExecMode::AutoEdit,
        None,
    )
    .await;
    let (output1, is_error1) =
        agent_end_with(&collected, "call_agent_bg").expect("background Agent ToolCallEnd");
    assert!(!is_error1, "{output1}");
    assert!(output1.contains("status: running"), "{output1}");

    // Second message: immediately resume the same id → conflict
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
    assert!(is_error2, "resume while running should fail: {output2}");
    assert!(output2.contains("still running"), "{output2}");
    agent.shutdown();
}

/// TaskStop a background subagent: the registry shows Killed, and no wake synthetic message is delivered.
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
    // The parent model did call TaskStop and it succeeded
    let (stop_output, stop_error) =
        agent_end_with(&collected, "call_taskstop").expect("should have TaskStop ToolCallEnd");
    assert!(!stop_error, "TaskStop should succeed: {stop_output}");

    // Wait one more window (a final TaskListChanged still arrives after the
    // driver is cancelled): the registry should show Killed; the event stream
    // must not contain a <task-notification> synthetic message
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
            .any(|t| t.command.contains("Subagent") && t.status == pig_protocol::TaskStatus::Killed))
    });
    assert!(
        killed,
        "registry should have a Killed subagent task: {collected:#?}"
    );
    assert!(
        !collected.iter().any(|e| matches!(
            e,
            Event::UserMessage { text, .. } if text.contains("<task-notification")
        )),
        "subagent killed by TaskStop should not wake the parent session: {collected:#?}"
    );
    agent.shutdown();
}

/// Background approval: under ConfirmBeforeEdit a background subagent's write
/// operation still goes through the pending approval gate — ApprovalRequested
/// appears → approve → the command executes → the completion notification
/// arrives.
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
    let mut bg_output: Option<String> = None;
    let mut completes = 0usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the background approval link"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::ApprovalRequested {
                request_id, tool, ..
            } => {
                assert_eq!(
                    tool, "Bash",
                    "background subagent Bash should prompt for approval"
                );
                saw_approval = true;
                agent
                    .ops
                    .send(Op::ApprovalReply {
                        request_id: request_id.clone(),
                        decision: ApprovalDecision::Allow,
                        feedback: None,
                    })
                    .await
                    .unwrap();
            }
            Event::UserMessage { text, .. } if text.contains("<task-notification") => {
                notification = Some(text.clone());
            }
            Event::ToolCallEnd {
                item_id, output, ..
            } if item_id.contains("call_agent_bg") => {
                bg_output = Some(output.clone());
            }
            Event::TurnComplete { .. } => completes += 1,
            _ => {}
        }
        if completes >= 2 && notification.is_some() {
            break;
        }
    }

    assert!(
        saw_approval,
        "background subagent should trigger the approval gate"
    );
    assert!(
        cwd.join("child_bash.txt").exists(),
        "after approval the background command actually executes (file created)"
    );
    let notification = notification.expect("should have a completion notification");
    // The notification body does not inline the full result; the child conclusion lives in result.md (see the body/result attribute for the path)
    assert!(
        !notification.contains(mock::SUBAGENT_CHILD_DONE),
        "notification body should not inline the full conclusion: {notification}"
    );
    assert!(
        notification.contains("read it with Read"),
        "notification body should guide to Read: {notification}"
    );
    let bg_output = bg_output.expect("should have background Agent ToolCallEnd");
    let agent_id = parse_agent_id(&bg_output);
    let result_path =
        agent_log_path(&data_dir, &sid, agent_id).with_file_name(format!("{agent_id}.result.md"));
    let full = std::fs::read_to_string(&result_path).expect("result file should exist");
    assert!(
        full.contains(mock::SUBAGENT_CHILD_DONE),
        "result file should contain the child conclusion: {full}"
    );
    agent.shutdown();
}

/// Background swarm (run_in_background): the immediate receipt lists
/// agent_id/task_id/status per item + a "do not poll" hint; two live
/// SubagentCards (background=true); each subagent wakes the parent session on
/// completion (two <task-notification> messages); the rollout's ToolCall
/// record carries agent_cards (2 cards); a simulated restart replay rebuilds
/// both cards (same item_id) and appends finished to each to settle the
/// terminal state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_background_receipt_and_cards_replayed() {
    let (config_path, cwd, data_dir) = setup("subagent-swarm-bg");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let sid = new_session(&agent, cwd.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} SWARMBG", mock::SUBAGENT_TRIGGER),
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    // Wait for two completion notifications (each subagent wakes once)
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut notifications = 0usize;
    let mut swarm_output: Option<String> = None;
    let mut live_cards: Vec<(String, String, bool)> = Vec::new();
    while notifications < 2 {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for two completion notifications"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::UserMessage { text, .. } if text.contains("<task-notification") => {
                notifications += 1;
            }
            Event::ToolCallEnd {
                item_id, output, ..
            } if item_id.contains("call_swarm_bg") => {
                swarm_output = Some(output.clone());
            }
            Event::SubagentCard {
                item_id,
                agent_id,
                background,
                ..
            } => live_cards.push((item_id.clone(), agent_id.clone(), *background)),
            _ => {}
        }
    }
    // Immediate receipt: header stats + per-item agent_id/task_id/status + the do-not-poll hint
    let output = swarm_output.expect("should have AgentSwarm ToolCallEnd");
    assert!(output.contains("2 subagents total"), "{output}");
    assert_eq!(
        output.matches("agent_id: ").count(),
        2,
        "listed per subagent: {output}"
    );
    assert_eq!(output.matches("task_id: ").count(), 2, "{output}");
    assert!(output.contains("status: running"), "{output}");
    assert!(
        output.contains("<task-notification>") && output.contains("do not poll"),
        "receipt should explain the notification semantics: {output}"
    );
    // Live: two background cards attach to the same tool card (same item_id), agent_ids distinct
    assert_eq!(live_cards.len(), 2, "two live cards: {live_cards:?}");
    assert!(live_cards.iter().all(|c| c.2), "background all true");
    assert_ne!(live_cards[0].1, live_cards[1].1, "agent_ids distinct");
    assert_eq!(
        live_cards[0].0, live_cards[1].0,
        "attached to the same tool card"
    );
    agent.shutdown();

    // Rollout: the AgentSwarm ToolCall record carries 2 agent_cards (background=true)
    let rollout_path = data_dir.join("sessions").join(format!("{sid}.jsonl"));
    let content = std::fs::read_to_string(&rollout_path).expect("rollout readable");
    let swarm_record = content
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|record| record["type"] == "tool_call" && record["tool"] == "AgentSwarm")
        .expect("rollout should have an AgentSwarm tool record");
    let agent_cards = swarm_record["agent_cards"]
        .as_array()
        .expect("agent_cards should be an array");
    assert_eq!(
        agent_cards.len(),
        2,
        "batch cards persisted: {swarm_record}"
    );
    assert!(
        agent_cards
            .iter()
            .all(|card| card["background"] == true && card["agent_id"].is_string()),
        "card metadata complete: {agent_cards:?}"
    );
    assert_eq!(
        swarm_record["agent_card"],
        serde_json::Value::Null,
        "single-card slot untouched"
    );

    // Simulate a restart and reopen the session: replay rebuilds both subagent cards and appends finished to each to settle the terminal state
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut collected = Vec::new();
    let mut finished_ids = std::collections::HashSet::new();
    while finished_ids.len() < 2 {
        assert!(
            Instant::now() < deadline,
            "replay should emit finished for both background subagents: {collected:#?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events2.recv()).await
        else {
            continue;
        };
        if let Event::SubagentActivity {
            agent_id,
            finished: true,
            ..
        } = &event
        {
            finished_ids.insert(agent_id.clone());
        }
        collected.push(event);
    }
    let replay_cards: Vec<(String, String, bool)> = collected
        .iter()
        .filter_map(|e| match e {
            Event::SubagentCard {
                item_id,
                agent_id,
                background,
                ..
            } => Some((item_id.clone(), agent_id.clone(), *background)),
            _ => None,
        })
        .collect();
    assert_eq!(
        replay_cards.len(),
        2,
        "replay rebuilds two cards: {replay_cards:?}"
    );
    assert!(
        replay_cards.iter().all(|c| c.2),
        "replayed cards background=true"
    );
    assert_eq!(
        replay_cards[0].0, replay_cards[1].0,
        "both replayed cards attach to the same AgentSwarm tool card"
    );
    // The replayed cards' agent_id matches live; finished points at the same set of agent_ids
    for (_, agent_id, _) in &replay_cards {
        assert!(
            finished_ids.contains(agent_id),
            "{agent_id} should have a finished event"
        );
        assert!(
            live_cards.iter().any(|c| &c.1 == agent_id),
            "replayed card should share the live card's agent_id: {agent_id}"
        );
    }
    agent2.shutdown();
}

/// Subagent card metadata persisted with the rollout and rebuilt on replay
/// (A3e): run one background subagent → simulate a restart OpenSession replay
/// → the event stream should have SubagentCard (full meta, background=true),
/// followed by a SubagentActivity finished for that agent_id (background tasks
/// do not outlive the process, so replay settles the terminal state, keeping
/// the replayed card from spinning forever).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_background_card_replayed() {
    let (config_path, cwd, data_dir) = setup("subagent-replay");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let sid = new_session(&agent, cwd.clone()).await;

    // Run one background subagent to completion (waiting for the completion notification = the task has terminated)
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
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the notification"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        if let Event::UserMessage { text, .. } = &event
            && text.contains("<task-notification")
        {
            break;
        }
    }
    agent.shutdown();

    // Simulate a restart and reopen the session: replay rebuild
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    // The last event of the replay sequence is that agent's finished (after the closing TurnComplete)
    let collected = recv_until(&events2, Duration::from_secs(20), |e| {
        matches!(e, Event::SubagentActivity { finished: true, .. })
    })
    .await;

    // SubagentCard: full meta, background=true
    let card = collected
        .iter()
        .find_map(|e| match e {
            Event::SubagentCard {
                agent_id,
                profile,
                description,
                model,
                background,
                ..
            } => Some((
                agent_id.clone(),
                profile.clone(),
                description.clone(),
                model.clone(),
                *background,
            )),
            _ => None,
        })
        .expect("replay should re-emit SubagentCard");
    assert!(card.4, "BG replay background should be true");
    assert_eq!(card.1, "explore");
    assert_eq!(card.2, "subagent selftest delegation");
    assert!(!card.3.is_empty(), "model should be non-empty");
    // The finished closing marker comes after the card and points at the same agent_id
    let card_pos = collected
        .iter()
        .position(|e| matches!(e, Event::SubagentCard { .. }))
        .expect("already asserted to exist");
    let has_finished = collected.iter().skip(card_pos).any(|e| {
        matches!(
            e,
            Event::SubagentActivity { agent_id, item: None, finished: true, .. }
                if *agent_id == card.0
        )
    });
    assert!(
        has_finished,
        "replay should append finished to settle the terminal state: {collected:#?}"
    );
    agent2.shutdown();
}
