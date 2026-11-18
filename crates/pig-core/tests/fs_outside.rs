use crate::common;

use common::{new_session, outside_dir, setup};
use pig_protocol::{ApprovalDecision, Event, ExecMode, Op};
use std::path::{Path, PathBuf};

/// An isolated, writable directory outside the workspace and system tmp.
/// Forward slashes keep the raw mock request body escape-free.
fn outside_target(name: &str) -> (PathBuf, String) {
    // The workspace sits under the system tmp (as everywhere in the test
    // suite); the "outside" dir must escape BOTH the workspace and tmp
    // itself — tmp is a permanent exemption, so a sibling under %TEMP% would
    // never pop. Its parent (AppData/Local) is outside every exemption.
    let outside = outside_dir(name);
    std::fs::create_dir_all(&outside).unwrap();
    let file = outside.join("target.txt");
    std::fs::write(&file, "OUTSIDE_MARKER_42\n").unwrap();
    (outside, file.display().to_string().replace('\\', "/"))
}

struct Run {
    events: Vec<Event>,
    approvals: Vec<String>,
    /// Tiers offered by each approval dialog, in the same order.
    decisions: Vec<Vec<ApprovalDecision>>,
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
            permissions: mode.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    let mut approvals = Vec::new();
    let mut decisions = Vec::new();
    loop {
        let Ok(Ok(event)) =
            tokio::time::timeout(std::time::Duration::from_secs(30), agent.events.recv()).await
        else {
            panic!("timed out waiting for the turn; approvals so far: {approvals:?}");
        };
        match &event {
            Event::ApprovalRequested {
                request_id,
                detail,
                available_decisions,
                ..
            } => {
                approvals.push(detail.clone());
                decisions.push(available_decisions.clone());
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
            Event::TurnComplete { .. } | Event::TurnAborted { .. } => {
                events.push(event);
                break;
            }
            _ => {}
        }
        events.push(event);
    }
    Run {
        events,
        approvals,
        decisions,
    }
}

fn tool_outputs(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCallEnd { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

/// Reads outside the workspace are not gated: the sandbox grants full-disk
/// read (Codex's `workspace-write` semantics), so no approval pops and the
/// file content comes back on the first turn — in every exec mode.
#[tokio::test]
async fn outside_read_needs_no_approval() {
    for mode in [ExecMode::ReadOnly, ExecMode::Auto, ExecMode::Full] {
        let (config_path, dir, data_dir) = setup("fsout-read");
        let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
        let session_id = new_session(&agent, dir.clone()).await;

        let (outside_root, outside) = outside_target("read");
        let msg = format!(
            "{} {outside}",
            pig_provider::mock::SCENARIO_OUTSIDE_READ_TRIGGER
        );

        let run = run_turn(&agent, &session_id, &msg, mode, ApprovalDecision::Reject).await;
        assert!(
            run.approvals.is_empty(),
            "outside reads must not pop an approval under {mode:?}: {:?}",
            run.approvals
        );
        let outs = tool_outputs(&run.events);
        assert!(
            outs.iter().any(|o| o.contains("OUTSIDE_MARKER_42")),
            "the outside read returns content under {mode:?}, got {outs:?}"
        );
        assert!(
            outs.iter()
                .all(|o| !o.contains("Path escapes") && !o.contains("declined")),
            "no boundary error for reads under {mode:?}: {outs:?}"
        );
        agent.shutdown();
        let _ = std::fs::remove_dir_all(&outside_root);
    }
}

/// Writes outside the workspace stay gated per call: Allow writes the file,
/// and the next call asks again (there is no persistent switch).
#[tokio::test]
async fn outside_write_pops_per_call() {
    let (config_path, dir, data_dir) = setup("fsout-write");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let session_id = new_session(&agent, dir.clone()).await;

    let (outside_root, outside_file) = outside_target("write");
    let outside_dir = outside_file.rsplit_once('/').unwrap().0.to_string();
    let file = format!("{outside_dir}/written.txt");
    let msg = format!(
        "{} {file}",
        pig_provider::mock::SCENARIO_OUTSIDE_WRITE_TRIGGER
    );

    let first = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::Auto,
        ApprovalDecision::Allow,
    )
    .await;
    assert_eq!(
        first.approvals.len(),
        1,
        "Auto must still pop for outside writes"
    );
    assert!(first.approvals[0].starts_with("Write outside the workspace:"));
    assert_eq!(
        first.decisions[0],
        vec![ApprovalDecision::Allow, ApprovalDecision::Reject],
        "the outside-write grant is per call: no memory tier may be offered"
    );
    let written = Path::new(&file.replace('/', "\\")).exists() || Path::new(&file).exists();
    assert!(
        written,
        "approved outside write should create the file: {} | outputs: {:?}",
        file,
        tool_outputs(&first.events)
    );

    let second = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::Auto,
        ApprovalDecision::Reject,
    )
    .await;
    assert_eq!(
        second.approvals.len(),
        1,
        "Allow is per-call: the second outside write must pop again"
    );
    let outs = tool_outputs(&second.events);
    assert!(
        outs.iter()
            .any(|o| o.contains("declined write outside the workspace")),
        "reject note expected, got {outs:?}"
    );
    agent.shutdown();
    let _ = std::fs::remove_dir_all(&outside_root);
}

/// Rejecting an outside write returns the boundary note as the tool error and
/// leaves the filesystem untouched
#[tokio::test]
async fn outside_write_reject_leaves_no_file() {
    let (config_path, dir, data_dir) = setup("fsout-write-reject");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let session_id = new_session(&agent, dir.clone()).await;

    let (outside_root, outside_file) = outside_target("write-reject");
    let outside_dir = outside_file.rsplit_once('/').unwrap().0.to_string();
    let file = format!("{outside_dir}/never-written.txt");
    let msg = format!(
        "{} {file}",
        pig_provider::mock::SCENARIO_OUTSIDE_WRITE_TRIGGER
    );

    let run = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::Auto,
        ApprovalDecision::Reject,
    )
    .await;
    assert_eq!(run.approvals.len(), 1);
    assert!(
        tool_outputs(&run.events)
            .iter()
            .any(|o| o.contains("declined write outside the workspace")),
        "reject note expected"
    );
    assert!(
        !Path::new(&file.replace('/', "\\")).exists() && !Path::new(&file).exists(),
        "a rejected outside write must not create the file"
    );
    agent.shutdown();
    let _ = std::fs::remove_dir_all(&outside_root);
}

/// Full has no sandbox and no workspace boundary: an outside write runs
/// directly, without an approval card.
#[tokio::test]
async fn full_outside_write_needs_no_approval() {
    let (config_path, dir, data_dir) = setup("fsout-full-write");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let session_id = new_session(&agent, dir.clone()).await;

    let (outside_root, outside_file) = outside_target("full-write");
    let outside_dir = outside_file.rsplit_once('/').unwrap().0.to_string();
    let file = format!("{outside_dir}/full-written.txt");
    let msg = format!(
        "{} {file}",
        pig_provider::mock::SCENARIO_OUTSIDE_WRITE_TRIGGER
    );

    let run = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::Full,
        ApprovalDecision::Reject,
    )
    .await;
    assert!(
        run.approvals.is_empty(),
        "Full must not ask for outside writes: {:?}",
        run.approvals
    );
    assert!(
        Path::new(&file.replace('/', "\\")).exists() || Path::new(&file).exists(),
        "Full outside write should create the file: {file} | outputs: {:?}",
        tool_outputs(&run.events)
    );
    agent.shutdown();
    let _ = std::fs::remove_dir_all(&outside_root);
}

/// A doomed outside write: the target matches the seeded `.env` deny group (and
/// is on the sensitive-name list besides) — neither is liftable by an approval,
/// so the gate rejects upfront instead of popping a dialog the tool layer
/// would have vetoed anyway.
#[tokio::test]
async fn outside_write_to_a_sensitive_name_never_pops() {
    let (config_path, dir, data_dir) = setup("fsout-write-sensitive");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir);
    let session_id = new_session(&agent, dir.clone()).await;

    let (outside_root, outside_file) = outside_target("write-sensitive");
    let outside_dir = outside_file.rsplit_once('/').unwrap().0.to_string();
    let file = format!("{outside_dir}/.env");
    let msg = format!(
        "{} {file}",
        pig_provider::mock::SCENARIO_OUTSIDE_WRITE_TRIGGER
    );

    let run = run_turn(
        &agent,
        &session_id,
        &msg,
        ExecMode::Auto,
        ApprovalDecision::Allow,
    )
    .await;
    assert!(
        run.approvals.is_empty(),
        "no dialog for a doomed write: {:?}",
        run.approvals
    );
    let outs = tool_outputs(&run.events);
    assert!(
        outs.iter()
            .any(|o| o.contains("Approvals cannot lift deny rules")),
        "the upfront rejection explains itself: {outs:?}"
    );
    assert!(
        !Path::new(&file.replace('/', "\\")).exists() && !Path::new(&file).exists(),
        "the write never happened"
    );
    agent.shutdown();
    let _ = std::fs::remove_dir_all(&outside_root);
}
