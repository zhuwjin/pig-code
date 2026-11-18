use std::time::Duration;

use pig_protocol::{ApiFormat, Event, Op};
use pig_provider::mock;

use crate::common;
use crate::fixtures::{
    config, main_finishes, open_session, seed_old_segment, send_and_complete, workspace,
};
use crate::server::{ServerMode, dir_path, new_agent_for, start_server};

// ---------- mock-server recovery across all three formats ----------

async fn recover_from_trigger(format: ApiFormat, trigger: &str, label: &str) {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = workspace(label);
    // Warm up against a harmless file, then replace it with the overflow
    // trigger; the mock's tool call reads whatever this file contains.
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        config(
            &format!("http://127.0.0.1:{port}/v1"),
            format,
            128_000,
            8_192,
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    agent.shutdown();

    // Seed an older complete segment without running a request (a marker in a
    // previous user message would otherwise keep matching later mock bodies).
    seed_old_segment(
        &data_dir,
        &session_id,
        &format!("old context that may be omitted {}", "o".repeat(50_000)),
        trigger,
    );

    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    open_session(&agent, &session_id).await;
    // The newest user message is oversized: compaction preserves it as the
    // active request and omits the older segment that carries the trigger.
    let huge = format!("Read the mock file and summarize. {}", "x".repeat(200_000));
    let events = send_and_complete(&agent, &session_id, &huge).await;

    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::TurnStarted { .. }))
            .count(),
        1,
        "{label}: no second TurnStarted"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::UserMessage { .. }))
            .count(),
        1,
        "{label}: no duplicate user message"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ContextCompacted {
                    automatic: true,
                    ..
                }
            ))
            .count(),
        1,
        "{label}: exactly one durable overflow compaction {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Error { .. })),
        "{label}: the failed attempt is not surfaced when recovery succeeds {events:#?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::TextDone { .. }))
            .count(),
        1,
        "{label}: the recovered answer appears once {events:#?}"
    );
    let finishes = main_finishes(&data_dir, &session_id);
    assert!(
        finishes.iter().any(|(finish, _)| finish == "error"),
        "{label}: failed attempt recorded: {finishes:?}"
    );
    assert_eq!(
        finishes.last().map(|(finish, _)| finish.as_str()),
        Some("stop"),
        "{label}: the turn recovers to a normal finish: {finishes:?}"
    );

    // The retried request used the compacted prefix and no longer contains the
    // trigger (the unsent tool result was omitted).
    let bodies = log.lock().unwrap().clone();
    let retried = bodies
        .iter()
        .rev()
        .find(|body| body.contains("Earlier context compacted"))
        .expect("retry request carrying the compact note");
    assert!(
        !retried.contains(trigger),
        "{label}: the trigger left the retried context"
    );

    // Restart: the compacted prefix is rebuilt from the durable record.
    agent.shutdown();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    agent
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let replay = common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::TurnComplete {
                duration_ms: 0,
                stats: None,
                ..
            }
        )
    })
    .await;
    assert!(
        replay
            .iter()
            .any(|event| matches!(event, Event::ContextCompacted { .. })),
        "{label}: replay restores the compaction divider"
    );
    let rebuilt = send_and_complete(&agent, &session_id, "continue after restart").await;
    assert!(
        rebuilt
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "{label}: the restarted session continues"
    );
    let bodies = log.lock().unwrap().clone();
    let continued = bodies
        .iter()
        .rev()
        .find(|body| body.contains("continue after restart"))
        .expect("post-restart request");
    assert!(
        continued.contains("Earlier context compacted"),
        "{label}: the restarted prefix keeps the compact note"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_overflow_recovers_once_in_all_formats() {
    for (format, label) in [
        (ApiFormat::OpenAiChat, "chat-stream"),
        (ApiFormat::OpenAiResponses, "responses-stream"),
        (ApiFormat::AnthropicMessages, "anthropic-stream"),
    ] {
        recover_from_trigger(format, mock::CONTEXT_OVERFLOW_STREAM_TRIGGER, label).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_overflow_recovers_once_in_all_formats() {
    for (format, label) in [
        (ApiFormat::OpenAiChat, "chat-http"),
        (ApiFormat::OpenAiResponses, "responses-http"),
        (ApiFormat::AnthropicMessages, "anthropic-http"),
    ] {
        recover_from_trigger(format, mock::CONTEXT_OVERFLOW_HTTP_TRIGGER, label).await;
    }
}

/// A single user segment with one assistant/tool round recovers from a typed
/// overflow: the tool round is omitted whole, the request is preserved, and
/// the retried request no longer carries the trigger.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_assistant_round_overflow_recovers_once() {
    let server = start_server(ServerMode::SingleRoundOverflow);
    let (agent, session_id, config, dir, data_dir) =
        new_agent_for(&server, "single-round-overflow", 128_000, 8_192).await;
    // The trigger lives only in the tool result (the Read of this file).
    std::fs::write(
        dir_path(&data_dir).join(mock::MOCK_FILE_NAME),
        format!(
            "{}\n{}",
            mock::CONTEXT_OVERFLOW_STREAM_TRIGGER,
            "payload line for the overflow test\n".repeat(2_000)
        ),
    )
    .unwrap();
    let events = send_and_complete(&agent, &session_id, "read the mock file").await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::TurnStarted { .. }))
            .count(),
        1,
        "no second TurnStarted: {events:#?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::UserMessage { .. }))
            .count(),
        1,
        "no duplicate user message: {events:#?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ContextCompacted {
                    automatic: true,
                    ..
                }
            ))
            .count(),
        1,
        "one durable recovery compaction: {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Error { .. })),
        "the failed attempt is recovered: {events:#?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::TextDone { .. }))
            .count(),
        1,
        "the recovered answer appears once: {events:#?}"
    );

    let finishes = main_finishes(&data_dir, &session_id);
    assert!(
        finishes.iter().any(|(finish, _)| finish == "error"),
        "the overflow attempt is recorded: {finishes:?}"
    );
    assert_eq!(
        finishes.last().map(|(finish, _)| finish.as_str()),
        Some("stop"),
        "the turn recovers to a normal finish: {finishes:?}"
    );
    let compacts = crate::fixtures::compact_finishes(&data_dir, &session_id);
    assert_eq!(compacts.len(), 1, "one summary sidecar: {compacts:?}");
    assert_eq!(compacts[0].0, "stop");

    let bodies = server.bodies.lock().unwrap().clone();
    let retried = bodies
        .iter()
        .find(|body| body.contains("\"stream\":true") && body.contains("Earlier context compacted"))
        .expect("the retry request carries the compact note");
    assert!(
        retried.contains("read the mock file"),
        "the original request is preserved: {retried}"
    );
    assert!(
        !retried.contains(mock::CONTEXT_OVERFLOW_STREAM_TRIGGER),
        "the trigger left the retried context: {retried}"
    );
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    let compact: serde_json::Value = serde_json::from_str(
        rollout
            .lines()
            .find(|line| line.contains("\"type\":\"compact\""))
            .expect("compact record"),
    )
    .unwrap();
    assert!(
        compact["preserved_user"].is_u64(),
        "the active user is preserved by index: {compact}"
    );
    assert_eq!(compact["tail_len"], 0, "{compact}");

    // Restart rebuilds the same reduced context (system + note + user) and
    // continues.
    agent.shutdown();
    let agent = pig_core::spawn_agent_with_data_dir(Some(config), dir.clone(), data_dir.clone());
    open_session(&agent, &session_id).await;
    let continued = send_and_complete(&agent, &session_id, "continue after restart").await;
    assert!(
        continued
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the restarted session continues: {continued:#?}"
    );
    let bodies = server.bodies.lock().unwrap().clone();
    let restarted = bodies
        .iter()
        .rev()
        .find(|body| body.contains("continue after restart"))
        .expect("post-restart request");
    assert!(
        restarted.contains("Earlier context compacted") && restarted.contains("read the mock file"),
        "the restarted prefix keeps the note and the preserved user: {restarted}"
    );
    agent.shutdown();
}

/// A compaction whose note is bigger than the only omitted work grows the
/// estimated context. The existing shrink guard must surface the typed error
/// instead of retrying the same step with a larger context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_growing_compaction_surfaces_the_overflow_instead_of_retrying() {
    let server = start_server(ServerMode::OverflowGrowth);
    let (agent, session_id, _config, _dir, data_dir) =
        new_agent_for(&server, "overflow-growth", 128_000, 8_192).await;
    // The trigger is the tiny tool result: the omitted round is far smaller
    // than the summary returned for it.
    std::fs::write(
        dir_path(&data_dir).join(mock::MOCK_FILE_NAME),
        mock::CONTEXT_OVERFLOW_STREAM_TRIGGER,
    )
    .unwrap();
    let events = send_and_complete(&agent, &session_id, "read the tiny file").await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::ContextCompacted {
                automatic: true,
                ..
            }
        )),
        "the failed attempt triggers the durable compaction: {events:#?}"
    );
    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Error { error, .. } => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "the typed error reaches the UI once: {events:#?}"
    );
    assert!(
        matches!(
            errors[0],
            pig_protocol::CoreError::ContextWindowExceeded { .. }
        ),
        "the final error is typed: {:?}",
        errors[0]
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::TextDone { .. })),
        "no fabricated answer: {events:#?}"
    );
    let bodies = server.bodies.lock().unwrap().clone();
    let streaming = bodies
        .iter()
        .filter(|body| body.contains("\"stream\":true"))
        .count();
    assert_eq!(
        streaming, 2,
        "only the tool step and the failed attempt; a growing compaction must not retry: {bodies:#?}"
    );
    let finishes = main_finishes(&data_dir, &session_id);
    assert_eq!(
        finishes.last().map(|(finish, _)| finish.as_str()),
        Some("error"),
        "the turn ends with the typed error: {finishes:?}"
    );
    let compacts = crate::fixtures::compact_finishes(&data_dir, &session_id);
    assert_eq!(compacts.len(), 1, "one summary sidecar: {compacts:?}");
    assert_eq!(compacts[0].0, "stop");
    agent.shutdown();
}

/// A second overflow in the same turn is not recovered again: the turn already
/// spent its one durable compaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_overflow_surfaces_the_typed_error() {
    let (port, _log) = mock::start_mock_server_with_log();
    let dir = workspace("second");
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        config(
            &format!("http://127.0.0.1:{port}/v1"),
            ApiFormat::OpenAiChat,
            128_000,
            8_192,
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    // A large older segment the compaction may omit.
    let old = "y".repeat(200_000);
    send_and_complete(&agent, &session_id, &old).await;

    let events =
        send_and_complete(&agent, &session_id, mock::CONTEXT_OVERFLOW_STREAM_TRIGGER).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ContextCompacted {
                    automatic: true,
                    ..
                }
            ))
            .count(),
        1,
        "one compaction before the retry: {events:#?}"
    );
    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Error { error, .. } => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "only the final error reaches the UI");
    assert!(
        matches!(
            errors[0],
            pig_protocol::CoreError::ContextWindowExceeded { .. }
        ),
        "the final error is typed: {:?}",
        errors[0]
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::TextDone { .. })),
        "the turn does not fabricate an answer: {events:#?}"
    );
    let finishes = main_finishes(&data_dir, &session_id);
    let last_two = &finishes[finishes.len() - 2..];
    assert_eq!(last_two[0].0, "error");
    assert_eq!(last_two[1].0, "error");
    agent.shutdown();
}

/// No completed assistant/tool work to omit (the trigger is the only user
/// message): the reactively typed error is surfaced without a fake compaction
/// event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_compactable_prefix_surfaces_the_error() {
    let (port, _log) = mock::start_mock_server_with_log();
    let dir = workspace("no-prefix");
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        config(
            &format!("http://127.0.0.1:{port}/v1"),
            ApiFormat::OpenAiChat,
            128_000,
            8_192,
        ),
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
    let session_id = common::new_session(&agent, dir).await;
    let events =
        send_and_complete(&agent, &session_id, mock::CONTEXT_OVERFLOW_STREAM_TRIGGER).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::CompactStarted { .. })),
        "nothing to compact: {events:#?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Error {
                error: pig_protocol::CoreError::ContextWindowExceeded { .. },
                ..
            }
        )),
        "typed error reaches the UI: {events:#?}"
    );
    agent.shutdown();
}

/// `FinishReason::Length` is an incomplete response, not an overflow: it must
/// never trigger compaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn length_finish_is_not_treated_as_overflow() {
    let (port, _log) = mock::start_mock_server_with_log();
    let dir = workspace("length");
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        config(
            &format!("http://127.0.0.1:{port}/v1"),
            ApiFormat::OpenAiChat,
            128_000,
            8_192,
        ),
    )
    .unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
    let session_id = common::new_session(&agent, dir).await;
    let events = send_and_complete(&agent, &session_id, mock::FINISH_LENGTH_TRIGGER).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::ContextCompacted { .. } | Event::Error { .. })),
        "length is not an overflow: {events:#?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::TurnComplete {
                finish_reason: Some(pig_protocol::FinishReason::Length),
                ..
            }
        )),
        "the incomplete reason is reported: {events:#?}"
    );
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overflow_after_a_side_effect_never_replays_the_tool() {
    let server = start_server(ServerMode::SideEffectOverflow);
    let (agent, session_id, config, dir, data_dir) =
        new_agent_for(&server, "side-effect-overflow", 128_000, 8_192).await;
    agent.shutdown();
    seed_old_segment(
        &data_dir,
        &session_id,
        "old context to compact",
        mock::CONTEXT_OVERFLOW_STREAM_TRIGGER,
    );
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config.clone()), dir.clone(), data_dir.clone());
    open_session(&agent, &session_id).await;

    let events = send_and_complete(&agent, &session_id, "write the overflow fixture once").await;
    let target = dir.join("overflow-side-effect.txt");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_else(|error| panic!(
            "first Write result at {}: {error}; {events:#?}",
            target.display()
        )),
        "written once"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ToolCallEnd {
                    is_error: false,
                    ..
                }
            ))
            .count(),
        1,
        "the Write must execute exactly once: {events:#?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::ContextCompacted {
                    automatic: true,
                    ..
                }
            ))
            .count(),
        1,
        "one recovery compaction: {events:#?}"
    );
    let request_shapes = server
        .bodies
        .lock()
        .expect("server requests")
        .iter()
        .map(|body| {
            (
                body.contains("\"stream\":true"),
                body.contains(mock::CONTEXT_OVERFLOW_STREAM_TRIGGER),
                body.contains("call_srv_1"),
                body.contains("Earlier context compacted"),
                body.contains("\"role\":\"tool\""),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Error { .. })),
        "recovery succeeds without a terminal error; request shapes: {request_shapes:?}"
    );
    let rollout_path = data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let rollout = std::fs::read_to_string(&rollout_path).expect("rollout after recovery");
    assert_eq!(
        rollout.matches("\"type\":\"tool_execution_start\"").count(),
        1,
        "overflow retry must not start the Write again: {rollout}"
    );

    std::fs::write(&target, "external-update").expect("external post-recovery update");
    agent.shutdown();
    let agent = pig_core::spawn_agent_with_data_dir(Some(config), dir.clone(), data_dir.clone());
    open_session(&agent, &session_id).await;
    send_and_complete(&agent, &session_id, "continue without replaying tools").await;
    assert_eq!(
        std::fs::read_to_string(&target).expect("external update survives replay"),
        "external-update"
    );
    let rollout = std::fs::read_to_string(&rollout_path).expect("rollout after restart");
    assert_eq!(
        rollout.matches("\"type\":\"tool_execution_start\"").count(),
        1,
        "restart and continuation must preserve the single execution boundary"
    );
    agent.shutdown();
}
