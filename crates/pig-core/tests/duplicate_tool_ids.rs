//! Duplicate provider tool-call ids in one response: the wire id stays the
//! provider's id (history/tool_result pairing), while the UI item ids and
//! approval request ids must be unique per invocation.

use crate::common;

use std::time::{Duration, Instant};

use pig_protocol::{ApprovalDecision, Event, ExecMode, Op};
use pig_provider::mock;

use common::{new_session, setup_with_log};

fn item_ids(events: &[Event], kind: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ToolInputStart { item_id, .. } if kind == "input" => Some(item_id.clone()),
            Event::ToolCallBegin { item_id, .. } if kind == "begin" => Some(item_id.clone()),
            Event::ToolCallEnd { item_id, .. } if kind == "end" => Some(item_id.clone()),
            _ => None,
        })
        .collect()
}

fn send(agent: &pig_core::AgentHandle, session_id: &str, content: &str, mode: ExecMode) {
    agent
        .ops
        .send_blocking(Op::SendMessage {
            session_id: session_id.into(),
            content: content.into(),
            files: vec![],
            images: vec![],
            permissions: mode.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .unwrap();
}

/// Two approval-gated sync-point calls share one provider id. Each gets its own
/// approval request id (answering one never aliases the other) and its own item
/// id; both write their own path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_serial_tool_ids_keep_distinct_approvals_and_items() {
    let (config_path, cwd, data_dir, _log) = setup_with_log("duplicate-serial");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd.clone()).await;

    send(
        &agent,
        &sid,
        mock::DUPLICATE_SERIAL_ID_TRIGGER,
        ExecMode::ReadOnly,
    );
    let mut events = Vec::new();
    let mut approvals: Vec<(String, String)> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "turn did not finish: {events:#?}"
        );
        let Ok(event) = tokio::time::timeout(Duration::from_secs(3), agent.events.recv()).await
        else {
            continue;
        };
        let Ok(event) = event else {
            panic!("event channel closed: {events:#?}");
        };
        if let Event::ApprovalRequested {
            request_id,
            tool,
            detail,
            ..
        } = &event
            && tool == "Write"
        {
            approvals.push((request_id.clone(), detail.clone()));
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
        events.push(event);
        if done {
            break;
        }
    }

    assert_eq!(
        approvals.len(),
        2,
        "both duplicate-id writes ask separately: {events:#?}"
    );
    assert_ne!(
        approvals[0].0, approvals[1].0,
        "duplicate provider ids must not alias approval requests: {approvals:#?}"
    );
    let details: Vec<&str> = approvals
        .iter()
        .map(|(_, detail)| detail.as_str())
        .collect();
    assert!(
        details.iter().any(|detail| detail.contains("dup-a.txt"))
            && details.iter().any(|detail| detail.contains("dup-b.txt")),
        "each approval names its own target: {details:#?}"
    );

    let inputs = item_ids(&events, "input");
    let begins = item_ids(&events, "begin");
    let ends = item_ids(&events, "end");
    assert_eq!(begins.len(), 2, "{events:#?}");
    assert_eq!(ends.len(), 2, "{events:#?}");
    assert_eq!(
        inputs.len(),
        2,
        "the streaming previews exist for both calls: {events:#?}"
    );
    assert!(
        inputs
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            == 2,
        "input previews must not collide: {inputs:?}"
    );
    assert!(
        begins
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            == 2,
        "item ids must be unique: {begins:?}"
    );
    assert!(
        ends.iter().collect::<std::collections::HashSet<_>>().len() == 2,
        "end ids must be unique: {ends:?}"
    );
    assert_eq!(
        begins, inputs,
        "each execution card reuses its own streaming preview"
    );
    assert!(
        begins
            .iter()
            .all(|item_id| item_id.ends_with(mock::DUPLICATE_CALL_ID)),
        "the provider call id stays in the internal id for diagnostics: {begins:?}"
    );

    assert_eq!(
        std::fs::read_to_string(cwd.join("dup-a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        std::fs::read_to_string(cwd.join("dup-b.txt")).unwrap(),
        "beta\n"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::TextDone { full_text, .. }
                if full_text.contains(mock::DUPLICATE_SERIAL_ID_MARKER)
        )),
        "the turn continued after both tools: {events:#?}"
    );
    agent.shutdown();
}

/// Two read-only calls share one provider id and run in the same parallel
/// group; both cards/results are distinct, while the next wire request keeps
/// the provider's duplicate id for both tool results.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_parallel_tool_ids_keep_distinct_cards_and_wire_ids() {
    let (config_path, cwd, data_dir, log) = setup_with_log("duplicate-parallel");
    std::fs::write(cwd.join("dup-a.txt"), "alpha\n").unwrap();
    std::fs::write(cwd.join("dup-b.txt"), "beta\n").unwrap();
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd.clone()).await;

    send(
        &agent,
        &sid,
        mock::DUPLICATE_PARALLEL_ID_TRIGGER,
        ExecMode::Auto,
    );
    let mut events = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "turn did not finish: {events:#?}"
        );
        let Ok(event) = tokio::time::timeout(Duration::from_secs(3), agent.events.recv()).await
        else {
            continue;
        };
        let Ok(event) = event else {
            panic!("event channel closed: {events:#?}");
        };
        let done = matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. }
        );
        events.push(event);
        if done {
            break;
        }
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::ApprovalRequested { .. })),
        "read-only calls need no approval: {events:#?}"
    );

    let inputs = item_ids(&events, "input");
    let begins = item_ids(&events, "begin");
    let ends: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCallEnd {
                item_id, output, ..
            } => Some((item_id.as_str(), output.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(begins.len(), 2, "{events:#?}");
    assert_eq!(ends.len(), 2, "{events:#?}");
    assert!(
        begins
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            == 2,
        "parallel item ids must be unique: {begins:?}"
    );
    assert_eq!(begins, inputs, "each card reuses its own preview id");
    assert!(
        ends.iter().any(|(_, output)| output.contains("alpha"))
            && ends.iter().any(|(_, output)| output.contains("beta")),
        "both results settle independently: {ends:#?}"
    );

    // Wire fidelity: the follow-up request carries the provider's duplicate id
    // for BOTH tool results (history pairing is not rewritten).
    let bodies = log.lock().unwrap().clone();
    let follow_up = bodies
        .iter()
        .rev()
        .find(|body| {
            body.contains(mock::DUPLICATE_PARALLEL_ID_TRIGGER)
                && body.matches("\"role\":\"tool\"").count() == 2
        })
        .expect("follow-up request with both tool results");
    assert_eq!(
        follow_up.matches("\"tool_call_id\":\"call_dup\"").count(),
        2,
        "the provider's duplicate call ids stay on the wire: {follow_up}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::TextDone { full_text, .. }
                if full_text.contains(mock::DUPLICATE_PARALLEL_ID_MARKER)
        )),
        "the turn continued after both parallel calls: {events:#?}"
    );
    agent.shutdown();
}
