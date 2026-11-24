use std::time::Duration;

use pig_protocol::{ApiFormat, Event, Op};
use pig_provider::mock;

use crate::common;
use crate::fixtures::{compact_finishes, config, send_and_complete, workspace};
use crate::server::{ServerMode, dir_path, new_agent_for, start_server};

/// A step that reports no usage must clear the anchor: otherwise the stale
/// previous `used` is attached to the new history length and the large tool
/// result appended by that step never enters the preflight projection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_usage_clears_the_anchor_before_a_large_tool_result() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = workspace("missing-usage");
    for name in [mock::MISSING_USAGE_FILE_A, mock::MISSING_USAGE_FILE_B] {
        let mut content = String::new();
        for _ in 0..400 {
            content.push_str(&"a".repeat(100));
            content.push('\n');
        }
        std::fs::write(dir.join(name), content).unwrap();
    }
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        config(
            &format!("http://127.0.0.1:{port}/v1"),
            ApiFormat::OpenAiChat,
            // trigger = 128000 - 100000 - 13000 = 15000.
            128_000,
            100_000,
        ),
    )
    .unwrap();
    let data_dir = dir.join("data");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    let session_id = common::new_session(&agent, dir.clone()).await;
    // An older segment, without usage (the anchor starts empty).
    send_and_complete(&agent, &session_id, "ECHO_HISTORY prelude").await;

    let events =
        send_and_complete(&agent, &session_id, mock::MISSING_USAGE_AFTER_FIRST_TRIGGER).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::ContextCompacted {
                automatic: true,
                ..
            }
        )),
        "the missing-usage step must not hide the large tool result: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the turn still completes: {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Error { .. })),
        "{events:#?}"
    );
    let bodies = log.lock().unwrap().clone();
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("Earlier context compacted")),
        "the retried request carries the compact note"
    );
    let trace = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.model-io.jsonl")),
    )
    .unwrap();
    let had_usage_free_tool_step = trace
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|record| {
            record["finish"] == "tool_calls" && record["usage"]["used"].as_u64() == Some(0)
        });
    assert!(
        had_usage_free_tool_step,
        "the mock must have emitted the usage-less tool step: {trace}"
    );
    agent.shutdown();
}

/// The usage anchor plus the messages appended after the previous response
/// (a large tool result) triggers compaction before the next sample.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_after_a_large_tool_result_compacts_before_sampling() {
    let server = start_server(ServerMode::Preflight);
    let (agent, session_id, _config, _dir, data_dir) =
        new_agent_for(&server, "preflight", 32_000, 1_000).await;
    // 6k-token older segment; keeps turn-start projection below the 18k trigger.
    let older = format!("A_OLD_SEGMENT {}", "a".repeat(24_000));
    send_and_complete(&agent, &session_id, &older).await;
    // Step 1 reports used=16000; the ~4.5k-token tool result appended after it
    // pushes the projected request over the trigger at step 2. Short lines keep
    // Read from applying its per-line truncation.
    std::fs::write(
        dir_path(&data_dir).join(mock::MOCK_FILE_NAME),
        "tttt\n".repeat(2_000),
    )
    .unwrap();
    let events =
        send_and_complete(&agent, &session_id, "PREFLIGHT_SCENARIO read the mock file").await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::ContextCompacted {
                automatic: true,
                ..
            }
        )),
        "preflight compacted before the next sample: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TextDone { .. })),
        "the turn completes after the preflight: {events:#?}"
    );
    let bodies = server.bodies.lock().unwrap().clone();
    let streaming: Vec<&String> = bodies
        .iter()
        .filter(|body| body.contains("\"stream\":true"))
        .collect();
    let after_compact = streaming
        .iter()
        .rev()
        .find(|body| body.contains("Earlier context compacted"))
        .expect("a request carrying the compact note");
    assert!(
        after_compact.contains("\"role\":\"tool\"") && after_compact.contains("tttt"),
        "the kept newest segment still carries the tool result: {after_compact}"
    );
    assert!(
        !after_compact.contains("A_OLD_SEGMENT"),
        "the older segment was omitted"
    );
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    assert_eq!(
        rollout.matches("\"type\":\"tool_execution_start\"").count(),
        1,
        "retaining the tool result avoids repeating the Read: {rollout}"
    );
    agent.shutdown();
}

/// One user segment with one assistant/tool round: the preflight compacts
/// inside the same turn, preserves the request verbatim and starts the retry
/// from the reduced context (no second user/TurnStarted).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_segment_preflight_compacts_inside_the_turn() {
    let server = start_server(ServerMode::SingleSegmentPreflight);
    let (agent, session_id, _config, _dir, data_dir) =
        new_agent_for(&server, "preflight-single", 31_000, 1_000).await;
    // The Read result is ~5k tokens: 16k step usage + growth crosses the 17k
    // trigger at the next step. Short lines avoid Read's per-line truncation.
    std::fs::write(
        dir_path(&data_dir).join(mock::MOCK_FILE_NAME),
        "tttt\n".repeat(5_000),
    )
    .unwrap();
    let events = send_and_complete(
        &agent,
        &session_id,
        "PREFLIGHT_SINGLE_SCENARIO read the mock file",
    )
    .await;

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
        "exactly one same-turn compaction: {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Error { .. })),
        "{events:#?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::TextDone { .. }))
            .count(),
        1,
        "the turn continues to one answer: {events:#?}"
    );

    let bodies = server.bodies.lock().unwrap().clone();
    let retried = bodies
        .iter()
        .find(|body| body.contains("\"stream\":true") && body.contains("Earlier context compacted"))
        .expect("the retry request carries the compact note");
    assert!(
        retried.contains("PREFLIGHT_SINGLE_SCENARIO read the mock file"),
        "the active request is preserved verbatim: {retried}"
    );
    assert!(
        !retried.contains("\"role\":\"tool\""),
        "the oversized tool round is omitted whole: {retried}"
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
    assert_eq!(compact["omitted"], 2, "{compact}");
    assert_eq!(compact["tail_len"], 0, "{compact}");
    assert!(
        compact["preserved_user"].is_u64(),
        "the active user is preserved by index: {compact}"
    );

    let continued = send_and_complete(&agent, &session_id, "continue after single preflight").await;
    assert!(
        continued
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the session continues: {continued:#?}"
    );
    agent.shutdown();
}

/// Cancelling a running compaction reports `aborted` instead of a fake
/// completion, and the session stays usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_compaction_reports_aborted() {
    let server = start_server(ServerMode::SlowSummary);
    let (agent, session_id, _config, _dir, data_dir) =
        new_agent_for(&server, "cancel-compact", 128_000, 8_192).await;
    send_and_complete(&agent, &session_id, "first topic").await;
    send_and_complete(&agent, &session_id, "second topic").await;
    agent
        .ops
        .send(Op::Compact {
            session_id: session_id.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, Duration::from_secs(5), |event| {
        matches!(event, Event::CompactStarted { .. })
    })
    .await;
    let cancel_started = std::time::Instant::now();
    agent
        .ops
        .send(Op::Interrupt {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let events = common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::TurnAborted { .. } | Event::ContextCompacted { .. } | Event::Error { .. }
        )
    })
    .await;
    // The sidecar server sleeps five seconds before answering; cancellation is
    // observed while the request is in flight, not after it completes.
    assert!(
        cancel_started.elapsed() < Duration::from_secs(4),
        "cancellation must interrupt the sidecar promptly: {:?}",
        cancel_started.elapsed()
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnAborted { .. })),
        "cancellation emits TurnAborted: {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::ContextCompacted { .. })),
        "no compaction completion after cancel: {events:#?}"
    );
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .expect("rollout");
    assert!(
        !rollout.contains("\"type\":\"compact\""),
        "a cancelled sidecar never rewrites the rollout: {rollout}"
    );
    let compacts = compact_finishes(&data_dir, &session_id);
    assert_eq!(compacts.len(), 1);
    assert_eq!(
        compacts[0].0, "cancelled",
        "the cancelled attempt is recorded as cancelled: {compacts:?}"
    );
    let resumed = send_and_complete(&agent, &session_id, "continue after cancel").await;
    assert!(
        resumed
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the session continues: {resumed:#?}"
    );
    agent.shutdown();
}

/// Recalling a turn removes it from the model context. Its real usage sample
/// must not survive as the preflight watermark, or the next turn would compact
/// solely because of the recalled turn's stale high value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_resets_the_stale_usage_watermark() {
    let server = start_server(ServerMode::HighUsage);
    let (agent, session_id, _config, _dir, _data) =
        new_agent_for(&server, "recall-watermark", 128_000, 8_192).await;

    // A sizeable older turn so a (wrong) compaction would have work to omit.
    let old = format!("old work {}", "a".repeat(4_000));
    let first = send_and_complete(&agent, &session_id, &old).await;
    assert!(
        first
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the older turn completes: {first:#?}"
    );

    // This turn's last request reports 120000 used (trigger = 106808).
    let high = send_and_complete(&agent, &session_id, "HIGH_USAGE_SCENARIO").await;
    assert!(
        high.iter()
            .any(|event| matches!(event, Event::ContextUsage { used: 120_000, .. })),
        "the high sample crossed the compact trigger: {high:#?}"
    );

    agent
        .ops
        .send(Op::RecallLastMessage {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(event, Event::MessageRecalled { .. })
    })
    .await;

    let next = send_and_complete(&agent, &session_id, "continue after recall").await;
    assert!(
        !next.iter().any(|event| matches!(
            event,
            Event::CompactStarted { .. } | Event::ContextCompacted { .. }
        )),
        "the recalled turn's stale watermark must not trigger compaction: {next:#?}"
    );
    assert!(
        next.iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the session continues after the recall: {next:#?}"
    );
    agent.shutdown();
}
