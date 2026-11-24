//! End-to-end coverage of steer preemption: a message delivered to the steer
//! lane while the model is still streaming abandons that request at once
//! instead of waiting for the step boundary. What the user had already read
//! is kept as the assistant's message. Once tool-call input has started, a
//! steer waits for the boundary as before.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pig_protocol::{Event, MessageDelivery, Op};
use pig_provider::mock;

const STEER: &str = "STEER_TEXT_NOW";

type RequestLog = Arc<Mutex<Vec<String>>>;

async fn send(
    agent: &pig_core::AgentHandle,
    session_id: &str,
    text: &str,
    delivery: MessageDelivery,
) {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: text.to_string(),
            files: vec![],
            images: vec![],
            permissions: pig_protocol::ExecMode::Auto.permissions(),
            delivery,
        })
        .await
        .unwrap();
}

async fn turn_end(agent: &pig_core::AgentHandle) -> Vec<Event> {
    common::recv_until(&agent.events, Duration::from_secs(60), |event| {
        matches!(
            event,
            Event::Error { .. } | Event::TurnComplete { .. } | Event::TurnAborted { .. }
        )
    })
    .await
}

/// Whatever arrives during `window`; never waits for a particular event.
async fn drain(events: &async_channel::Receiver<Event>, window: Duration) -> Vec<Event> {
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
        collected.push(event);
    }
    collected
}

fn position(events: &[Event], is: impl Fn(&Event) -> bool) -> Option<usize> {
    events.iter().position(|event| is(event))
}

fn is_steer(event: &Event) -> bool {
    matches!(event, Event::UserMessage { text, .. } if text == STEER)
}

fn streaming_requests(log: &RequestLog) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|body| body.contains("\"stream\":true"))
        .cloned()
        .collect()
}

/// The `(role, text content)` turns of a Chat Completions request body, in
/// order. Parsed rather than matched as text: the wire's key order is not
/// part of what these tests assert.
fn request_messages(body: &str) -> Vec<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(body).expect("the request is JSON");
    value["messages"]
        .as_array()
        .expect("the request carries messages")
        .iter()
        .map(|message| {
            let content = match &message["content"] {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            (
                message["role"].as_str().unwrap_or_default().to_string(),
                content,
            )
        })
        .collect()
}

async fn open_held_stream(
    agent: &pig_core::AgentHandle,
    session_id: &str,
    trigger: &str,
    stall: &mut mock::StallObserver,
    first_frame: fn(&Event) -> bool,
) {
    send(agent, session_id, trigger, MessageDelivery::Queue).await;
    assert!(
        stall.wait_stall_started(Duration::from_secs(10)).await,
        "the mock never held the stream"
    );
    common::recv_until(&agent.events, Duration::from_secs(10), first_frame).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_abandons_the_streaming_request_and_keeps_what_was_said() {
    let (port, log, mut stall) = mock::start_mock_server_with_stall_observer();
    let (config_path, cwd, data_dir) = common::setup_on("steer-preempt", port);
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, cwd.clone()).await;
    open_held_stream(
        &agent,
        &session_id,
        mock::STEER_STALL_TRIGGER,
        &mut stall,
        |event| matches!(event, Event::TextDelta { .. }),
    )
    .await;

    send(&agent, &session_id, STEER, MessageDelivery::Steer).await;
    let injected = common::recv_until(&agent.events, Duration::from_secs(10), is_steer).await;

    // The first response is still being held: the steer could only get in
    // because the request was abandoned for it.
    assert!(
        stall.is_stalled(),
        "the held response must still be open when the steer is injected"
    );
    let committed = injected.iter().find_map(|event| match event {
        Event::TextDone { full_text, .. } => Some(full_text.clone()),
        _ => None,
    });
    assert_eq!(
        committed.as_deref(),
        Some(mock::STEER_STALL_TEXT),
        "what the user had read is kept as the assistant's message: {injected:#?}"
    );
    assert!(
        position(&injected, |event| matches!(event, Event::TextDone { .. }))
            < position(&injected, is_steer),
        "the kept message precedes the steer: {injected:#?}"
    );
    assert!(
        !injected.iter().any(|event| matches!(
            event,
            Event::TurnAborted { .. } | Event::TurnComplete { .. } | Event::Error { .. }
        )),
        "nothing was interrupted: {injected:#?}"
    );
    // Injected exactly once: the drain reports the lane empty right after the
    // message itself (it is emitted after the UserMessage, so wait for it).
    let lane = common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(event, Event::SteerChanged { .. })
    })
    .await;
    let items = lane.iter().rev().find_map(|event| match event {
        Event::SteerChanged { items, .. } => Some(items.clone()),
        _ => None,
    });
    assert!(
        items.is_some_and(|items| items.is_empty()),
        "the steer lane must be drained: {lane:#?}"
    );
    assert!(
        !lane.iter().any(|event| matches!(event, Event::UserMessage { .. })),
        "the steer is injected once, not once per signal: {lane:#?}"
    );

    stall.release();
    let rest = turn_end(&agent).await;
    assert!(
        rest.iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the turn continues to its answer: {rest:#?}"
    );
    assert!(
        !rest.iter().any(|event| matches!(
            event,
            Event::Error { .. } | Event::TurnAborted { .. }
        )),
        "{rest:#?}"
    );
    assert!(
        !injected
            .iter()
            .chain(&rest)
            .any(|event| format!("{event:?}").contains(mock::STEER_STALL_TAIL)),
        "the abandoned response's tail must reach neither the UI nor history"
    );

    // The model's next request saw the kept text as the assistant's turn,
    // then the steer as the user's, and never the abandoned tail.
    let requests = streaming_requests(&log);
    assert_eq!(requests.len(), 3, "{requests:#?}");
    let messages = request_messages(&requests[1]);
    let kept = messages
        .iter()
        .position(|(role, content)| {
            role == "assistant" && content == mock::STEER_STALL_TEXT
        })
        .unwrap_or_else(|| panic!("the kept message is the assistant's turn: {messages:?}"));
    let steer = messages
        .iter()
        .position(|(role, content)| role == "user" && content == STEER)
        .unwrap_or_else(|| panic!("the steer follows as the user's turn: {messages:?}"));
    assert!(kept < steer, "{messages:?}");
    assert!(
        !messages
            .iter()
            .any(|(_, content)| content.contains(mock::STEER_STALL_TAIL)),
        "{messages:?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_after_tool_input_started_waits_for_the_step_boundary() {
    let (port, _log, mut stall) = mock::start_mock_server_with_stall_observer();
    let (config_path, cwd, data_dir) = common::setup_on("steer-preempt-tool", port);
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, cwd.clone()).await;
    open_held_stream(
        &agent,
        &session_id,
        mock::STEER_TOOL_STALL_TRIGGER,
        &mut stall,
        |event| matches!(event, Event::ToolInputStart { .. }),
    )
    .await;

    send(&agent, &session_id, STEER, MessageDelivery::Steer).await;
    let quiet = drain(&agent.events, Duration::from_millis(1500)).await;

    // Half-received arguments cannot be dropped: the steer is admitted and
    // waits, and the streaming tool input is neither discarded nor answered.
    assert!(stall.is_stalled());
    assert!(
        quiet.iter().any(|event| matches!(
            event,
            Event::SteerChanged { items, .. } if items.iter().any(|item| item.text == STEER)
        )),
        "the steer is admitted to its lane: {quiet:#?}"
    );
    assert!(!quiet.iter().any(is_steer), "{quiet:#?}");
    assert!(
        !quiet
            .iter()
            .any(|event| matches!(event, Event::ToolInputDiscard { .. })),
        "{quiet:#?}"
    );

    stall.release();
    let rest = turn_end(&agent).await;
    let tool_end = position(&rest, |event| matches!(event, Event::ToolCallEnd { .. }))
        .expect("the tool call completes and runs");
    let injected = position(&rest, is_steer).expect("the steer is injected at the boundary");
    assert!(tool_end < injected, "{rest:#?}");
    assert!(
        rest.iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "{rest:#?}"
    );
    assert!(
        !rest
            .iter()
            .any(|event| matches!(event, Event::Error { .. } | Event::TurnAborted { .. })),
        "{rest:#?}"
    );
    agent.shutdown();
}
