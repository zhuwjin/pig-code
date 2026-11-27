mod common;

use common::{new_session, recv_until, setup};
use pig_protocol::{ApprovalDecision, CoreError, Event, ExecMode, Op};
use pig_provider::mock;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Collect events of one full turn; when `approval` is Some, approval requests are answered automatically with the given decision.
/// When `plan` is true, plan mode is enabled before the turn (orthogonally stacked with mode)
async fn run_scenario_b(
    mode: ExecMode,
    approval: Option<ApprovalDecision>,
    cwd_name: &str,
    plan: bool,
) -> (Vec<Event>, PathBuf, pig_core::AgentHandle, String) {
    let (config_path, dir, data_dir) = setup(cwd_name);
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;

    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: mode.permissions(),
        })
        .await
        .unwrap();
    if plan {
        agent
            .ops
            .send(Op::SetPlanMode {
                session_id: session_id.clone(),
                enabled: true,
            })
            .await
            .unwrap();
    }
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!(
                "{} create and modify files, then run a command",
                mock::SCENARIO_B_TRIGGER
            ),
            files: vec![],
            images: vec![],
            permissions: mode.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event
            && let Some(decision) = approval
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
    // (item_id, output, is_error) — item_id contains the tool call id
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

fn assert_shell_output(events: &[Event], call_id: &str, expected: &str) -> bool {
    let ends = tool_ends(events);
    let (_, output, is_error) = ends
        .iter()
        .find(|(id, _, _)| id.contains(call_id))
        .unwrap_or_else(|| panic!("missing shell result for {call_id}: {ends:?}"));
    if common::native_sandbox_usable() {
        assert!(
            !is_error && output.contains(expected),
            "expected {expected:?} from {call_id}: {ends:?}"
        );
        true
    } else {
        assert!(
            common::native_sandbox_refusal(output),
            "an unavailable native sandbox must reject {call_id}: {ends:?}"
        );
        false
    }
}

fn assert_shell_write(events: &[Event], call_id: &str, target: &std::path::Path) -> bool {
    let executed = assert_shell_output(events, call_id, "[exit code: 0]");
    assert_eq!(
        target.exists(),
        executed,
        "shell write must match the native sandbox outcome: {target:?}"
    );
    executed
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
        ExecMode::ReadOnly,
        Some(ApprovalDecision::Allow),
        "allow",
        false,
    )
    .await;

    assert_eq!(
        approvals(&events),
        ["Write", "Edit"],
        "ReadOnly asks for the file tools; Bash runs read-only without a prompt"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TurnComplete { .. })),
        "turn completes normally"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::SCENARIO_B_MARKER)
        )),
        "final text contains the scenario B marker"
    );

    assert_shell_output(&events, "call_b_bash", mock::SCENARIO_B_BASH_MARKER);

    // The turn reports measured per-call tool spans (Write + Edit + Bash, the
    // gate and its approval wait included), not a residual
    let stats = events
        .iter()
        .find_map(|e| match e {
            Event::TurnComplete {
                stats: Some(stats), ..
            } => Some(*stats),
            _ => None,
        })
        .expect("turn stats are reported");
    assert!(
        stats.tool_ms > 0,
        "a turn that ran three tools must report a tool span: {stats:?}"
    );
    assert!(
        stats.tool_exec_ms > 0,
        "pure execution is measured: {stats:?}"
    );
    assert!(
        stats.tool_wall_ms > 0,
        "tool wall time is measured: {stats:?}"
    );
    assert!(stats.tool_exec_ms <= stats.tool_wall_ms, "{stats:?}");
    assert!(stats.tool_wall_ms <= stats.tool_ms, "{stats:?}");
    assert!(stats.overhead_ms <= stats.duration_ms, "{stats:?}");

    let changes = file_changes(&events);
    assert_eq!(
        changes.len(),
        2,
        "Write+Edit each yield one FileChanged: {changes:?}"
    );
    let (path, diff, adds, dels) = changes[0];
    assert_eq!(path, mock::SCENARIO_B_FILE);
    assert_eq!((adds, dels), (3, 0), "new file is all additions: {diff}");
    assert!(diff.contains("+line2"));
    let (_, diff2, adds2, dels2) = changes[1];
    assert_eq!(
        (adds2, dels2),
        (3, 0),
        "diff is always original (file missing)→current: {diff2}"
    );
    assert!(
        diff2.contains("+LINE2") && !diff2.contains("-line2"),
        "diff is original→current: {diff2}"
    );

    let file = dir.join(mock::SCENARIO_B_FILE);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "hello\nLINE2\nline3\n",
        "file content correctly modified by Write+Edit"
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
            assert!(
                Instant::now() < deadline,
                "timed out waiting for FileReverted"
            );
            continue;
        };
        if matches!(&event, Event::FileReverted { path, .. } if path == mock::SCENARIO_B_FILE) {
            break;
        }
    }
    assert!(
        !file.exists(),
        "newly created file should be deleted after revert"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_b_deny_all() {
    let (events, dir, agent, _) = run_scenario_b(
        ExecMode::ReadOnly,
        Some(ApprovalDecision::Reject),
        "deny",
        false,
    )
    .await;

    assert_eq!(approvals(&events), ["Write", "Edit"]);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TurnComplete { .. })),
        "turn still runs to completion after rejections"
    );
    let denied = tool_ends(&events)
        .iter()
        .filter(|(_, out, err)| out.contains("The user rejected this action") && *err)
        .count();
    assert_eq!(
        denied,
        2,
        "the two file tools were rejected: {:?}",
        tool_ends(&events)
    );
    assert_shell_output(&events, "call_b_bash", mock::SCENARIO_B_BASH_MARKER);
    assert!(file_changes(&events).is_empty(), "no file changes");
    assert!(
        !dir.join(mock::SCENARIO_B_FILE).exists(),
        "file not created"
    );
    agent.shutdown();
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_blocks_an_unrequested_shell_write() {
    // ReadOnly is a sandbox guarantee, not a command classifier: a shell write
    // with no escalation request runs directly and is refused by the kernel.
    let (events, dir, agent) = run_trigger(
        ExecMode::ReadOnly,
        None,
        mock::SCENARIO_PLAN_BASH_TRIGGER,
        "read-only-write",
        None,
        false,
    )
    .await;
    assert!(
        approval_details(&events).is_empty(),
        "ReadOnly does not prompt for shell commands: {:?}",
        approval_details(&events)
    );
    assert!(
        !dir.join(mock::PLAN_BASH_FILE).exists(),
        "the unrequested write must not create the file"
    );
    assert!(
        tool_ends(&events)
            .iter()
            .any(|(_, out, _)| out.contains("[exit code: 1]")),
        "the shell command fails instead of silently writing: {:?}",
        tool_ends(&events)
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_escalation_prompts_and_writes_the_approved_root() {
    let (config_path, dir, data_dir) = setup("read-only-escalation");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    let target = dir.join("escalated.txt");
    let target_token = target.to_string_lossy().replace('\\', "/");

    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::ReadOnly.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} {target_token}", mock::SCENARIO_ESCALATION_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out: {collected:#?}");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id, detail, ..
        } = &event
        {
            assert!(
                detail.contains("Widen the sandbox"),
                "the dialog explains the widening: {detail}"
            );
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
        let done = matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. }
        );
        collected.push(event);
        if done {
            break;
        }
    }
    assert_shell_write(&collected, "call_esc_1", &target);
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::ESCALATION_MARKER)
        )),
        "the shell command completed: {collected:#?}"
    );
    agent.shutdown();
}

/// Auto already grants the workspace: a widening that only names writable
/// roots is redundant, so it runs without a dialog and the output carries the
/// guidance (ReadOnly keeps prompting — see the test above).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_widening_for_the_workspace_runs_without_a_dialog() {
    let (config_path, dir, data_dir) = setup("workspace-widening");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    let target = dir.join("redundant.txt");
    let target_token = target.to_string_lossy().replace('\\', "/");

    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} {target_token}", mock::SCENARIO_ESCALATION_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out: {collected:#?}");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        let done = matches!(event, Event::TurnComplete { .. });
        collected.push(event);
        if done {
            break;
        }
    }

    assert!(
        !collected
            .iter()
            .any(|event| matches!(event, Event::ApprovalRequested { .. })),
        "a widening that names only writable roots must not prompt: {collected:#?}"
    );
    assert_shell_write(&collected, "call_esc_1", &target);
    let ends = tool_ends(&collected);
    assert!(
        ends.iter().any(|(_, output, is_error)| {
            output.contains("already covered by the session's writable roots") && !is_error
        }),
        "the output carries the guidance: {ends:?}"
    );
    agent.shutdown();
}

/// A widening that names a workspace-metadata path must say so: the path sits
/// inside the writable root but is read-only (the sandbox expresses it as a
/// deeper read-only entry), so calling it "already writable" and dropping the
/// request reads as "the request was ignored" and sends the model into an
/// escalation retry that cannot work either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_widening_for_workspace_metadata_is_rejected_with_the_reason() {
    let (config_path, dir, data_dir) = setup("metadata-widening");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    let target = dir.join(".pigcode").join("blocked.txt");
    let target_token = target.to_string_lossy().replace('\\', "/");

    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} {target_token}", mock::SCENARIO_ESCALATION_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out: {collected:#?}");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        let done = matches!(event, Event::TurnComplete { .. });
        collected.push(event);
        if done {
            break;
        }
    }

    assert!(
        !collected
            .iter()
            .any(|event| matches!(event, Event::ApprovalRequested { .. })),
        "workspace metadata cannot be widened, so no dialog should show: {collected:#?}"
    );
    assert!(!target.exists(), "the write never ran: {collected:#?}");
    let ends = tool_ends(&collected);
    assert!(
        ends.iter().any(|(_, output, is_error)| {
            *is_error
                && output.contains("read-only workspace metadata")
                && output.contains("cannot lift it")
                && !output.contains("already covered by the session's writable roots")
        }),
        "the output names the real constraint: {ends:?}"
    );
    agent.shutdown();
}

/// A sandbox widening offers no remembering tier (Codex parity: only
/// approve/abort, so every boundary move stays visible). A legacy client can
/// still answer `ApprovedForSession`; core treats it as a one-shot approval,
/// and the next command that needs the same root asks again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_widening_offers_no_session_tier() {
    let (config_path, dir, data_dir) = setup("session-grant");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    let root = dir.join("outside");
    std::fs::create_dir_all(&root).unwrap();
    let root_token = root.to_string_lossy().replace('\\', "/");

    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::ReadOnly.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} {root_token}", mock::SESSION_GRANT_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut approvals = 0u32;
    let mut saw_structured_request = false;
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out: {collected:#?}");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id,
            requested_write_roots,
            available_decisions,
            ..
        } = &event
        {
            approvals += 1;
            // The dialog carries the roots as data, not only inside the text.
            saw_structured_request |= requested_write_roots
                .iter()
                .any(|entry| entry.contains("outside"));
            assert!(
                !available_decisions.contains(&ApprovalDecision::ApprovedForSession),
                "a widening must not offer the session option: {available_decisions:?}"
            );
            assert!(
                !available_decisions.contains(&ApprovalDecision::ApprovedForProject),
                "a widening must not offer the project-rule option: {available_decisions:?}"
            );
            assert_eq!(
                available_decisions,
                &vec![ApprovalDecision::Allow, ApprovalDecision::Reject],
                "the widening dialog is approve/abort only"
            );
            // A legacy client still answers with the session tier: one shot.
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::ApprovedForSession,
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

    assert_eq!(
        approvals, 2,
        "the second command asks again (no session memory): {collected:#?}"
    );
    assert!(saw_structured_request, "the request lists the roots");
    assert_shell_write(&collected, "call_sg_1", &root.join("grant-a.txt"));
    assert_shell_write(&collected, "call_sg_2", &root.join("grant-b.txt"));
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::SESSION_GRANT_MARKER)
        )),
        "the turn finished: {collected:#?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_needs_no_approval() {
    let (events, dir, agent, _) = run_scenario_b(ExecMode::Auto, None, "auto", false).await;

    assert!(
        approvals(&events).is_empty(),
        "Auto: no routine approvals, including Bash"
    );
    assert!(dir.join(mock::SCENARIO_B_FILE).exists());
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_blocks_writes_without_approval() {
    // Plan on + read-only: mutating tools are hard-rejected by the plan; not even an approval card pops up
    let (events, dir, agent, _) = run_scenario_b(ExecMode::ReadOnly, None, "plan", true).await;

    assert!(
        approvals(&events).is_empty(),
        "no approval prompts in plan mode"
    );
    let blocked = tool_ends(&events)
        .iter()
        .filter(|(_, out, _)| out.contains("Plan mode"))
        .count();
    assert_eq!(
        blocked,
        3,
        "all three mutating tools blocked by plan mode: {:?}",
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
async fn plan_mode_overrides_auto() {
    // "Full access + plan": plan overrides full access — writes are still hard-rejected (same semantics as ZCode)
    let (events, dir, agent, _) = run_scenario_b(ExecMode::Auto, None, "plan-full", true).await;

    assert!(
        approvals(&events).is_empty(),
        "no approval prompts in plan mode"
    );
    let blocked = tool_ends(&events)
        .iter()
        .filter(|(_, out, _)| out.contains("Plan mode"))
        .count();
    assert_eq!(
        blocked,
        3,
        "writes still hard-rejected under full access: {:?}",
        tool_ends(&events)
    );
    assert!(!dir.join(mock::SCENARIO_B_FILE).exists());
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_no_approvals() {
    let (events, dir, agent, _) = run_scenario_b(ExecMode::Auto, None, "full", false).await;

    assert!(
        approvals(&events).is_empty(),
        "no approval prompts under full access"
    );
    assert!(dir.join(mock::SCENARIO_B_FILE).exists());
    assert_shell_output(&events, "call_b_bash", mock::SCENARIO_B_BASH_MARKER);
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
            content: format!("{} run the modification chain", mock::SCENARIO_B_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
            Event::TurnComplete { .. } => {
                panic!("interrupt during approval should not TurnComplete")
            }
            _ => {}
        }
    }
    assert!(
        interrupted && aborted,
        "Interrupt while waiting for approval should unblock and abort"
    );
    agent.shutdown();
}

// ---------- Dangerous command forced approval (blocklist hit = popup, all modes) ----------

/// Dangerous-command scenario driver: every approval popup is answered with decision.
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
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: mode.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} run a dangerous command", mock::SCENARIO_DANGER_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: mode.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
    (collected, dir, agent)
}

fn approval_details(events: &[Event]) -> Vec<(&str, &str, Option<&str>)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ApprovalRequested {
                tool,
                detail,
                danger_key,
                ..
            } => Some((tool.as_str(), detail.as_str(), danger_key.as_deref())),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn danger_auto_allow_executes() {
    // Auto normally does not ask about Bash, but dangerous commands must
    // prompt; the mock's mkfs hits the blocklist yet is harmless to run
    // (command missing → exit 127 / no args → just prints usage) — a non-zero
    // exit is still enough to prove the execution path was taken
    let (events, _dir, agent) =
        run_danger(ExecMode::Auto, ApprovalDecision::Allow, "danger-allow").await;
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        2,
        "both dangerous commands should prompt: {details:?}"
    );
    assert!(
        details.iter().all(|(tool, d, key)| *tool == "ExecCommand"
            && *key == Some("disk_format")
            && d.contains("mkfs")),
        "danger command: danger_key=disk_format + detail is plain command text: {details:?}"
    );
    let ends = tool_ends(&events);
    assert!(
        ends.iter()
            .any(|(_, out, err)| !err && out.contains("[exit code:")),
        "should actually execute after Allow: {ends:?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn danger_auto_reject() {
    let (events, dir, agent) =
        run_danger(ExecMode::Auto, ApprovalDecision::Reject, "danger-reject").await;
    let ends = tool_ends(&events);
    assert!(
        ends.iter()
            .any(|(_, out, err)| *err && out.contains("The user rejected this high-risk command")),
        "receipt to the model on Reject: {ends:?}"
    );
    assert!(
        !ends.iter().any(|(_, out, _)| out.contains("[exit code:")),
        "reject path should not execute: {ends:?}"
    );
    // No file side effects (the scenario only has Bash)
    assert!(file_changes(&events).is_empty());
    let _ = dir;
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn danger_read_only_danger_key() {
    let (events, _dir, agent) =
        run_danger(ExecMode::ReadOnly, ApprovalDecision::Allow, "danger-cbe").await;
    let details = approval_details(&events);
    assert!(
        !details.is_empty(),
        "ReadOnly still prompts for dangerous Bash"
    );
    assert!(
        details.iter().all(|(_, _, key)| key.is_some()),
        "danger prompt should carry danger_key (GUI renders the warning title by key): {details:?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn danger_always_allow_not_remembered() {
    // Pressing AlwaysAllow on a dangerous command is not recorded into always_allowed: the second dangerous command in the same session still prompts
    let (events, _dir, agent) = run_danger(
        ExecMode::Auto,
        ApprovalDecision::ApprovedForSession,
        "danger-always",
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        2,
        "second dangerous command still prompts (not remembered): {details:?}"
    );
    agent.shutdown();
}

/// A high-risk command offers neither remembering tier: danger is handled by
/// the popup every time, so the dialog is approve/abort only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn danger_prompt_offers_no_remembering_tier() {
    let (events, _dir, agent) =
        run_danger(ExecMode::Auto, ApprovalDecision::Allow, "danger-tiers").await;
    let mut decisions: Vec<Vec<ApprovalDecision>> = events
        .iter()
        .filter_map(|event| match event {
            Event::ApprovalRequested {
                available_decisions,
                ..
            } => Some(available_decisions.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(decisions.len(), 2, "both dangerous commands prompt");
    for list in decisions.drain(..) {
        assert_eq!(
            list,
            vec![ApprovalDecision::Allow, ApprovalDecision::Reject]
        );
    }
    agent.shutdown();
}

// ---------- Full (no sandbox, no routine approvals) ----------

/// Every preset's wire name round-trips.
#[test]
fn exec_mode_serde_roundtrip() {
    for mode in [ExecMode::ReadOnly, ExecMode::Auto, ExecMode::Full] {
        let json = serde_json::to_string(&mode).unwrap();
        let back: ExecMode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, mode, "{json}");
    }
}

/// Approval decisions keep their wire names (rollouts and GUI replies carry
/// them); the project tier is snake_cased.
#[test]
fn approval_decision_wire_names() {
    let cases = [
        (ApprovalDecision::Allow, "\"Allow\""),
        (
            ApprovalDecision::ApprovedForSession,
            "\"ApprovedForSession\"",
        ),
        (
            ApprovalDecision::ApprovedForProject,
            "\"approved_for_project\"",
        ),
        (ApprovalDecision::Reject, "\"Reject\""),
        (ApprovalDecision::DeferToUser, "\"DeferToUser\""),
        (ApprovalDecision::Abort, "\"Abort\""),
    ];
    for (decision, wire) in cases {
        assert_eq!(serde_json::to_string(&decision).unwrap(), wire);
        let back: ApprovalDecision = serde_json::from_str(wire).unwrap();
        assert_eq!(back, decision);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_danger_is_rejected_without_dialog() {
    // Full runs without a sandbox and without confirmations, but dangerous
    // commands are rejected outright (Codex `Never` parity); the mock's mkfs
    // commands never reach execution.
    let (events, _dir, agent) =
        run_danger(ExecMode::Full, ApprovalDecision::Allow, "full-danger").await;
    assert!(
        approval_details(&events).is_empty(),
        "no approval prompts under Full"
    );
    let ends = tool_ends(&events);
    assert_eq!(
        ends.iter()
            .filter(|(_, out, err)| *err && out.contains("Blocked: high-risk command"))
            .count(),
        2,
        "both dangerous commands are rejected: {ends:?}"
    );
    assert!(
        !ends.iter().any(|(_, out, _)| out.contains("[exit code:")),
        "rejected commands never execute: {ends:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TurnComplete { .. })),
        "turn completes after the rejections"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_normal_flow_no_dialogs() {
    let (events, dir, agent, _session_id) =
        run_scenario_b(ExecMode::Full, None, "full-b", false).await;
    assert!(
        approvals(&events).is_empty(),
        "Write/Edit/Bash never prompt under Full"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TurnComplete { .. })),
        "turn completes normally"
    );
    // Write+Edit took effect
    let file = dir.join(mock::SCENARIO_B_FILE);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "hello\nLINE2\nline3\n"
    );
    // Bash executed successfully
    let ends = tool_ends(&events);
    assert!(
        ends.iter().any(|(id, out, err)| id.contains("call_b_bash")
            && out.contains(mock::SCENARIO_B_BASH_MARKER)
            && !err),
        "Bash output contains the marker: {ends:?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_sensitive_file_still_blocked() {
    // Sensitive-file protection lives at the tool execute layer, independent of
    // mode (unconditionally effective under Full too): tool::execute is
    // mode-unaware; directly verify that reading .env is still denied
    let dir = std::env::temp_dir().join(format!("pig-core-full-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::write(dir.join(".env"), "SECRET=1\n").unwrap();

    let mut tracker = pig_core::tool::ChangeTracker::default();
    let state = pig_core::task::SessionToolState::for_test();
    let call = pig_provider::ToolCall {
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
            fs_grant: None,
            sandbox_override: Default::default(),
            sandbox_profile: Default::default(),
            additional_permissions: Default::default(),
        },
    )
    .await;
    assert!(is_error, "{out}");
    // Two independent layers refuse it in every mode: the sandbox veto (the
    // built-in `env` group) and the tool-level name check. The test accepts
    // either message; what matters is that the read fails and nothing leaks.
    assert!(
        out.contains("sensitive file") || out.contains("sandbox deny rule"),
        ".env still unreadable under Full: {out}"
    );
    assert!(!out.contains("SECRET=1"), "content must not leak: {out}");
}

// ---------- AlwaysAllow subject granularity / Auto read-only pass-through / TaskStop exemption ----------

/// Generic trigger-scenario driver: every approval popup is answered with
/// decision (None = no reply; a popup would hang until timeout).
/// When `permissions` is non-empty, .pigcode/permissions.toml is written
/// before new_session (rules load with the session).
/// When `plan` is true, plan mode is enabled before the turn (orthogonally
/// stacked with mode)
async fn run_trigger(
    mode: ExecMode,
    decision: Option<ApprovalDecision>,
    trigger: &str,
    cwd_name: &str,
    permissions: Option<&str>,
    plan: bool,
) -> (Vec<Event>, PathBuf, pig_core::AgentHandle) {
    run_trigger_with(setup, mode, decision, trigger, cwd_name, permissions, plan).await
}

#[allow(clippy::too_many_arguments)]
async fn run_trigger_with(
    setup_fn: fn(&str) -> (PathBuf, PathBuf, PathBuf),
    mode: ExecMode,
    decision: Option<ApprovalDecision>,
    trigger: &str,
    cwd_name: &str,
    permissions: Option<&str>,
    plan: bool,
) -> (Vec<Event>, PathBuf, pig_core::AgentHandle) {
    let (config_path, dir, data_dir) = setup_fn(cwd_name);
    if let Some(permissions) = permissions {
        std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
        std::fs::write(dir.join(".pigcode/permissions.toml"), permissions).unwrap();
    }
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;

    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: mode.permissions(),
        })
        .await
        .unwrap();
    if plan {
        agent
            .ops
            .send(Op::SetPlanMode {
                session_id: session_id.clone(),
                enabled: true,
            })
            .await
            .unwrap();
    }
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{trigger} start"),
            files: vec![],
            images: vec![],
            permissions: mode.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event
            && let Some(decision) = decision
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
    (collected, dir, agent)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn always_allow_is_per_subject() {
    // Scenario: Write a → Write a (same subject, no prompt) → Write b
    // (different subject, prompt) → Bash echo → Bash echo (same first word, no
    // prompt) → Bash ls (different first word, no prompt either: ReadOnly
    // shells run without routine approvals; the read-only sandbox is the gate).
    let (events, dir, agent) = run_trigger(
        ExecMode::ReadOnly,
        Some(ApprovalDecision::ApprovedForSession),
        mock::SCENARIO_SUBJECT_TRIGGER,
        "subject",
        None,
        false,
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        2,
        "first a/first b each prompt once; the second a skips and Bash never asks: {details:?}"
    );
    assert!(details[0].1.contains(mock::SUBJECT_FILE_A), "{details:?}");
    assert!(details[1].1.contains(mock::SUBJECT_FILE_B), "{details:?}");
    // The turn completes and the tools all really executed
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::SUBJECT_MARKER)
        )),
        "turn completes"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(mock::SUBJECT_FILE_A)).unwrap(),
        "v1\n",
        "second Write of the same subject skips the prompt and executes"
    );
    assert!(dir.join(mock::SUBJECT_FILE_B).exists());
    agent.shutdown();
}

/// A model-requested sandbox widening asks, even in Auto, when the root is
/// genuinely outside the writable set — and the approved roots become writable
/// for that command only. (A root the session already writes is dropped as
/// redundant instead; see `a_widening_for_the_workspace_runs_without_a_dialog`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escalation_request_always_prompts_and_grants_once() {
    let (config_path, dir, data_dir) = setup("escalation-approval");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    // Outside the workspace AND outside tmp — both are writable in Auto, and
    // the workspace sits inside the temp dir on this host.
    let outside = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tmp")
        .join(format!("pig-escalation-approval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("escalated.txt");
    let target_token = target.to_string_lossy().replace('\\', "/");

    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::Auto.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} {target_token}", mock::SCENARIO_ESCALATION_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out: {collected:#?}");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id, detail, ..
        } = &event
        {
            assert!(
                detail.contains("Widen the sandbox"),
                "the dialog explains the widening: {detail}"
            );
            assert!(
                detail.contains("outside the workspace") || detail.contains("artifact"),
                "the model's justification is shown: {detail}"
            );
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
        let done = matches!(event, Event::TurnComplete { .. });
        collected.push(event);
        if done {
            break;
        }
    }

    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ApprovalRequested { .. })),
        "an explicit widening request must always prompt: {collected:#?}"
    );
    assert_shell_write(&collected, "call_esc_1", &target);
    let _ = std::fs::remove_dir_all(&outside);
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_bash_runs_without_approval() {
    // Auto + allowlisted read-only command (ls): executes directly without a prompt (decision=None; a popup would hang until timeout)
    let (events, _dir, agent) = run_trigger(
        ExecMode::Auto,
        None,
        mock::SCENARIO_READONLY_TRIGGER,
        "readonly",
        None,
        false,
    )
    .await;
    assert!(
        approval_details(&events).is_empty(),
        "read-only command should not prompt"
    );
    assert_shell_output(&events, "call_ro_ls", "[exit code: 0]");
    agent.shutdown();
}

#[test]
fn approval_subject_extracts() {
    let call = |name: &str, args: serde_json::Value| pig_provider::ToolCall {
        id: "t1".into(),
        name: name.into(),
        arguments: args.to_string(),
    };
    assert_eq!(
        pig_core::tool::approval_subject(&call(
            "ExecCommand",
            serde_json::json!({"command": "cargo build --release"})
        )),
        "cargo"
    );
    assert_eq!(
        pig_core::tool::approval_subject(&call(
            "ExecCommand",
            serde_json::json!({"command": "  ls  -la"})
        )),
        "ls",
        "first word after leading whitespace"
    );
    assert_eq!(
        pig_core::tool::approval_subject(&call("ExecCommand", serde_json::json!({"command": ""}))),
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
    // Other tools → empty string (tool-level memory)
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
        .expect("TaskStop exists");
    assert!(stop.read_only(), "TaskStop should be marked read-only");
    assert!(
        !pig_core::tool::requires_approval(stop.as_ref(), ExecMode::ReadOnly.permissions()),
        "exempt even under ReadOnly"
    );
    assert!(
        !pig_core::tool::requires_approval(stop.as_ref(), ExecMode::Auto.permissions()),
        "read-only also exempt under Auto"
    );
}

// ---------- Project-level permission rules (priority chain) / ExitPlanMode ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_deny_hard_rejects_even_in_auto() {
    // Auto + deny ExecCommand(mkfs*): not even the danger popup is sent; the project rule hard-rejects directly
    let (events, _dir, agent) = run_trigger(
        ExecMode::Auto,
        Some(ApprovalDecision::Allow),
        mock::SCENARIO_DANGER_TRIGGER,
        "perm-deny",
        Some("deny = [\"ExecCommand(mkfs*)\"]\n"),
        false,
    )
    .await;
    assert!(
        approval_details(&events).is_empty(),
        "deny takes precedence over the danger prompt"
    );
    let ends = tool_ends(&events);
    assert_eq!(ends.len(), 2, "both mkfs commands rejected: {ends:?}");
    assert!(
        ends.iter().all(|(_, out, err)| *err
            && out.contains("Blocked by a project rule")
            && out.contains("ExecCommand(mkfs*)")),
        "deny receipt contains the rule text: {ends:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::DANGER_MARKER)
        )),
        "turn completes normally"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_allow_skips_approval() {
    // ReadOnly shells do not prompt by default; the allow rule is still accepted and the command runs
    let (events, _dir, agent) = run_trigger(
        ExecMode::ReadOnly,
        None,
        mock::SCENARIO_READONLY_TRIGGER,
        "perm-allow",
        Some("allow = [\"Bash(ls)\"]\n"),
        false,
    )
    .await;
    assert!(approval_details(&events).is_empty(), "allow skips approval");
    assert_shell_output(&events, "call_ro_ls", "[exit code: 0]");
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_allow_does_not_exempt_danger() {
    // An allow hit on a dangerous command still prompts (the danger check runs before allow)
    let (events, _dir, agent) = run_trigger(
        ExecMode::ReadOnly,
        Some(ApprovalDecision::Allow),
        mock::SCENARIO_DANGER_TRIGGER,
        "perm-danger-allow",
        Some("allow = [\"Bash(mkfs*)\"]\n"),
        false,
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        2,
        "dangerous commands still prompt: {details:?}"
    );
    assert!(
        details.iter().all(|(_, _, key)| key.is_some()),
        "{details:?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_rule_tier_is_offered_and_applied_live() {
    // "Remember to project rules": the decision appends a conservative rule to
    // .pigcode/permissions.toml and applies it to the live session at once, so
    // the second Write of the same path runs approval-free without a reload.
    // A fresh session in the same workspace loads the rule back from disk —
    // project `allow` rules are admitted only by the real workspace-trust
    // decision (the same one that admits project hooks).
    let (config_path, dir, data_dir) = setup("project-rule");
    let canonical = dir
        .canonicalize()
        .unwrap_or_else(|_| dir.clone())
        .display()
        .to_string()
        .replace('\\', "\\\\");
    let mut raw = std::fs::read_to_string(&config_path).unwrap();
    raw.push_str(&format!(
        "\n[hooks]\ntrusted_projects = [\"{canonical}\"]\n"
    ));
    std::fs::write(&config_path, raw).unwrap();
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::ReadOnly.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{} start", mock::PROJECT_RULE_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut approvals = 0u32;
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out: {collected:#?}");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id,
            available_decisions,
            ..
        } = &event
        {
            approvals += 1;
            assert!(
                available_decisions.contains(&ApprovalDecision::ApprovedForProject),
                "an ordinary Write approval offers the project tier: {available_decisions:?}"
            );
            assert!(
                available_decisions.contains(&ApprovalDecision::ApprovedForSession),
                "the session tier stays available too: {available_decisions:?}"
            );
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::ApprovedForProject,
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
    assert_eq!(
        approvals, 1,
        "the rule covers the second write in the same turn: {collected:#?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(mock::PROJECT_RULE_FILE)).unwrap(),
        "v2\n",
        "both writes ran"
    );
    let file = std::fs::read_to_string(dir.join(".pigcode/permissions.toml"))
        .expect("the rule file was created");
    assert!(
        file.contains(&format!("Write({})", mock::PROJECT_RULE_FILE)),
        "the exact path became a rule: {file}"
    );
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::PROJECT_RULE_MARKER)
        )),
        "the turn finished: {collected:#?}"
    );

    // A new session loads the rule from disk: the same write needs no dialog.
    let second = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: second.clone(),
            permissions: ExecMode::ReadOnly.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: second,
            content: format!("{} start", mock::PROJECT_RULE_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let mut dialogs = 0u32;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "second turn timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if matches!(event, Event::ApprovalRequested { .. }) {
            dialogs += 1;
        }
        if matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. }
        ) {
            break;
        }
    }
    assert_eq!(
        dialogs, 0,
        "a persisted project rule is loaded by the next session"
    );
    agent.shutdown();
}

/// The same persisted project rule must NOT skip approval while the workspace
/// is untrusted: project allow rules are admitted only by the explicit trust
/// decision (`[hooks] trusted_projects`), never by writing the file itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_rule_requires_workspace_trust() {
    let (config_path, dir, data_dir) = setup("project-rule-untrusted");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::ReadOnly.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} start", mock::PROJECT_RULE_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut approvals = 0u32;
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "first turn timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id,
            available_decisions,
            ..
        } = &event
        {
            approvals += 1;
            assert!(
                !available_decisions.contains(&ApprovalDecision::ApprovedForProject),
                "an untrusted workspace must not offer a durable project rule: \
                 {available_decisions:?}"
            );
            // A legacy client that still answers with the project tier gets a
            // one-shot approval: no rule is written, so the second write asks
            // again.
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::ApprovedForProject,
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
    assert_eq!(
        approvals, 2,
        "neither write gains a durable rule: {collected:#?}"
    );
    assert!(
        !dir.join(".pigcode/permissions.toml").exists(),
        "an untrusted workspace never pretends to persist a project rule"
    );

    // Same workspace, no trust entry: nothing was saved, so the dialog must
    // still appear for the writes in a later session.
    let second = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: second.clone(),
            permissions: ExecMode::ReadOnly.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: second,
            content: format!("{} start", mock::PROJECT_RULE_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let mut dialogs = 0u32;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "second turn timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event {
            dialogs += 1;
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
        if matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. }
        ) {
            break;
        }
    }
    assert_eq!(
        dialogs, 2,
        "an untrusted workspace must keep asking for both writes despite its project allow rule"
    );
    agent.shutdown();
}

/// Trust is persisted **before** any project surface activates. A failed config
/// save must leave the session untrusted: no `ProjectHooksTrusted` and project
/// allow rules must not skip their dialog.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_trust_save_does_not_activate_project_surfaces() {
    let (config_path, dir, data_dir) = setup("trust-save-failure");
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    std::fs::write(
        dir.join(".pigcode/permissions.toml"),
        format!("allow = [\"Write({})\"]\n", mock::PROJECT_RULE_FILE),
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path.clone()), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::ReadOnly.permissions(),
        })
        .await
        .unwrap();

    // The agent loaded its config at startup; replacing the path with a
    // directory makes the next atomic save fail (temp + rename over it).
    std::fs::remove_file(&config_path).unwrap();
    std::fs::create_dir(&config_path).unwrap();
    agent
        .ops
        .send(Op::TrustProjectHooks {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();

    let mut save_failed = false;
    let mut trusted = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::Error { error, .. }
                if matches!(error.semantic_error(), CoreError::ConfigWrite { .. }) =>
            {
                save_failed = true;
                break;
            }
            Event::ProjectHooksTrusted { .. } => {
                trusted = true;
                break;
            }
            _ => {}
        }
    }
    assert!(save_failed, "the failed trust save must be surfaced");
    assert!(!trusted, "a failed save must not activate trust");

    // The pre-existing project allow rule must still be inert.
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} start", mock::PROJECT_RULE_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let mut dialogs = 0u32;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "turn timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event {
            dialogs += 1;
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
        if matches!(event, Event::ProjectHooksTrusted { .. }) {
            trusted = true;
        }
        if matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. }
        ) {
            break;
        }
    }
    assert_eq!(
        dialogs, 2,
        "a project allow rule must stay inert after a failed trust save"
    );
    assert!(!trusted, "trust must not be activated later in the session");
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_level_deny_wins_over_the_project_allow() {
    // Both levels load into one session: a deny in the user file
    // ({data_dir}/permissions.toml) hard-rejects even though the project file
    // allows the same subject, and no dialog is raised.
    let (config_path, dir, data_dir) = setup("user-perms");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(dir.join(".pigcode")).unwrap();
    std::fs::write(
        data_dir.join("permissions.toml"),
        format!("deny = [\"Write({})\"]\n", mock::PROJECT_RULE_FILE),
    )
    .unwrap();
    std::fs::write(
        dir.join(".pigcode/permissions.toml"),
        format!("allow = [\"Write({})\"]\n", mock::PROJECT_RULE_FILE),
    )
    .unwrap();
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPermissions {
            session_id: session_id.clone(),
            permissions: ExecMode::ReadOnly.permissions(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!("{} start", mock::PROJECT_RULE_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out: {collected:#?}");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        let done = matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. }
        );
        collected.push(event);
        if done {
            break;
        }
    }
    assert!(
        approval_details(&collected).is_empty(),
        "the deny beats the project allow without a dialog"
    );
    let ends = tool_ends(&collected);
    assert_eq!(ends.len(), 2, "both writes rejected: {ends:?}");
    assert!(
        ends.iter().all(|(_, out, err)| *err
            && out.contains("Blocked by a user rule")
            && out.contains("permissions.toml")),
        "the receipt names the user rule: {ends:?}"
    );
    assert!(
        !dir.join(mock::PROJECT_RULE_FILE).exists(),
        "nothing was written"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_plan_mode_allow_exits_and_executes() {
    // Plan on + ExitPlanMode + Allow: a popup (containing the full plan) →
    // plan closes, the mode tier stays → the subsequent Write executes via
    // mode-based approval (read-only); the full plan is persisted
    // under .pigcode/plans/
    let (events, dir, agent) = run_trigger(
        ExecMode::ReadOnly,
        Some(ApprovalDecision::Allow),
        mock::SCENARIO_PLAN_EXIT_TRIGGER,
        "plan-exit-allow",
        None,
        true,
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        2,
        "ExitPlanMode + Write each prompt once: {details:?}"
    );
    assert_eq!(details[0].0, "ExitPlanMode");
    assert!(
        details[0].1.contains("Step 1"),
        "full plan in detail: {details:?}"
    );
    assert_eq!(
        details[1].0, "Write",
        "after the plan closes, Write goes through mode-based approval"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::PlanModeChanged { enabled, .. } if !enabled
        )),
        "UI receives the plan-disabled event"
    );
    assert!(
        mode_changes(&events).is_empty(),
        "exec mode unchanged throughout (orthogonal)"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(mock::PLAN_EXIT_FILE)).unwrap(),
        "executed\n",
        "Write actually executes under the mode"
    );
    // Plan persisted: .pigcode/plans/plan-<sid>.md contains the full plan
    let plans_dir = dir.join(".pigcode").join("plans");
    let plan_files: Vec<_> = std::fs::read_dir(&plans_dir)
        .expect("plans directory exists")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(plan_files.len(), 1, "exactly one plan file: {plan_files:?}");
    let plan_text = std::fs::read_to_string(plan_files[0].path()).unwrap();
    assert!(
        plan_text.contains("Step 1"),
        "plan file contains the full plan: {plan_text}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_plan_mode_reject_stays_plan() {
    let (events, dir, agent) = run_trigger(
        ExecMode::ReadOnly,
        Some(ApprovalDecision::Reject),
        mock::SCENARIO_PLAN_EXIT_TRIGGER,
        "plan-exit-reject",
        None,
        true,
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        1,
        "only one ExitPlanMode prompt: {details:?}"
    );
    assert!(
        tool_ends(&events)
            .iter()
            .any(|(_, out, err)| *err && out.contains("The user declined to exit plan mode")),
        "rejection receipt"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::PlanModeChanged { .. })),
        "plan toggle unchanged"
    );
    assert!(
        !dir.join(mock::PLAN_EXIT_FILE).exists(),
        "no Write after Reject"
    );
    // kimi semantics: the plan is persisted before the approval popup — after rejection the plan file is kept too (rewritten in place after revision)
    let plans_dir = dir.join(".pigcode").join("plans");
    assert!(
        std::fs::read_dir(&plans_dir).is_ok_and(|mut d| d.next().is_some()),
        "plan file should remain on disk after rejection"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_plan_mode_reject_with_feedback() {
    // kimi Revise: rejection carrying feedback → the feedback enters the model receipt, the plan toggle stays on
    let (config_path, dir, data_dir) = setup("plan-revise");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SetPlanMode {
            session_id: session_id.clone(),
            enabled: true,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{} start", mock::SCENARIO_PLAN_EXIT_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event {
            agent
                .ops
                .send(Op::ApprovalReply {
                    request_id: request_id.clone(),
                    decision: ApprovalDecision::Reject,
                    feedback: Some("step 2 is wrong, use option B instead".to_string()),
                })
                .await
                .unwrap();
        }
        let done = matches!(event, Event::TurnComplete { .. });
        collected.push(event);
        if done {
            break;
        }
    }
    let ends = tool_ends(&collected);
    assert!(
        ends.iter().any(|(_, out, err)| *err
            && out.contains("Feedback")
            && out.contains("step 2 is wrong, use option B instead")),
        "receipt should carry the feedback: {ends:?}"
    );
    assert!(
        !collected
            .iter()
            .any(|e| matches!(e, Event::PlanModeChanged { .. })),
        "plan toggle stays on after rejection"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_plan_mode_outside_plan_errors() {
    let (events, _dir, agent) = run_trigger(
        ExecMode::Auto,
        None,
        mock::SCENARIO_PLAN_EXIT_TRIGGER,
        "plan-exit-nonplan",
        None,
        false,
    )
    .await;
    assert!(
        approval_details(&events).is_empty(),
        "no prompt when plan mode is off"
    );
    assert!(
        tool_ends(&events)
            .iter()
            .any(|(_, out, err)| *err && out.contains("Only available in plan mode")),
        "errors when plan mode is off"
    );
    agent.shutdown();
}

// ---------- EnterPlanMode (paired with ExitPlanMode) ----------

fn mode_changes(events: &[Event]) -> Vec<ExecMode> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::PermissionsChanged { permissions, .. } => permissions.preset(),
            _ => None,
        })
        .collect()
}

fn plan_changes(events: &[Event]) -> Vec<bool> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::PlanModeChanged { enabled, .. } => Some(*enabled),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enter_plan_without_dialog_and_exit_keeps_mode() {
    // Auto → EnterPlanMode (no popup, plan on) → Write hard-rejected by
    // the plan → ExitPlanMode (popup Allow) → plan closes, the mode tier
    // stays Auto throughout → Write executes without approval
    let (events, dir, agent) = run_trigger(
        ExecMode::Auto,
        Some(ApprovalDecision::Allow),
        mock::SCENARIO_PLAN_ENTER_TRIGGER,
        "plan-enter",
        None,
        false,
    )
    .await;

    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        1,
        "only ExitPlanMode prompts (EnterPlanMode does not): {details:?}"
    );
    assert_eq!(details[0].0, "ExitPlanMode");

    assert_eq!(
        plan_changes(&events),
        [true, false],
        "EnterPlanMode on → ExitPlanMode off"
    );
    assert!(
        mode_changes(&events).is_empty(),
        "exec mode unchanged throughout (orthogonal, no restore)"
    );

    let ends = tool_ends(&events);
    assert!(
        ends.iter().any(|(id, out, err)| id.contains("call_pn_1")
            && !err
            && out.contains("Plan mode is on")),
        "EnterPlanMode succeeds: {ends:?}"
    );
    assert!(
        ends.iter()
            .any(|(id, out, err)| id.contains("call_pn_2") && *err && out.contains("Plan mode")),
        "Write hard-rejected while plan mode is on: {ends:?}"
    );
    assert!(
        ends.iter()
            .any(|(id, _, err)| id.contains("call_pn_4") && !err),
        "Write executes after the plan closes: {ends:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(mock::PLAN_ENTER_FILE)).unwrap(),
        "executed\n"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enter_plan_mode_idempotent_when_already_plan() {
    // EnterPlanMode while plan is already on: idempotent hint, no plan events; the subsequent ExitPlanMode still prompts
    let (events, _dir, agent) = run_trigger(
        ExecMode::Auto,
        Some(ApprovalDecision::Reject),
        mock::SCENARIO_PLAN_ENTER_TRIGGER,
        "plan-enter-idem",
        None,
        true,
    )
    .await;
    let ends = tool_ends(&events);
    assert!(
        ends.iter().any(|(id, out, err)| id.contains("call_pn_1")
            && !err
            && out.contains("Already in plan mode")),
        "idempotent hint: {ends:?}"
    );
    assert!(
        plan_changes(&events).is_empty(),
        "idempotent path emits no plan events"
    );
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        1,
        "only one ExitPlanMode prompt: {details:?}"
    );
    agent.shutdown();
}

// ---------- Plan file semantics (kimi writesOnlyPlanFile / ExitPlanMode reads from the file) ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_plan_mode_reads_plan_file_when_arg_empty() {
    // kimi semantics: ExitPlanMode without a plan argument → core reads the plan file into the popup
    let (config_path, dir, data_dir) = setup("plan-read-file");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;
    // Pre-persist the plan file (equivalent to the model writing it via Write)
    let plans_dir = dir.join(".pigcode/plans");
    std::fs::create_dir_all(&plans_dir).unwrap();
    std::fs::write(
        plans_dir.join(format!("plan-{session_id}.md")),
        "Plan from file: land step one",
    )
    .unwrap();
    agent
        .ops
        .send(Op::SetPlanMode {
            session_id: session_id.clone(),
            enabled: true,
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{} start", mock::SCENARIO_PLAN_FILE_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
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
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event {
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
        let done = matches!(event, Event::TurnComplete { .. });
        collected.push(event);
        if done {
            break;
        }
    }
    let details = approval_details(&collected);
    assert_eq!(
        details.len(),
        2,
        "ExitPlanMode + Write (ReadOnly) each prompt once: {details:?}"
    );
    assert_eq!(details[0].0, "ExitPlanMode");
    assert!(
        details[0].1.contains("Plan from file"),
        "detail should come from the plan file: {details:?}"
    );
    assert_eq!(
        details[1].0, "Write",
        "after approval, Write goes through mode-based approval"
    );
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::PlanModeChanged { enabled, .. } if !enabled)),
        "plan closes after approval"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(mock::PLAN_FILE_EXEC_FILE)).unwrap(),
        "executed\n",
        "Write executes after approval"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_plan_mode_without_plan_and_file_errors() {
    // No plan argument and no plan file: no approval popup; the receipt guides writing the plan file first
    let (events, _dir, agent) = run_trigger(
        ExecMode::ReadOnly,
        None,
        mock::SCENARIO_PLAN_FILE_TRIGGER,
        "plan-no-file",
        None,
        true,
    )
    .await;
    assert!(
        approval_details(&events).is_empty(),
        "no prompt without a plan or file"
    );
    assert!(
        tool_ends(&events)
            .iter()
            .any(|(_, out, err)| *err && out.contains("plan file is empty or missing")),
        "receipt should guide to write the plan file: {:?}",
        tool_ends(&events)
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_write_plan_dir_passthrough_others_denied() {
    // kimi writesOnlyPlanFile: writes into the plans directory pass through approval-free; writes to normal files are still hard-rejected by the plan
    let (events, dir, agent) = run_trigger(
        ExecMode::ReadOnly,
        None,
        mock::SCENARIO_PLAN_WRITE_GATE_TRIGGER,
        "plan-write-gate",
        None,
        true,
    )
    .await;
    assert!(
        approval_details(&events).is_empty(),
        "plan dir writes pass through without approval (no prompts at all)"
    );
    let ends = tool_ends(&events);
    assert!(
        ends.iter()
            .any(|(id, _, err)| id.contains("call_pg_1") && !err),
        "plan dir write should pass through: {ends:?}"
    );
    assert!(
        dir.join(".pigcode/plans/plan-gate.md").exists(),
        "plan file actually written"
    );
    assert!(
        ends.iter()
            .any(|(id, out, err)| id.contains("call_pg_2") && *err && out.contains("Plan mode")),
        "normal file should be hard-rejected by plan mode: {ends:?}"
    );
    assert!(!dir.join("other.txt").exists(), "normal file not created");
    agent.shutdown();
}

/// Approval replies are strictly per-request: one decision wakes exactly the
/// request it names, never a same-command waiter, a different workspace's
/// session, or a different requested root. Cross-session approval leaks are the
/// security regression this test pins down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_reply_resolves_only_the_exact_request() {
    use pig_core::session::{PendingApprovals, reject_session_approvals, resolve_approval};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
    let park = |id: &str| {
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(id.to_string(), tx);
        rx
    };
    // Same command, different roots; different command; another session.
    let mut session_a = park("s1-turn-1-approval-call_a");
    let mut session_a_other_root = park("s1-turn-1-approval-call_b");
    let mut other_command = park("s1-turn-1-approval-call_c");
    let mut session_b = park("s2-turn-1-approval-call_a");

    resolve_approval(
        &pending,
        "s1-turn-1-approval-call_a",
        ApprovalDecision::Allow,
        None,
    );

    assert_eq!(session_a.try_recv(), Ok((ApprovalDecision::Allow, None)));
    // Identical tool/command but another requested root (or another session):
    // still parked. The reply carried no fan-out key, so nothing can match.
    assert!(session_a_other_root.try_recv().is_err());
    assert!(other_command.try_recv().is_err());
    assert!(session_b.try_recv().is_err());
    assert_eq!(pending.lock().unwrap().len(), 3);

    // A nonexistent request_id (late/duplicate UI reply) is a no-op.
    resolve_approval(&pending, "req-missing", ApprovalDecision::Reject, None);
    assert_eq!(pending.lock().unwrap().len(), 3);

    // Session deletion rejects only its own requests.
    reject_session_approvals(&pending, "s1");
    assert_eq!(
        session_a_other_root.try_recv(),
        Ok((ApprovalDecision::Reject, None))
    );
    assert_eq!(
        other_command.try_recv(),
        Ok((ApprovalDecision::Reject, None))
    );
    assert!(
        session_b.try_recv().is_err(),
        "another session is untouched"
    );
    assert_eq!(pending.lock().unwrap().len(), 1);
}

/// Two sessions in the same workspace await the same Write concurrently. A
/// decision for session A must not approve session B's identical action (the
/// old coalescing key ignored the session and the requested roots).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approvals_do_not_cross_sessions() {
    let (config_path, dir, data_dir) = setup("approval-cross-session");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let session_a = new_session(&agent, dir.clone()).await;
    let session_b = new_session(&agent, dir.clone()).await;
    for session_id in [&session_a, &session_b] {
        agent
            .ops
            .send(Op::SetPermissions {
                session_id: session_id.clone(),
                permissions: ExecMode::ReadOnly.permissions(),
            })
            .await
            .unwrap();
        agent
            .ops
            .send(Op::SendMessage {
                session_id: session_id.clone(),
                content: format!("{} cross-session", mock::SCENARIO_B_TRIGGER),
                files: vec![],
                images: vec![],
                permissions: ExecMode::ReadOnly.permissions(),
                delivery: pig_protocol::MessageDelivery::Queue,
            })
            .await
            .unwrap();
    }

    let mut approval_a: Option<String> = None;
    let mut approval_b: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while approval_a.is_none() || approval_b.is_none() {
        assert!(
            Instant::now() < deadline,
            "both sessions should ask for approval"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event {
            if request_id.starts_with(&format!("{session_a}-")) {
                approval_a.get_or_insert_with(|| request_id.clone());
            } else if request_id.starts_with(&format!("{session_b}-")) {
                approval_b.get_or_insert_with(|| request_id.clone());
            }
        }
    }
    let approval_a = approval_a.expect("A approval");
    let approval_b = approval_b.expect("B approval");

    // Answer only A's request.
    agent
        .ops
        .send(Op::ApprovalReply {
            request_id: approval_a,
            decision: ApprovalDecision::Allow,
            feedback: None,
        })
        .await
        .unwrap();

    // A advances to its Edit approval (proving the Write was authorized);
    // B must not have resolved its own Write in the meantime.
    let mut a_advanced = false;
    let mut b_write_settled = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut answered_a_edit = false;
    while !a_advanced {
        assert!(Instant::now() < deadline, "A did not advance");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::ApprovalRequested {
                request_id, tool, ..
            } if tool == "Edit" && request_id.starts_with(&format!("{session_a}-")) => {
                a_advanced = true;
                if !answered_a_edit {
                    answered_a_edit = true;
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
            }
            Event::ToolCallEnd { session_id, .. } if session_id == &session_b => {
                b_write_settled = true;
            }
            _ => {}
        }
    }
    assert!(
        !b_write_settled,
        "B's identical Write must stay pending after A's approval"
    );

    // Unblock B; then auto-approve everything until both turns end.
    agent
        .ops
        .send(Op::ApprovalReply {
            request_id: approval_b,
            decision: ApprovalDecision::Allow,
            feedback: None,
        })
        .await
        .unwrap();
    let mut completed = 0u32;
    let deadline = Instant::now() + Duration::from_secs(30);
    while completed < 2 {
        assert!(Instant::now() < deadline, "both turns should complete");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested { request_id, .. } = &event {
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
        if matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. }
        ) {
            completed += 1;
        }
    }
    assert_eq!(
        std::fs::read_to_string(dir.join(mock::SCENARIO_B_FILE)).unwrap(),
        mock::SCENARIO_B_CONTENT.replace("line2", "LINE2"),
        "both sessions ran their Write and Edit legs"
    );
    agent.shutdown();
}

/// A plan toggle issued while a turn is running is durable and surfaced as
/// deferred, then applied at the next turn start. A nonfatal operation
/// rejection (revert while running) must not tear the turn down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_plan_toggle_is_deferred_and_applied_next_turn() {
    let (config_path, dir, data_dir) = setup("running-plan");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let events = agent.events.clone();
    let sid = new_session(&agent, dir.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: mock::SCENARIO_SLOW_TRIGGER.to_string(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    recv_until(
        &events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;

    // A nonfatal operation rejection must leave the running turn alone.
    agent
        .ops
        .send(Op::RevertFile {
            session_id: sid.clone(),
            path: "README.mock.md".into(),
        })
        .await
        .unwrap();
    let received = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::Error { .. })
    })
    .await;
    assert!(
        received.iter().any(|e| matches!(
            e,
            Event::Error {
                error,
                fatal: false,
                ..
            } if matches!(error.semantic_error(), pig_protocol::CoreError::RevertTurnInFlight)
        )),
        "a rejected operation is typed nonfatal: {received:#?}"
    );
    assert!(
        !received
            .iter()
            .any(|e| matches!(e, Event::TurnAborted { .. })),
        "the rejected operation must not abort the turn: {received:#?}"
    );

    agent
        .ops
        .send(Op::SetPlanMode {
            session_id: sid.clone(),
            enabled: true,
        })
        .await
        .unwrap();
    let received = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::PlanModeChanged { .. })
    })
    .await;
    assert!(
        received.iter().any(|e| matches!(
            e,
            Event::PlanModeChanged {
                enabled: true,
                deferred: true,
                ..
            }
        )),
        "the running toggle is surfaced as deferred: {received:#?}"
    );

    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(15), |e| {
        matches!(e, Event::TurnAborted { .. })
    })
    .await;

    // The next turn runs under plan mode: modifying tools are hard-rejected
    // and the deferred switch is confirmed as applied. SCENARIO_DANGER is
    // checked before the earlier SLOW trigger in the mock, so it still produces
    // a shell (non-read-only) tool call.
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} plan", mock::SCENARIO_DANGER_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let mut approvals = 0u32;
    let mut applied = false;
    let mut plan_rejections = 0u32;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "plan turn timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::ApprovalRequested { .. } => approvals += 1,
            Event::PlanModeChanged {
                enabled: true,
                deferred: false,
                ..
            } => applied = true,
            Event::ToolCallEnd {
                output, is_error, ..
            } if *is_error && output.contains("Plan mode") => plan_rejections += 1,
            Event::TurnComplete { .. } | Event::TurnAborted { .. } => break,
            _ => {}
        }
    }
    assert!(applied, "the next turn start confirmed the durable toggle");
    assert_eq!(approvals, 0, "plan mode rejects before any dialog");
    assert!(
        plan_rejections >= 1,
        "modifying tools are hard-rejected under the deferred plan mode"
    );
    assert!(
        !dir.join(mock::SCENARIO_B_FILE).exists(),
        "plan mode must not write the file"
    );
    agent.shutdown();
}

/// Codex's `require_escalated`: one approval, then the command runs with a
/// root write while the seeded deny groups stay enforced
/// (EscalatedSandboxWithRestrictions).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_escalated_prompts_and_runs_with_restrictions() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tmp")
        .join(format!("pig-require-escalated-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let target = root.join("report.txt");
    let (events, _dir, agent) = run_trigger(
        ExecMode::Auto,
        Some(ApprovalDecision::Allow),
        &format!(
            "{} {}",
            mock::SCENARIO_REQUIRE_ESCALATED_TRIGGER,
            target.display()
        ),
        "require-escalated",
        None,
        false,
    )
    .await;

    let details = approval_details(&events);
    assert_eq!(details.len(), 1, "{details:?}");
    assert_eq!(details[0].0, "ExecCommand");
    assert!(
        details[0].1.contains("broad filesystem access"),
        "with deny groups active the dialog must say the sandbox stays on: {:?}",
        details[0].1
    );
    assert!(
        details[0]
            .1
            .contains("writes a report outside the workspace")
    );
    if assert_shell_write(&events, "call_require_escalated", &target) {
        assert_eq!(
            std::fs::read_to_string(&target).unwrap().trim(),
            "escalated"
        );
    }
    agent.shutdown();
}
