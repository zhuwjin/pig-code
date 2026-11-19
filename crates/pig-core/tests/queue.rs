//! Message queueing + mid-turn steering integration tests. The SLOW_BASH
//! scenario (a `sleep 30` tool call) provides a stable mid-turn window for
//! steer delivery/fallback timing; the default mock scenario covers the
//! queue lane's turn-end auto-continue flow.

use crate::common;

use common::{new_session, recv_until, setup};
use pig_protocol::{ApprovalDecision, Event, ExecMode, MessageDelivery, Op};
use pig_provider::mock;
use std::time::{Duration, Instant};

async fn send(agent: &pig_core::AgentHandle, sid: &str, text: &str, delivery: MessageDelivery) {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.to_string(),
            content: text.to_string(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery,
        })
        .await
        .unwrap();
}

/// Collect events until the predicate hits, auto-approving nothing (Auto
/// mode keeps the SLOW bash approval-free — it is not read-only)
async fn slow_command_supported() -> bool {
    if common::native_sandbox_usable() {
        return true;
    }
    common::assert_native_sandbox_rejection().await;
    false
}

async fn collect(
    agent: &pig_core::AgentHandle,
    timeout: Duration,
    mut done: impl FnMut(&Event) -> bool,
) -> Vec<Event> {
    let deadline = Instant::now() + timeout;
    let mut collected = Vec::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out; collected: {collected:#?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(1), agent.events.recv()).await
        else {
            continue;
        };
        let finished = done(&event);
        collected.push(event);
        if finished {
            return collected;
        }
    }
}

/// Latest queue-lane snapshot from collected events
fn queue_items(events: &[Event]) -> Vec<pig_protocol::QueueItem> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::QueueChanged { items, .. } => Some(items.clone()),
            _ => None,
        })
        .next_back()
        .unwrap_or_default()
}

fn steer_items(events: &[Event]) -> Vec<pig_protocol::QueueItem> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::SteerChanged { items, .. } => Some(items.clone()),
            _ => None,
        })
        .next_back()
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_accepted_then_falls_back_on_interrupt() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-steer-fallback");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    // The sleep-30 window: a stable mid-turn moment to deliver the steer
    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;

    send(&agent, &sid, "STEER_MARKER_1", MessageDelivery::Steer).await;
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::SteerChanged { .. })
    })
    .await;
    let steered = steer_items(&events);
    assert_eq!(steered.len(), 1, "steer lane holds the item: {steered:?}");
    assert_eq!(steered[0].text, "STEER_MARKER_1");
    assert!(steered[0].id.starts_with('q'), "ids are q-prefixed");

    // Interrupting the turn: the unconsumed steer falls back to the queue
    // lane in place. TurnAborted is emitted from inside the turn; the
    // fallback snapshot lands afterwards at the manager's wrap-up, so wait
    // for the queue-lane appearance (and then cancel the drain so the
    // restarted turn does not run the leftover message)
    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::QueueChanged { items, .. }
            if items.iter().any(|item| item.text == "STEER_MARKER_1"))
    })
    .await;
    let queued = queue_items(&events);
    let leftover_id = queued
        .iter()
        .find(|item| item.text == "STEER_MARKER_1")
        .expect("fallback item present")
        .id
        .clone();
    agent
        .ops
        .send(Op::CancelQueued {
            session_id: sid.clone(),
            id: leftover_id,
        })
        .await
        .unwrap();
    assert!(
        queued.iter().any(|item| item.text == "STEER_MARKER_1"),
        "the aborted turn's steer falls back to the queue lane: {queued:?}"
    );
    // No user message was ever injected (the boundary never came)
    assert!(
        !events.iter().any(
            |e| matches!(e, Event::UserMessage { text, .. } if text.contains("STEER_MARKER_1"))
        )
    );

    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_injects_at_stop_boundary_within_same_turn() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-steer-inject");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let requests = mock::start_mock_server_with_log().1;
    let _ = requests; // the log of THIS agent's server lives in its config; use events instead
    let sid = new_session(&agent, cwd).await;

    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;
    send(&agent, &sid, "STEER_AT_STOP", MessageDelivery::Steer).await;
    recv_until(&agent.events, Duration::from_secs(10), |e| {
        matches!(e, Event::SteerChanged { .. })
    })
    .await;

    // Let the sleep finish (~30s): the text-only stop boundary injects the
    // steer and runs one more model round INSIDE the same turn — exactly one
    // TurnStarted and one TurnComplete overall
    let mut turn_starts = 0usize;
    let mut user_seen = false;
    let mut steer_cleared = false;
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        assert!(Instant::now() < deadline, "turn did not finish in time");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::TurnStarted { .. } => turn_starts += 1,
            Event::UserMessage { text, .. } if text == "STEER_AT_STOP" => user_seen = true,
            Event::SteerChanged { items, .. } if items.is_empty() => steer_cleared = true,
            Event::TurnComplete { .. } => break,
            _ => {}
        }
    }
    assert_eq!(
        turn_starts, 0,
        "no extra turn: the steer rides the running one"
    );
    assert!(user_seen, "the steer became a real user message mid-turn");
    assert!(steer_cleared, "the steer lane drained on injection");

    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_lane_auto_continues_with_ids() {
    let (config_path, cwd, data_dir) = setup("queue-auto-continue");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    // Default delivery (Queue): a mid-turn send lands in the queue lane with
    // an id, then auto-starts when the turn ends
    send(&agent, &sid, "first turn please", MessageDelivery::Queue).await;
    recv_until(&agent.events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnStarted { .. })
    })
    .await;
    send(&agent, &sid, "ECHO_SECOND", MessageDelivery::Queue).await;
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::QueueChanged { .. })
    })
    .await;
    let queued = queue_items(&events);
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].text, "ECHO_SECOND");

    let mut completes = 0;
    let mut second_user = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline);
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::UserMessage { text, .. } if text == "ECHO_SECOND" => second_user = true,
            Event::TurnComplete { .. } => {
                completes += 1;
                if completes == 2 {
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(
        second_user,
        "the queued message auto-started as the next turn"
    );

    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_and_reorder_manage_queue_lane() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-manage");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    // Three items queued during one slow window: cancel the first, reorder
    // the last to the front
    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;
    send(&agent, &sid, "Q_A", MessageDelivery::Queue).await;
    send(&agent, &sid, "Q_B", MessageDelivery::Queue).await;
    send(&agent, &sid, "Q_C", MessageDelivery::Queue).await;
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 3),
    )
    .await;
    let items = queue_items(&events);
    let (id_a, id_c) = (items[0].id.clone(), items[2].id.clone());

    agent
        .ops
        .send(Op::CancelQueued {
            session_id: sid.clone(),
            id: id_a,
        })
        .await
        .unwrap();
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 2),
    )
    .await;
    let items = queue_items(&events);
    assert_eq!(
        items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
        vec!["Q_B", "Q_C"]
    );

    // Move C before B (before_id = B)
    let id_b = items[0].id.clone();
    agent
        .ops
        .send(Op::ReorderQueued {
            session_id: sid.clone(),
            id: id_c.clone(),
            before_id: Some(id_b),
        })
        .await
        .unwrap();
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::QueueChanged { items, .. } if items.len() == 2
            && items[0].text == "Q_C")
    })
    .await;
    let items = queue_items(&events);
    assert_eq!(
        items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
        vec!["Q_C", "Q_B"],
        "reorder put C before B"
    );

    // Drain: interrupt the slow turn (a user stop pauses the queue — ZCode
    // error-paused semantics), then resume it; C must run before B
    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: false,
                ..
            }
        )
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::QueueDrainChanged {
                reason: Some(pig_protocol::QueuePauseReason::Stopped),
                ..
            }
        )),
        "a user stop pauses the non-empty queue: {events:?}"
    );
    agent
        .ops
        .send(Op::ResumeQueue {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let mut order: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "queued turns did not run: {order:?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        if let Event::UserMessage { text, .. } = &event
            && (text == "Q_C" || text == "Q_B")
        {
            order.push(text.clone());
            if order.len() == 2 {
                break;
            }
        }
    }
    assert_eq!(order, vec!["Q_C", "Q_B"]);

    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_moves_reorder_and_steer_send_now() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-lane-moves");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;
    send(&agent, &sid, "Q_A", MessageDelivery::Queue).await;
    send(&agent, &sid, "Q_B", MessageDelivery::Queue).await;
    send(&agent, &sid, "S_A", MessageDelivery::Steer).await;
    send(&agent, &sid, "S_B", MessageDelivery::Steer).await;
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::SteerChanged { items, .. } if items.len() == 2),
    )
    .await;
    let queued = queue_items(&events);
    let steered = steer_items(&events);
    let (q_a, q_b) = (queued[0].id.clone(), queued[1].id.clone());
    let (s_a, s_b) = (steered[0].id.clone(), steered[1].id.clone());

    agent
        .ops
        .send(Op::ReorderQueued {
            session_id: sid.clone(),
            id: s_b.clone(),
            before_id: Some(s_a.clone()),
        })
        .await
        .unwrap();
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::SteerChanged { items, .. } if items[0].text == "S_B"),
    )
    .await;
    assert!(
        queue_items(&events).is_empty(),
        "a steer-only reorder emits no queue-lane snapshot"
    );

    agent
        .ops
        .send(Op::ReorderQueued {
            session_id: sid.clone(),
            id: q_b.clone(),
            before_id: Some(q_a.clone()),
        })
        .await
        .unwrap();
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items[0].text == "Q_B"),
    )
    .await;
    assert!(
        steer_items(&events).is_empty(),
        "a queue-only reorder emits no steer-lane snapshot"
    );

    // A cross-lane anchor cannot move the item into the other lane; it only
    // resolves to the current lane's tail.
    agent
        .ops
        .send(Op::ReorderQueued {
            session_id: sid.clone(),
            id: s_b.clone(),
            before_id: Some(q_a.clone()),
        })
        .await
        .unwrap();
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::SteerChanged { items, .. } if items[0].text == "S_A"),
    )
    .await;
    assert_eq!(
        steer_items(&events)
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        vec!["S_A", "S_B"]
    );
    assert!(
        queue_items(&events).is_empty(),
        "a cross-lane anchor still emits only the dragged lane's snapshot"
    );

    agent
        .ops
        .send(Op::MoveQueuedLane {
            session_id: sid.clone(),
            id: q_b.clone(),
            steer: true,
        })
        .await
        .unwrap();
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::SteerChanged { items, .. } if items.iter().any(|item| item.text == "Q_B"))
    })
    .await;
    assert_eq!(
        steer_items(&events)
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        vec!["Q_B", "S_A", "S_B"]
    );
    assert_eq!(queue_items(&events)[0].text, "Q_A");

    agent
        .ops
        .send(Op::MoveQueuedLane {
            session_id: sid.clone(),
            id: s_a.clone(),
            steer: false,
        })
        .await
        .unwrap();
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::SteerChanged { items, .. } if !items.iter().any(|item| item.text == "S_A"))
    })
    .await;
    assert_eq!(
        queue_items(&events)
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        vec!["Q_A", "S_A"]
    );
    assert_eq!(
        steer_items(&events)
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        vec!["Q_B", "S_B"]
    );

    agent
        .ops
        .send(Op::SendQueuedNow {
            session_id: sid.clone(),
            id: q_b,
        })
        .await
        .unwrap();
    let events = collect(
        &agent,
        Duration::from_secs(15),
        |e| matches!(e, Event::UserMessage { text, .. } if text == "Q_B"),
    )
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnAborted { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::UserMessage { text, .. } if text == "Q_B"))
    );

    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_queued_now_interrupts_and_runs_first() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-send-now");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;
    send(&agent, &sid, "SEND_ME_NOW", MessageDelivery::Queue).await;
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 1),
    )
    .await;
    let id = queue_items(&events)[0].id.clone();

    agent
        .ops
        .send(Op::SendQueuedNow {
            session_id: sid.clone(),
            id,
        })
        .await
        .unwrap();

    let mut aborted = false;
    let mut ran = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "send-now flow did not complete");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::TurnAborted { .. } => aborted = true,
            Event::UserMessage { text, .. } if text == "SEND_ME_NOW" => ran = true,
            Event::TurnComplete { .. } if aborted => break,
            _ => {}
        }
    }
    assert!(aborted, "send-now interrupts the running turn");
    assert!(ran, "the promoted item starts as the fresh turn");

    agent.shutdown();
}

/// Deleting every queued item clears the pause banner (regression: the
/// banner survived an emptied queue — nothing left to resume).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_all_queued_items_clears_the_pause() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-cancel-unpauses");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;
    send(&agent, &sid, "DEL_A", MessageDelivery::Queue).await;
    send(&agent, &sid, "DEL_B", MessageDelivery::Queue).await;
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 2),
    )
    .await;
    let ids: Vec<String> = queue_items(&events).iter().map(|i| i.id.clone()).collect();

    // User stop pauses the queue
    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: false,
                ..
            }
        )
    })
    .await;

    // Delete both items; the pause clears with the last one
    for id in ids {
        agent
            .ops
            .send(Op::CancelQueued {
                session_id: sid.clone(),
                id,
            })
            .await
            .unwrap();
    }
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: true,
                ..
            }
        )
    })
    .await;

    agent.shutdown();
}

/// SendQueuedNow on a paused queue is the user's resume: it re-enables the
/// drain (QueueDrainChanged true) so the rest of the queue keeps going after
/// the promoted turn (regression: the banner stayed and auto_drain remained
/// false while the promoted item ran).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_queued_now_resumes_a_paused_queue() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-send-now-resume");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;
    send(&agent, &sid, "NOW_A", MessageDelivery::Queue).await;
    send(&agent, &sid, "LATER_B", MessageDelivery::Queue).await;
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 2),
    )
    .await;
    let id_a = queue_items(&events)[0].id.clone();

    // User stop pauses the non-empty queue
    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: false,
                ..
            }
        )
    })
    .await;

    agent
        .ops
        .send(Op::SendQueuedNow {
            session_id: sid.clone(),
            id: id_a,
        })
        .await
        .unwrap();

    let mut resumed = false;
    let mut order: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "send-now on a paused queue did not resume the drain: {order:?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::QueueDrainChanged {
                auto_drain: true, ..
            } => resumed = true,
            Event::UserMessage { text, .. } if text == "NOW_A" || text == "LATER_B" => {
                order.push(text.clone());
                if order.len() == 2 {
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(resumed, "send-now re-enabled the drain (banner clears)");
    assert_eq!(
        order,
        vec!["NOW_A", "LATER_B"],
        "the promoted item runs first, then the drain continues"
    );

    agent.shutdown();
}

/// A stale send-now id (already cancelled) must not unpause the queue or start
/// anything: it reports a nonfatal rejection and leaves the paused backlog
/// exactly as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_send_queued_now_keeps_the_queue_paused() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-stale-send-now");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;
    send(&agent, &sid, "NOW_A", MessageDelivery::Queue).await;
    send(&agent, &sid, "LATER_B", MessageDelivery::Queue).await;
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 2),
    )
    .await;
    let id_a = queue_items(&events)[0].id.clone();

    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: false,
                ..
            }
        )
    })
    .await;
    // Cancel A: the paused backlog keeps only LATER_B.
    agent
        .ops
        .send(Op::CancelQueued {
            session_id: sid.clone(),
            id: id_a.clone(),
        })
        .await
        .unwrap();
    collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 1),
    )
    .await;

    // A stale send-now id must be rejected without changing the drain state.
    agent
        .ops
        .send(Op::SendQueuedNow {
            session_id: sid.clone(),
            id: id_a,
        })
        .await
        .unwrap();
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::Error { error, .. } if matches!(
                error.semantic_error(),
                pig_protocol::CoreError::Internal { detail } if detail.contains("no longer pending")
            )
        )
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Error {
                fatal: false,
                error,
                ..
            } if matches!(
                error.semantic_error(),
                pig_protocol::CoreError::Internal { detail } if detail.contains("no longer pending")
            )
        )),
        "the stale id is rejected nonfatally: {events:#?}"
    );

    // Quiet window: no resume banner change, no turn, no queued message.
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(500) {
        match tokio::time::timeout(Duration::from_millis(100), agent.events.recv()).await {
            Ok(Ok(Event::QueueDrainChanged {
                auto_drain: true, ..
            })) => panic!("a stale send-now unpaused the queue"),
            Ok(Ok(Event::TurnStarted { .. })) => {
                panic!("a stale send-now started a turn")
            }
            Ok(Ok(Event::UserMessage { text, .. })) if text == "LATER_B" => {
                panic!("a stale send-now started the queued message")
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {}
        }
    }

    // The backlog is still there and still paused: an explicit resume runs it.
    agent
        .ops
        .send(Op::ResumeQueue {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let resumed = collect(
        &agent,
        Duration::from_secs(20),
        |e| matches!(e, Event::UserMessage { text, .. } if text == "LATER_B"),
    )
    .await;
    assert!(
        resumed
            .iter()
            .any(|e| matches!(e, Event::UserMessage { text, .. } if text == "LATER_B")),
        "the paused backlog survived the stale id: {resumed:#?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_with_images_falls_back_to_queue_lane() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-steer-images");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;

    // Steer with an attachment: rejected to the queue lane (attachments
    // cannot ride the steer injection)
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: "with picture".to_string(),
            files: vec![],
            images: vec![pig_protocol::PendingImage {
                bytes: vec![1, 2, 3],
                mime: "image/png".to_string(),
            }],
            permissions: ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Steer,
        })
        .await
        .unwrap();
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 1),
    )
    .await;
    assert!(
        steer_items(&events).is_empty(),
        "attachments never enter the steer lane"
    );
    assert_eq!(queue_items(&events)[0].text, "with picture");

    agent.shutdown();
}

/// Held-queue confirmation: a user stop pauses the queue; the next send is
/// held for a disposition; ClearQueueAndSend drops the backlog and runs the
/// held message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_queue_confirmation_flow() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-held");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd.clone()).await;

    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;
    send(&agent, &sid, "OLD_BACKLOG", MessageDelivery::Queue).await;
    collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 1),
    )
    .await;

    // User stop pauses the queue
    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: false,
                ..
            }
        )
    })
    .await;
    assert!(events.iter().any(|e| matches!(
        e,
        Event::QueueDrainChanged {
            reason: Some(pig_protocol::QueuePauseReason::Stopped),
            ..
        }
    )));

    // The next send is held, not silently queued
    send(&agent, &sid, "FRESH_MESSAGE", MessageDelivery::Queue).await;
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::HeldQueuePrompt { .. })
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::HeldQueuePrompt { text, items, .. }
                if text == "FRESH_MESSAGE" && items.len() == 1
        )),
        "{events:?}"
    );

    // Clear-and-send: the backlog disappears, the held message runs
    agent
        .ops
        .send(Op::ResolveHeldQueue {
            session_id: sid.clone(),
            disposition: pig_protocol::HeldDisposition::ClearQueueAndSend,
        })
        .await
        .unwrap();
    let mut ran_held = false;
    let mut queue_emptied = false;
    let mut prompt_closed = false;
    let mut old_backlog_ran = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "clear-and-send did not complete");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::UserMessage { text, .. } if text == "FRESH_MESSAGE" => ran_held = true,
            Event::UserMessage { text, .. } if text == "OLD_BACKLOG" => old_backlog_ran = true,
            Event::QueueChanged { items, .. } if items.is_empty() => queue_emptied = true,
            Event::HeldQueueResolved { .. } => prompt_closed = true,
            Event::TurnComplete { .. } if ran_held => break,
            _ => {}
        }
    }
    assert!(ran_held, "the held message started as the new turn");
    assert!(queue_emptied, "the old backlog was cleared");
    assert!(prompt_closed, "the prompt closed");
    assert!(
        !old_backlog_ran,
        "OLD_BACKLOG was cleared without ever running"
    );

    agent.shutdown();
}

/// Queue persistence: items pending at shutdown restore on reopen, PAUSED
/// (restart-pause semantics — a restart never auto-continues the head; the
/// user resumes via Op::ResumeQueue).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_queue_persists_across_restart() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-persist");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let sid = new_session(&agent, cwd.clone()).await;

    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;
    send(&agent, &sid, "PERSIST_A", MessageDelivery::Queue).await;
    send(&agent, &sid, "PERSIST_B", MessageDelivery::Queue).await;
    collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 2),
    )
    .await;
    // Stop + shutdown with the paused queue holding both items
    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: false,
                ..
            }
        )
    })
    .await;
    agent.shutdown();

    // Reopen in a fresh manager: the persisted queue restores PAUSED (no
    // auto-drain across a restart), then ResumeQueue starts the head
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    agent
        .ops
        .send(Op::OpenSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let mut restored = false;
    let mut paused = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !(restored && paused) {
        assert!(Instant::now() < deadline, "reopen restore did not complete");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::QueueChanged { items, .. } if items.len() == 2 => restored = true,
            Event::QueueDrainChanged {
                auto_drain: false,
                reason: Some(pig_protocol::QueuePauseReason::Restarted),
                ..
            } => paused = true,
            Event::UserMessage { text, .. } if text == "PERSIST_A" => {
                panic!("reopened queue must not auto-run")
            }
            _ => {}
        }
    }

    agent
        .ops
        .send(Op::ResumeQueue {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let mut head_ran = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "resumed head did not run");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::UserMessage { text, .. } if text == "PERSIST_A" => head_ran = true,
            Event::TurnComplete { .. } if head_ran => break,
            _ => {}
        }
    }
    assert!(head_ran, "the resumed head ran as a turn");

    agent.shutdown();
}

/// One steer per boundary: two steers sent in one window inject at two
/// consecutive boundaries (two extra model rounds), all within one turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_steer_per_boundary() {
    if !slow_command_supported().await {
        return;
    }
    let (config_path, cwd, data_dir) = setup("queue-steer-one-per");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    send(
        &agent,
        &sid,
        mock::SCENARIO_SLOW_TRIGGER,
        MessageDelivery::Queue,
    )
    .await;
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "ExecCommand"),
    )
    .await;
    send(&agent, &sid, "STEER_FIRST", MessageDelivery::Steer).await;
    send(&agent, &sid, "STEER_SECOND", MessageDelivery::Steer).await;
    collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::SteerChanged { items, .. } if items.len() == 2),
    )
    .await;

    let mut injections: Vec<String> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        assert!(
            Instant::now() < deadline,
            "turn did not finish: {injections:?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::UserMessage { text, .. } if text == "STEER_FIRST" || text == "STEER_SECOND" => {
                injections.push(text.clone());
            }
            Event::TurnComplete { .. } => break,
            _ => {}
        }
    }
    assert_eq!(
        injections,
        vec!["STEER_FIRST", "STEER_SECOND"],
        "both steers inject, earliest first, one per boundary"
    );

    agent.shutdown();
}

/// Held + compact interaction: a manual compaction must not drain or unpause a
/// paused queue, and the held message must survive (persisted, never
/// overwritten) until its disposition is resolved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_and_compact_keep_a_paused_queue_paused() {
    let (config_path, cwd, data_dir) = setup("queue-held-compact");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    // Pause a non-empty queue, then hold the next send. The pause turn waits on
    // an outside-workspace read approval (ReadOnly), which is a deterministic
    // mid-turn window; ECHO_HISTORY-driven follow-ups complete as plain text.
    let outside = std::env::temp_dir()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/"))
        .join(format!("pig-held-compact-{}.txt", std::process::id()));
    let outside = outside.display().to_string().replace('\\', "/");
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} {}", mock::SCENARIO_OUTSIDE_WRITE_TRIGGER, outside),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .unwrap();
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ApprovalRequested { tool, .. } if tool == "Write"),
    )
    .await;
    send(
        &agent,
        &sid,
        "ECHO_HISTORY OLD_BACKLOG",
        MessageDelivery::Queue,
    )
    .await;
    collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 1),
    )
    .await;
    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: false,
                ..
            }
        )
    })
    .await;
    send(
        &agent,
        &sid,
        "ECHO_HISTORY HELD_MESSAGE",
        MessageDelivery::Queue,
    )
    .await;
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::HeldQueuePrompt { .. })
    })
    .await;

    // Compact while the queue is paused and a message is held.
    agent
        .ops
        .send(Op::Compact {
            session_id: sid.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let compact_events = collect(&agent, Duration::from_secs(15), |e| {
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    assert!(
        !compact_events.iter().any(|e| matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: true,
                ..
            }
        )),
        "a completed compaction must not unpause the queue: {compact_events:#?}"
    );
    assert!(
        !compact_events.iter().any(|e| matches!(
            e,
            Event::UserMessage { text, .. }
                if text.contains("OLD_BACKLOG") || text.contains("HELD_MESSAGE")
        )),
        "a manual compaction must not start queued work: {compact_events:#?}"
    );

    // Keep the backlog: the held message joins it, still paused, and nothing
    // starts on its own.
    agent
        .ops
        .send(Op::ResolveHeldQueue {
            session_id: sid.clone(),
            disposition: pig_protocol::HeldDisposition::KeepQueueAndSend,
        })
        .await
        .unwrap();
    let events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 2),
    )
    .await;
    let items = queue_items(&events);
    assert_eq!(
        items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
        vec!["ECHO_HISTORY OLD_BACKLOG", "ECHO_HISTORY HELD_MESSAGE"],
        "both messages stayed in the queue, in admission order"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::TurnStarted { .. })),
        "KeepQueueAndSend must not start a turn: {events:#?}"
    );
    let quiet = tokio::time::timeout(Duration::from_millis(400), agent.events.recv()).await;
    assert!(
        !matches!(
            quiet,
            Ok(Ok(Event::TurnStarted { .. } | Event::UserMessage { .. }))
        ),
        "nothing runs while the queue stays paused"
    );

    // Resume: both messages run in order.
    agent
        .ops
        .send(Op::ResumeQueue {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let mut ran = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "resumed queue did not run: {ran:?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::UserMessage { text, .. }
                if text.contains("OLD_BACKLOG") || text.contains("HELD_MESSAGE") =>
            {
                ran.push(text.clone());
                if ran.len() == 2 {
                    break;
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        ran,
        vec!["ECHO_HISTORY OLD_BACKLOG", "ECHO_HISTORY HELD_MESSAGE"]
    );
    agent.shutdown();
}

/// A held-queue disposition that arrives while a turn owns the session must not
/// panic and must not lose the held message: ClearQueueAndSend keeps it queued
/// at the front for the next boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolving_held_queue_while_a_turn_runs_keeps_the_message() {
    let (config_path, cwd, data_dir) = setup("queue-held-running");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let sid = new_session(&agent, cwd).await;

    let outside = std::env::temp_dir()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/"))
        .join(format!("pig-held-running-{}.txt", std::process::id()));
    let outside = outside.display().to_string().replace('\\', "/");
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} {}", mock::SCENARIO_OUTSIDE_WRITE_TRIGGER, outside),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .unwrap();
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ApprovalRequested { tool, .. } if tool == "Write"),
    )
    .await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} queued", mock::SCENARIO_Q_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .unwrap();
    collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 1),
    )
    .await;
    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: false,
                ..
            }
        )
    })
    .await;
    send(&agent, &sid, "ECHO_HISTORY HELD_B", MessageDelivery::Queue).await;
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::HeldQueuePrompt { .. })
    })
    .await;

    // Resume starts the queued question turn; AskUserQuestion blocks on a
    // reply, giving a deterministic running window while HELD_B is still
    // unresolved.
    agent
        .ops
        .send(Op::ResumeQueue {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    recv_until(&agent.events, Duration::from_secs(15), |e| {
        matches!(e, Event::QuestionRequested { .. })
    })
    .await;

    // Clear-and-send during the running turn: queued at the front, no panic.
    agent
        .ops
        .send(Op::ResolveHeldQueue {
            session_id: sid.clone(),
            disposition: pig_protocol::HeldDisposition::ClearQueueAndSend,
        })
        .await
        .unwrap();
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::HeldQueueResolved { .. })
    })
    .await;
    assert!(
        event_errors(&events).is_empty(),
        "a stale held resolution is a normal state transition: {events:#?}"
    );

    // Stop the running slow turn; the held message stays queued (a stop pauses
    // a non-empty queue).
    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: false,
                ..
            }
        )
    })
    .await;

    agent
        .ops
        .send(Op::ResumeQueue {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let mut held_ran = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "the held message did not run");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::UserMessage { text, .. } if text.contains("HELD_B") => held_ran = true,
            Event::TurnComplete { .. } if held_ran => break,
            _ => {}
        }
    }
    agent.shutdown();
}

/// No model at send time: the message stays persisted (and reported through a
/// typed nonfatal error) so a later restart with a working model can run it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_model_send_keeps_the_message_for_restart() {
    let (config_path, cwd, data_dir) = setup("queue-no-model");
    let working_config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        &config_path,
        "default_provider = \"missing\"\ndefault_model = \"none\"\n",
    )
    .unwrap();

    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let sid = new_session(&agent, cwd.clone()).await;
    send(&agent, &sid, "KEEP_WITHOUT_MODEL", MessageDelivery::Queue).await;
    let events = collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::Error { .. })
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Error { error, fatal: false, .. } if matches!(
                error.semantic_error(),
                pig_protocol::CoreError::NoModelConfigured
            )
        )),
        "the failure must be typed and nonfatal: {events:#?}"
    );
    assert!(
        queue_items(&events)
            .iter()
            .any(|item| item.text == "KEEP_WITHOUT_MODEL"),
        "the unsent message stays visible in the queue: {events:#?}"
    );
    agent.shutdown();

    // A later start with a working model restores the message PAUSED and can
    // run it on an explicit resume.
    std::fs::write(&config_path, working_config).unwrap();
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    agent
        .ops
        .send(Op::OpenSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let mut restored = false;
    let mut paused = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !(restored && paused) {
        assert!(Instant::now() < deadline, "restore did not complete");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::QueueChanged { items, .. }
                if items.iter().any(|item| item.text == "KEEP_WITHOUT_MODEL") =>
            {
                restored = true;
            }
            Event::QueueDrainChanged {
                auto_drain: false,
                reason: Some(pig_protocol::QueuePauseReason::Restarted),
                ..
            } => paused = true,
            Event::UserMessage { text, .. } if text == "KEEP_WITHOUT_MODEL" => {
                panic!("the restored queue must not auto-run")
            }
            _ => {}
        }
    }
    agent
        .ops
        .send(Op::ResumeQueue {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let mut ran = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            Instant::now() < deadline,
            "the restored message did not run"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::UserMessage { text, .. } if text == "KEEP_WITHOUT_MODEL" => ran = true,
            Event::TurnComplete { .. } if ran => break,
            _ => {}
        }
    }
    agent.shutdown();
}

fn event_errors(events: &[Event]) -> Vec<&pig_protocol::CoreError> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Error { error, .. } => Some(error),
            _ => None,
        })
        .collect()
}

/// A finished background child schedules a notification, but a paused queue
/// stays paused until an explicit resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_notification_does_not_unpause_a_paused_queue() {
    let (config_path, cwd, data_dir) = setup("queue-wake-paused");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    // Background child whose Write waits for approval: we hold that approval
    // to make the child's completion arrive exactly when the queue is paused.
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} WRITEBG", mock::SUBAGENT_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let mut parent_done = false;
    let mut child_approval: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(25);
    while !parent_done || child_approval.is_none() {
        assert!(
            Instant::now() < deadline,
            "background child setup did not complete"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::TurnComplete { .. } => parent_done = true,
            Event::ApprovalRequested {
                request_id, tool, ..
            } if tool == "Write" && request_id.starts_with(&format!("{sid}-")) => {
                child_approval.get_or_insert_with(|| request_id.clone());
            }
            _ => {}
        }
    }
    let child_approval = child_approval.expect("child write approval");

    // Pause a non-empty queue. The question scenario blocks on AskUserQuestion,
    // a deterministic mid-turn window; it also keeps later mock dispatch on the
    // question branch (the aborted call is in history), so the wake turn ends
    // as plain text.
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} wake", mock::SCENARIO_Q_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::QuestionRequested { .. })
    })
    .await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: "ECHO_HISTORY WAKE_BACKLOG".to_string(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let queued_events = collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 1),
    )
    .await;
    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: false,
                ..
            }
        )
    })
    .await;

    // The terminal notification uses its own durable queue while paused.
    agent
        .ops
        .send(Op::ApprovalReply {
            request_id: child_approval,
            decision: ApprovalDecision::Allow,
            feedback: None,
        })
        .await
        .unwrap();
    let mut user_items = queue_items(&queued_events);
    let mut pending_notifications = Vec::new();
    let mut auto_drain_flipped = false;
    let mut started_during_pause = false;
    let mut event_trace = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            Instant::now() < deadline,
            "the notification never entered its queue: {pending_notifications:?}; user_items={user_items:?}; event_trace={event_trace:#?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        event_trace.push(format!("{event:?}"));
        match &event {
            Event::QueueChanged { items, .. } => user_items = items.clone(),
            Event::TaskNotificationQueueChanged { notifications, .. } => {
                pending_notifications = notifications.clone();
            }
            Event::QueueDrainChanged {
                auto_drain: true, ..
            } => auto_drain_flipped = true,
            Event::TurnStarted { .. } => started_during_pause = true,
            _ => {}
        }
        if user_items.len() == 1 && pending_notifications.len() == 1 {
            break;
        }
    }
    assert_eq!(user_items.len(), 1, "the user queue is independent");
    assert_eq!(pending_notifications.len(), 1, "one terminal notification");
    assert!(
        !auto_drain_flipped,
        "a notification must not unpause the queue"
    );
    assert!(
        !started_during_pause,
        "a notification must not start a turn while paused"
    );

    // The real user turn comes first and carries the notification at its first
    // safe boundary instead of paying for a separate notification turn.
    agent
        .ops
        .send(Op::ResumeQueue {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let mut backlog_ran = false;
    let mut notified = false;
    let mut turn_starts = 0usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "the resumed queue did not deliver the notification"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::TurnStarted { .. } => turn_starts += 1,
            Event::UserMessage { text, .. } if text.contains("WAKE_BACKLOG") => {
                backlog_ran = true;
            }
            Event::TaskNotificationBatch { notifications, .. } => {
                assert!(backlog_ran, "the user message must be admitted first");
                assert_eq!(notifications.len(), 1);
                notified = true;
            }
            Event::TurnComplete { .. } if notified => break,
            _ => {}
        }
    }
    assert_eq!(turn_starts, 1, "the notification rides the user turn");
    agent.shutdown();
}

/// A held message is persisted with the queue: a restart restores it into the
/// paused queue lane (no prompt owner survives), so an app close cannot lose a
/// message that was waiting for its disposition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_message_persists_across_restart() {
    let (config_path, cwd, data_dir) = setup("queue-held-restart");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let sid = new_session(&agent, cwd.clone()).await;

    let outside = std::env::temp_dir()
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/"))
        .join(format!("pig-held-restart-{}.txt", std::process::id()));
    let outside = outside.display().to_string().replace('\\', "/");
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} {}", mock::SCENARIO_OUTSIDE_WRITE_TRIGGER, outside),
            files: vec![],
            images: vec![],
            permissions: ExecMode::ReadOnly.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .unwrap();
    recv_until(
        &agent.events,
        Duration::from_secs(10),
        |e| matches!(e, Event::ApprovalRequested { tool, .. } if tool == "Write"),
    )
    .await;
    send(
        &agent,
        &sid,
        "ECHO_HISTORY OLD_RESTART",
        MessageDelivery::Queue,
    )
    .await;
    collect(
        &agent,
        Duration::from_secs(10),
        |e| matches!(e, Event::QueueChanged { items, .. } if items.len() == 1),
    )
    .await;
    agent
        .ops
        .send(Op::Interrupt {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::QueueDrainChanged {
                auto_drain: false,
                ..
            }
        )
    })
    .await;
    send(
        &agent,
        &sid,
        "ECHO_HISTORY HELD_RESTART",
        MessageDelivery::Queue,
    )
    .await;
    collect(&agent, Duration::from_secs(10), |e| {
        matches!(e, Event::HeldQueuePrompt { .. })
    })
    .await;
    agent.shutdown();

    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    agent
        .ops
        .send(Op::OpenSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let mut restored = false;
    let mut paused = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    while !(restored && paused) {
        assert!(Instant::now() < deadline, "held restore did not complete");
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        match &event {
            Event::QueueChanged { items, .. }
                if items.iter().any(|item| item.text.contains("OLD_RESTART"))
                    && items.iter().any(|item| item.text.contains("HELD_RESTART")) =>
            {
                restored = true;
            }
            Event::QueueDrainChanged {
                auto_drain: false,
                reason: Some(pig_protocol::QueuePauseReason::Restarted),
                ..
            } => paused = true,
            Event::UserMessage { text, .. }
                if text.contains("OLD_RESTART") || text.contains("HELD_RESTART") =>
            {
                panic!("the restored queue must not auto-run")
            }
            _ => {}
        }
    }

    agent
        .ops
        .send(Op::ResumeQueue {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let mut ran = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while ran.len() < 2 {
        assert!(
            Instant::now() < deadline,
            "restored held queue did not drain: {ran:?}"
        );
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_secs(2), agent.events.recv()).await
        else {
            continue;
        };
        if let Event::UserMessage { text, .. } = &event
            && (text.contains("OLD_RESTART") || text.contains("HELD_RESTART"))
        {
            ran.push(text.clone());
        }
    }
    assert_eq!(ran.len(), 2);
    agent.shutdown();
}
