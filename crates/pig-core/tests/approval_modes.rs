mod common;

use common::{new_session, setup};
use pig_protocol::{ApprovalDecision, Event, ExecMode, Op};
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
        .send(Op::SetExecMode {
            session_id: session_id.clone(),
            mode,
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
        false,
    )
    .await;

    assert_eq!(
        approvals(&events),
        ["Write", "Edit", "Bash"],
        "three approvals appear in order"
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

    let ends = tool_ends(&events);
    assert!(
        ends.iter().any(|(id, out, err)| id.contains("call_b_bash")
            && out.contains(mock::SCENARIO_B_BASH_MARKER)
            && !err),
        "Bash output contains the marker: {ends:?}"
    );

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
        ExecMode::ConfirmBeforeEdit,
        Some(ApprovalDecision::Reject),
        "deny",
        false,
    )
    .await;

    assert_eq!(approvals(&events), ["Write", "Edit", "Bash"]);
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
        3,
        "all three tools rejected: {:?}",
        tool_ends(&events)
    );
    assert!(file_changes(&events).is_empty(), "no file changes");
    assert!(
        !dir.join(mock::SCENARIO_B_FILE).exists(),
        "file not created"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_edit_only_bash_needs_approval() {
    let (events, dir, agent, _) = run_scenario_b(
        ExecMode::AutoEdit,
        Some(ApprovalDecision::Allow),
        "auto",
        false,
    )
    .await;

    assert_eq!(
        approvals(&events),
        ["Bash"],
        "AutoEdit: only Bash needs approval"
    );
    assert!(dir.join(mock::SCENARIO_B_FILE).exists());
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_blocks_writes_without_approval() {
    // Plan on + confirm before edit: mutating tools are hard-rejected by the plan; not even an approval card pops up
    let (events, dir, agent, _) =
        run_scenario_b(ExecMode::ConfirmBeforeEdit, None, "plan", true).await;

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
async fn plan_mode_overrides_full_access() {
    // "Full access + plan": plan overrides full access — writes are still hard-rejected (same semantics as ZCode)
    let (events, dir, agent, _) =
        run_scenario_b(ExecMode::FullAccess, None, "plan-full", true).await;

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
async fn full_access_no_approvals() {
    let (events, dir, agent, _) = run_scenario_b(ExecMode::FullAccess, None, "full", false).await;

    assert!(
        approvals(&events).is_empty(),
        "no approval prompts under full access"
    );
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
            content: format!("{} run the modification chain", mock::SCENARIO_B_TRIGGER),
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
            content: format!("{} run a dangerous command", mock::SCENARIO_DANGER_TRIGGER),
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
async fn danger_full_access_allow_executes() {
    // FullAccess normally does not ask about Bash, but dangerous commands must
    // prompt; the mock's mkfs hits the blocklist yet is harmless to run
    // (command missing → exit 127 / no args → just prints usage) — a non-zero
    // exit is still enough to prove the execution path was taken
    let (events, _dir, agent) = run_danger(
        ExecMode::FullAccess,
        ApprovalDecision::Allow,
        "danger-allow",
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        2,
        "both dangerous commands should prompt: {details:?}"
    );
    assert!(
        details.iter().all(|(tool, d, key)| *tool == "Bash"
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
async fn danger_confirm_before_edit_danger_key() {
    let (events, _dir, agent) = run_danger(
        ExecMode::ConfirmBeforeEdit,
        ApprovalDecision::Allow,
        "danger-cbe",
    )
    .await;
    let details = approval_details(&events);
    assert!(
        !details.is_empty(),
        "Bash already requires approval under ConfirmBeforeEdit"
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
        ExecMode::FullAccess,
        ApprovalDecision::AlwaysAllow,
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

// ---------- Yolo (unrestricted full-auto) ----------

/// "Yolo" string serde round trip (the sessions table stores/loads by variant name)
#[test]
fn exec_mode_yolo_serde_roundtrip() {
    let json = serde_json::to_string(&ExecMode::Yolo).unwrap();
    assert_eq!(json, "\"Yolo\"");
    let back: ExecMode = serde_json::from_str("\"Yolo\"").unwrap();
    assert_eq!(back, ExecMode::Yolo);
    // An unknown variant name falls back to the default (same rule as store.rs mode_from_row)
    let fallback: ExecMode = serde_json::from_str("\"NotAMode\"").unwrap_or_default();
    assert_eq!(fallback, ExecMode::ConfirmBeforeEdit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_danger_no_dialog_executes() {
    // Yolo: dangerous commands do not prompt either, they execute directly (mkfs hits the blocklist but is harmless; see the mock comments)
    let (events, _dir, agent) =
        run_danger(ExecMode::Yolo, ApprovalDecision::Allow, "yolo-danger").await;
    assert!(
        approval_details(&events).is_empty(),
        "no approval prompts under Yolo"
    );
    let ends = tool_ends(&events);
    assert_eq!(
        ends.iter()
            .filter(|(_, out, err)| !err && out.contains("[exit code:"))
            .count(),
        2,
        "both dangerous commands execute directly: {ends:?}"
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
async fn yolo_normal_flow_no_dialogs() {
    let (events, dir, agent, _session_id) =
        run_scenario_b(ExecMode::Yolo, None, "yolo-b", false).await;
    assert!(
        approvals(&events).is_empty(),
        "Write/Edit/Bash never prompt under Yolo"
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
async fn yolo_sensitive_file_still_blocked() {
    // Sensitive-file protection lives at the tool execute layer, independent of
    // mode (unconditionally effective under Yolo too): tool::execute is
    // mode-unaware; directly verify that reading .env is still denied
    let dir = std::env::temp_dir().join(format!("pig-core-yolo-env-{}", std::process::id()));
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
        },
    )
    .await;
    assert!(is_error, "{out}");
    assert!(
        out.contains("sensitive file"),
        ".env still unreadable under Yolo: {out}"
    );
    assert!(!out.contains("SECRET=1"), "content must not leak: {out}");
}

// ---------- AlwaysAllow subject granularity / AutoEdit read-only pass-through / TaskStop exemption ----------

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
    // prompt) → Bash ls (different first word, prompt)
    let (events, dir, agent) = run_trigger(
        ExecMode::ConfirmBeforeEdit,
        Some(ApprovalDecision::AlwaysAllow),
        mock::SCENARIO_SUBJECT_TRIGGER,
        "subject",
        None,
        false,
    )
    .await;
    let details = approval_details(&events);
    assert_eq!(
        details.len(),
        4,
        "first a/first b/first echo/first ls each prompt once; second same-subject skips: {details:?}"
    );
    assert!(details[0].1.contains(mock::SUBJECT_FILE_A), "{details:?}");
    assert!(details[1].1.contains(mock::SUBJECT_FILE_B), "{details:?}");
    assert!(details[2].1.contains("echo SUBJ_3"), "{details:?}");
    assert_eq!(details[3].1.trim(), "ls", "{details:?}");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_edit_readonly_bash_passthrough() {
    // AutoEdit + allowlisted read-only command (ls): executes directly without a prompt (decision=None; a popup would hang until timeout)
    let (events, _dir, agent) = run_trigger(
        ExecMode::AutoEdit,
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
    let ends = tool_ends(&events);
    assert!(
        ends.iter()
            .any(|(_, out, err)| !err && out.contains("[exit code: 0]")),
        "ls actually executes: {ends:?}"
    );
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
        "first word after leading whitespace"
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
        !pig_core::tool::requires_approval(stop.as_ref(), ExecMode::ConfirmBeforeEdit),
        "exempt even under ConfirmBeforeEdit"
    );
    assert!(
        !pig_core::tool::requires_approval(stop.as_ref(), ExecMode::AutoEdit),
        "read-only also exempt under AutoEdit"
    );
}

// ---------- Project-level permission rules (priority chain) / ExitPlanMode ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_deny_hard_rejects_even_in_full_access() {
    // FullAccess + deny Bash(mkfs*): not even the danger popup is sent; the project rule hard-rejects directly
    let (events, _dir, agent) = run_trigger(
        ExecMode::FullAccess,
        Some(ApprovalDecision::Allow),
        mock::SCENARIO_DANGER_TRIGGER,
        "perm-deny",
        Some("deny = [\"Bash(mkfs*)\"]\n"),
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
            && out.contains("Bash(mkfs*)")),
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
    // Under ConfirmBeforeEdit, ls would normally prompt; an allow hit → executes directly without a prompt
    let (events, _dir, agent) = run_trigger(
        ExecMode::ConfirmBeforeEdit,
        None,
        mock::SCENARIO_READONLY_TRIGGER,
        "perm-allow",
        Some("allow = [\"Bash(ls)\"]\n"),
        false,
    )
    .await;
    assert!(approval_details(&events).is_empty(), "allow skips approval");
    assert!(
        tool_ends(&events)
            .iter()
            .any(|(_, out, err)| !err && out.contains("[exit code: 0]")),
        "ls actually executes"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permissions_allow_does_not_exempt_danger() {
    // An allow hit on a dangerous command still prompts (the danger check runs before allow)
    let (events, _dir, agent) = run_trigger(
        ExecMode::ConfirmBeforeEdit,
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
async fn exit_plan_mode_allow_exits_and_executes() {
    // Plan on + ExitPlanMode + Allow: a popup (containing the full plan) →
    // plan closes, the mode tier stays → the subsequent Write executes via
    // mode-based approval (confirm before edit); the full plan is persisted
    // under .pigcode/plans/
    let (events, dir, agent) = run_trigger(
        ExecMode::ConfirmBeforeEdit,
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
        ExecMode::ConfirmBeforeEdit,
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
            mode: ExecMode::ConfirmBeforeEdit,
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
        ExecMode::AutoEdit,
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
            Event::ExecModeChanged { mode, .. } => Some(*mode),
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
    // AutoEdit → EnterPlanMode (no popup, plan on) → Write hard-rejected by
    // the plan → ExitPlanMode (popup Allow) → plan closes, the mode tier
    // stays AutoEdit throughout → Write executes without approval
    let (events, dir, agent) = run_trigger(
        ExecMode::AutoEdit,
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
        ExecMode::AutoEdit,
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
            mode: ExecMode::ConfirmBeforeEdit,
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
        "ExitPlanMode + Write (ConfirmBeforeEdit) each prompt once: {details:?}"
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
        ExecMode::ConfirmBeforeEdit,
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
        ExecMode::ConfirmBeforeEdit,
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

/// Concurrent approvals sharing a coalesce key are all woken by one decision
/// (regression for Swarm subagents awaiting approval on the same command
/// concurrently): same-key waiters are released together by one Allow;
/// different commands / different danger bit / no key (plan confirmation) are
/// unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approval_reply_fans_out_same_coalesce_key() {
    use pig_core::session::{PendingApprovals, resolve_approval};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
    let park = |pending: &PendingApprovals, id: &str, key: Option<(String, String, bool)>| {
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(id.to_string(), (tx, key));
        rx
    };
    let mut sleep_a = park(
        &pending,
        "req-sleep-a",
        Some(("Bash".into(), "sleep 5".into(), false)),
    );
    let mut sleep_b = park(
        &pending,
        "req-sleep-b",
        Some(("Bash".into(), "sleep 5".into(), false)),
    );
    let mut echo = park(
        &pending,
        "req-echo",
        Some(("Bash".into(), "echo hi".into(), false)),
    );
    let mut danger = park(
        &pending,
        "req-sleep-danger",
        Some(("Bash".into(), "sleep 5".into(), true)),
    );
    let mut plan = park(&pending, "req-plan", None);

    // Answer only one of the sleep 5 requests: the other same-key waiter should be woken together
    resolve_approval(&pending, "req-sleep-a", ApprovalDecision::Allow, None);

    assert_eq!(sleep_a.try_recv(), Ok((ApprovalDecision::Allow, None)));
    assert_eq!(sleep_b.try_recv(), Ok((ApprovalDecision::Allow, None)));
    // Different command, same command with a different danger bit, plan confirmation: none are affected; they stay parked in the waiting table
    assert!(echo.try_recv().is_err());
    assert!(danger.try_recv().is_err());
    assert!(plan.try_recv().is_err());
    {
        let left = pending.lock().unwrap();
        assert_eq!(
            left.len(),
            3,
            "only the three different-key/no-key entries remain: {left:?}"
        );
        assert!(left.contains_key("req-echo"));
        assert!(left.contains_key("req-sleep-danger"));
        assert!(left.contains_key("req-plan"));
    }

    // A nonexistent request_id (late/duplicate UI reply): a no-op that does not panic
    resolve_approval(&pending, "req-missing", ApprovalDecision::Reject, None);
    assert_eq!(pending.lock().unwrap().len(), 3);
}
