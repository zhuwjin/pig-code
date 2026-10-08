mod common;

use common::{new_session, setup};
use pig_protocol::{ApprovalDecision, Event, ExecMode, Op};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// An outside-workspace sibling directory with one readable file; forward
/// slashes in the returned message path keep the raw mock request body
/// escape-free
fn outside_target(name: &str) -> (PathBuf, String) {
    // The workspace sits under the system tmp (as everywhere in the test
    // suite); the "outside" dir must escape BOTH the workspace and tmp
    // itself — tmp is a permanent exemption, so a sibling under %TEMP% would
    // never pop. Its parent (AppData/Local) is outside every exemption.
    let marker = format!("pig-core-fsout-{name}-{}", std::process::id());
    let ws = std::env::temp_dir().join(format!("ws-{marker}"));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).unwrap();
    let outside = std::env::temp_dir()
        .parent()
        .unwrap()
        .join(format!("outside-{marker}"));
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&outside).unwrap();
    let file = outside.join("target.txt");
    std::fs::write(&file, "OUTSIDE_MARKER_42\n").unwrap();
    (ws, file.display().to_string().replace('\\', "/"))
}

struct Run {
    events: Vec<Event>,
    approvals: Vec<String>,
}

/// One turn with the given user message; ApprovalRequested events are answered
/// with `decision` and collected (their detail text) for assertions
async fn run_turn(
    agent: &pig_core::AgentHandle,
    session_id: &str,
    message: &str,
    mode: ExecMode,
    decision: ApprovalDecision,
) -> Run {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: message.to_string(),
            files: vec![],
            images: vec![],
            mode,
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    let mut approvals = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out: {events:#?}");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        if let Event::ApprovalRequested {
            request_id, detail, ..
        } = &event
        {
            approvals.push(detail.clone());
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
        events.push(event);
        if done {
            break;
        }
    }
    Run { events, approvals }
}

fn tool_outputs(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCallEnd { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

/// Allow = this one call: the read runs, and the next turn's outside read pops
/// again
#[tokio::test]
async fn outside_read_allow_is_per_call() {
    let (config_path, dir, data_dir) = setup("fsout-allow");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let session_id = new_session(&agent, dir.clone()).await;

    let msg = format!("{} {{path}}", pig_core::mock::SCENARIO_OUTSIDE_READ_TRIGGER);
    // Replace {path} with the real outside file (created relative to a fresh ws)
    let (ws, outside) = outside_target("allow");
    let _ = ws;
    let msg = msg.replace("{path}", &outside);

    let first = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::AutoEdit,
        ApprovalDecision::Allow,
    )
    .await;
    assert_eq!(
        first.approvals.len(),
        1,
        "one popup expected, got {:?}",
        first.approvals
    );
    assert!(
        first.approvals[0].starts_with("Read outside the workspace:"),
        "detail should lead with the fs line: {}",
        first.approvals[0]
    );
    let outs = tool_outputs(&first.events);
    assert!(
        outs.iter().any(|o| o.contains("OUTSIDE_MARKER_42")),
        "approved read should return the file content, got {outs:?}"
    );

    let second = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::AutoEdit,
        ApprovalDecision::Allow,
    )
    .await;
    assert_eq!(
        second.approvals.len(),
        1,
        "Allow is per-call: the second outside read must pop again"
    );
    let outs = tool_outputs(&second.events);
    assert!(
        outs.iter()
            .all(|o| !o.contains("Path escapes") && !o.contains("declined")),
        "the approved second-turn read must not hit the boundary error: {outs:?}"
    );
}

/// AlwaysAllow covers the same access kind for the rest of the session
#[tokio::test]
async fn outside_read_always_allow_covers_session() {
    let (config_path, dir, data_dir) = setup("fsout-always");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let session_id = new_session(&agent, dir.clone()).await;

    let (ws, outside) = outside_target("always");
    let _ = ws;
    let msg = format!(
        "{} {outside}",
        pig_core::mock::SCENARIO_OUTSIDE_READ_TRIGGER
    );

    let first = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::AutoEdit,
        ApprovalDecision::AlwaysAllow,
    )
    .await;
    assert_eq!(first.approvals.len(), 1);
    // "Always this session" flips the session toggle itself and tells the UI:
    // FsAccessChanged must arrive with read_outside on (the composer checks
    // its mode-menu box from this event)
    assert!(
        first.events.iter().any(|e| matches!(
            e,
            Event::FsAccessChanged {
                read_outside: true,
                ..
            }
        )),
        "FsAccessChanged(read_outside=true) expected after AlwaysAllow"
    );
    let second = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::AutoEdit,
        ApprovalDecision::Reject,
    )
    .await;
    assert!(
        second.approvals.is_empty(),
        "AlwaysAllow must suppress further same-kind popups this session"
    );
    let outs = tool_outputs(&second.events);
    assert!(
        outs.iter()
            .all(|o| !o.contains("Path escapes") && !o.contains("declined")),
        "the popup-free read must not hit the boundary error: {outs:?}"
    );
}

/// Reject returns the boundary note as the tool error
#[tokio::test]
async fn outside_read_reject_notes_boundary() {
    let (config_path, dir, data_dir) = setup("fsout-reject");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let session_id = new_session(&agent, dir.clone()).await;

    let (ws, outside) = outside_target("reject");
    let _ = ws;
    let msg = format!(
        "{} {outside}",
        pig_core::mock::SCENARIO_OUTSIDE_READ_TRIGGER
    );

    let run = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::AutoEdit,
        ApprovalDecision::Reject,
    )
    .await;
    assert_eq!(run.approvals.len(), 1);
    let outs = tool_outputs(&run.events);
    assert!(
        outs.iter()
            .any(|o| o.contains("declined read outside the workspace")),
        "reject note expected, got {outs:?}"
    );
}

/// A toggle flip issued WHILE the turn is running (exactly what the composer
/// does via Op::SetFsAccess) takes effect on the very next tool call: the
/// first outside read pops an approval; with the request still pending the
/// harness flips read-on; even after REJECTING that first request, the
/// second outside read in the same turn runs popup-free through the new
/// toggle state
#[tokio::test]
async fn midturn_toggle_flip_takes_effect_immediately() {
    let (config_path, dir, data_dir) = setup("fsout-midturn");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let session_id = new_session(&agent, dir.clone()).await;

    let (ws, outside) = outside_target("midturn");
    let _ = ws;
    let msg = format!(
        "{} {outside}",
        pig_core::mock::SCENARIO_OUTSIDE_READ2_TRIGGER
    );

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: msg,
            files: vec![],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();

    let mut approvals = 0usize;
    let mut second_round_output: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::ApprovalRequested {
                request_id, detail, ..
            } => {
                approvals += 1;
                // Mid-turn, while the approval is pending: flip the session
                // toggle read-on (the composer's exact Op path), then REJECT
                // the pending first request — harsh case
                agent
                    .ops
                    .send(Op::SetFsAccess {
                        session_id: session_id.clone(),
                        read_outside: true,
                        write_outside: false,
                    })
                    .await
                    .unwrap();
                assert!(detail.starts_with("Read outside the workspace:"));
                agent
                    .ops
                    .send(Op::ApprovalReply {
                        request_id: request_id.clone(),
                        decision: ApprovalDecision::Reject,
                        feedback: None,
                    })
                    .await
                    .unwrap();
            }
            Event::ToolCallEnd {
                output, is_error, ..
            } => {
                if approvals >= 1 && second_round_output.is_none() && !*is_error {
                    second_round_output = Some(output.clone());
                }
                // Any round's error output before the flip lands is the
                // expected reject note; AFTER the flip there must be none
                if approvals >= 1 && *is_error {
                    assert!(
                        output.contains("declined read outside the workspace"),
                        "only the rejected first call may error: {output}"
                    );
                }
            }
            Event::TurnComplete { .. } | Event::TurnAborted { .. } => break,
            _ => {}
        }
    }
    assert_eq!(approvals, 1, "the second outside read must NOT pop again");
    let out = second_round_output.expect("second-round read output");
    assert!(
        !out.contains("Path escapes") && !out.contains("declined"),
        "the post-flip read runs on the new toggle state: {out}"
    );
}

/// The fs trigger is independent of the mode's approval tiers: a Write outside
/// pops even under FullAccess (writes are otherwise approval-free there)
#[tokio::test]
async fn outside_write_pops_under_full_access() {
    let (config_path, dir, data_dir) = setup("fsout-write");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let session_id = new_session(&agent, dir.clone()).await;

    let (ws, outside_file) = outside_target("write");
    let _ = ws;
    let outside_dir = outside_file.rsplit_once('/').unwrap().0.to_string();
    let file = format!("{outside_dir}/written.txt");
    let msg = format!("{} {file}", pig_core::mock::SCENARIO_OUTSIDE_WRITE_TRIGGER);

    let run = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::FullAccess,
        ApprovalDecision::Allow,
    )
    .await;
    assert_eq!(
        run.approvals.len(),
        1,
        "FullAccess must still pop for outside writes"
    );
    assert!(run.approvals[0].starts_with("Write outside the workspace:"));
    let written = Path::new(&file.replace('/', "\\")).exists() || Path::new(&file).exists();
    assert!(
        written,
        "approved outside write should create the file: {} | outputs: {:?}",
        file,
        tool_outputs(&run.events)
    );
}

/// Yolo pops too — leaving the workspace is a scope change worth one click
#[tokio::test]
async fn outside_read_pops_under_yolo() {
    let (config_path, dir, data_dir) = setup("fsout-yolo");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let session_id = new_session(&agent, dir.clone()).await;

    let (ws, outside) = outside_target("yolo");
    let _ = ws;
    let msg = format!(
        "{} {outside}",
        pig_core::mock::SCENARIO_OUTSIDE_READ_TRIGGER
    );

    let run = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::Yolo,
        ApprovalDecision::Allow,
    )
    .await;
    assert_eq!(
        run.approvals.len(),
        1,
        "Yolo must still pop for outside reads"
    );
    assert!(
        tool_outputs(&run.events)
            .iter()
            .any(|o| o.contains("OUTSIDE_MARKER_42"))
    );
}
