//! End-to-end coverage of the repeat-call breaker: the reminders the model
//! sees and the turn the trip ends, for the serial tool path (one identical
//! call per response) and the parallel wave (three per response).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pig_protocol::{CoreError, Event, MessageDelivery, Op};
use pig_provider::mock;

/// The breaker stops a turn only after the streak outgrows this.
const STOP_AFTER: usize = 12;

async fn send(agent: &pig_core::AgentHandle, session_id: &str, text: &str) {
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.to_string(),
            content: text.to_string(),
            files: vec![],
            images: vec![],
            permissions: pig_protocol::ExecMode::Auto.permissions(),
            delivery: MessageDelivery::Queue,
        })
        .await
        .unwrap();
}

async fn run_loop(
    agent: &pig_core::AgentHandle,
    session_id: &str,
    trigger: &str,
) -> Vec<Event> {
    send(agent, session_id, trigger).await;
    common::recv_until(&agent.events, Duration::from_secs(120), |event| {
        matches!(
            event,
            Event::Error { .. } | Event::TurnComplete { .. } | Event::TurnAborted { .. }
        )
    })
    .await
}

/// Streaming requests of the looping turn, in order. The title sidecar is a
/// non-streaming call and never carries the trigger.
fn loop_requests(log: &Arc<Mutex<Vec<String>>>, trigger: &str) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|body| body.contains("\"stream\":true") && body.contains(trigger))
        .cloned()
        .collect()
}

fn breaker_detail(events: &[Event]) -> String {
    let error = events
        .iter()
        .rev()
        .find_map(|event| match event {
            Event::Error { error, fatal, .. } if *fatal => Some(error.semantic_error().clone()),
            _ => None,
        })
        .expect("the breaker ends the turn with a fatal error");
    match error {
        CoreError::Internal { detail } => detail,
        other => panic!("unexpected error kind: {other:?}"),
    }
}

fn tool_end_outputs(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCallEnd { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serial_repeats_escalate_reminders_and_trip_after_the_final_one() {
    let (config_path, cwd, data_dir, log) = common::setup_with_log("repeat-breaker-serial");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, cwd.clone()).await;

    let events = run_loop(&agent, &session_id, mock::REPEAT_LOOP_TRIGGER).await;

    // The whole batch is durable before the trip: every call ran once and
    // has its result, and the model was never sampled a 14th time.
    let outputs = tool_end_outputs(&events);
    assert_eq!(outputs.len(), STOP_AFTER + 1, "{events:#?}");
    let requests = loop_requests(&log, mock::REPEAT_LOOP_TRIGGER);
    assert_eq!(requests.len(), STOP_AFTER + 1, "{events:#?}");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Error { fatal: true, .. })),
        "{events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "a tripped turn never completes normally"
    );
    let detail = breaker_detail(&events);
    assert_eq!(
        detail,
        "Repeat breaker: `Read` was called 13 times in a row with identical arguments and ignored the reminder to stop; the turn was ended instead of looping"
    );

    // Request k (0-based) carries the results of calls 1..=k. A reminder earned
    // by call s therefore first appears in request s, and stays in history.
    let reminders = |request: usize| {
        (
            requests[request]
                .matches("state in one sentence")
                .count(),
            requests[request]
                .matches("Falsification check")
                .count(),
            requests[request]
                .matches("Write your final response now")
                .count(),
        )
    };
    assert_eq!(reminders(2), (0, 0, 0), "nothing before the third repeat");
    assert_eq!(reminders(3), (1, 0, 0), "first reminder at streak 3");
    assert_eq!(reminders(4), (2, 0, 0), "the fourth repeat earns it too");
    assert_eq!(reminders(5), (2, 1, 0), "second reminder at streak 5");
    assert_eq!(reminders(7), (2, 3, 0), "the fifth to seventh repeats carry it");
    assert_eq!(reminders(8), (2, 3, 1), "final reminder at streak 8");
    assert_eq!(
        reminders(STOP_AFTER),
        (2, 3, STOP_AFTER - 7),
        "every result from the eighth on asks for a final answer"
    );

    // What the user saw in the tool cards matches what the model was sent,
    // plus the tripping call: it runs and carries the reminder, but the model
    // is never sampled again to read it.
    assert_eq!(
        outputs
            .iter()
            .filter(|output| output.contains("Write your final response now"))
            .count(),
        STOP_AFTER - 6
    );
    assert!(
        outputs
            .iter()
            .any(|output| output.contains("A known file used by the pig-core mock provider")),
        "the repeated tool really ran: {outputs:#?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tripped_turn_does_not_leak_into_the_next_turn() {
    let (config_path, cwd, data_dir, log) = common::setup_with_log("repeat-breaker-reset");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, cwd.clone()).await;
    let first = run_loop(&agent, &session_id, mock::REPEAT_LOOP_TRIGGER).await;
    assert!(breaker_detail(&first).starts_with("Repeat breaker: `Read` was called 13 times"));

    // The next turn starts with an empty streak: an ordinary request completes
    // and samples exactly once.
    send(&agent, &session_id, "continue after the loop").await;
    let next = common::recv_until(&agent.events, Duration::from_secs(60), |event| {
        matches!(
            event,
            Event::TurnComplete { .. } | Event::TurnAborted { .. } | Event::Error { .. }
        )
    })
    .await;
    assert!(
        next.iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "{next:#?}"
    );
    assert!(
        !next.iter().any(|event| matches!(event, Event::Error { .. })),
        "{next:#?}"
    );
    let streamed = log
        .lock()
        .unwrap()
        .iter()
        .filter(|body| body.contains("\"stream\":true"))
        .count();
    assert_eq!(streamed, STOP_AFTER + 2, "{next:#?}");
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_waves_count_their_members_in_model_order() {
    let (config_path, cwd, data_dir, log) =
        common::setup_with_log("repeat-breaker-parallel");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, cwd.clone()).await;

    let events = run_loop(&agent, &session_id, mock::REPEAT_LOOP_PARALLEL_TRIGGER).await;

    // Three identical calls per response. The 13th call is the first member
    // of the fifth wave; the trip waits for the wave to be durable, so its two
    // siblings still run and get their results.
    let outputs = tool_end_outputs(&events);
    assert_eq!(outputs.len(), STOP_AFTER + 3, "{events:#?}");
    let requests = loop_requests(&log, mock::REPEAT_LOOP_PARALLEL_TRIGGER);
    assert_eq!(requests.len(), 5, "{events:#?}");
    assert!(
        breaker_detail(&events).starts_with("Repeat breaker: `Read` was called 13 times"),
        "{events:#?}"
    );

    // A wave is counted in call order whatever order its members finish in:
    // wave one holds streaks 1..=3, wave two 4..=6, and so on. The fifth wave
    // never reaches the model, so the last request shows 12 results.
    let counts = |request: usize| {
        (
            requests[request]
                .matches("state in one sentence")
                .count(),
            requests[request]
                .matches("Falsification check")
                .count(),
            requests[request]
                .matches("Write your final response now")
                .count(),
        )
    };
    assert_eq!(counts(0), (0, 0, 0));
    assert_eq!(counts(1), (1, 0, 0));
    assert_eq!(counts(2), (2, 2, 0));
    assert_eq!(counts(3), (2, 3, 2));
    assert_eq!(counts(4), (2, 3, 5));

    let with = |needle: &str| {
        outputs
            .iter()
            .filter(|output| output.contains(needle))
            .count()
    };
    assert_eq!(with("state in one sentence"), 2);
    assert_eq!(with("Falsification check"), 3);
    assert_eq!(with("Write your final response now"), 8);
    agent.shutdown();
}
