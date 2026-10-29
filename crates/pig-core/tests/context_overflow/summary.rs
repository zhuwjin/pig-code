use std::time::Duration;

use pig_protocol::{Event, Op};

use crate::common;
use crate::fixtures::{compact_finishes, send_and_complete};
use crate::server::{ServerMode, last_was_typed_overflow, new_agent_for, start_server};

/// Content already forwarded in a failed attempt makes a transparent retry
/// unsafe: the typed error is surfaced and no compaction runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwarded_content_prevents_the_reactive_retry() {
    let server = start_server(ServerMode::ForwardedOverflow);
    let (agent, session_id, _config, _dir, _data) =
        new_agent_for(&server, "forwarded", 128_000, 8_192).await;
    let events = send_and_complete(&agent, &session_id, "stream part then overflow").await;
    assert!(
        last_was_typed_overflow(&events),
        "the typed error surfaces: {events:#?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::ContextCompacted { .. } | Event::CompactStarted { .. }
        )),
        "no compaction after content was forwarded: {events:#?}"
    );
    let streaming_requests = server
        .bodies
        .lock()
        .unwrap()
        .iter()
        .filter(|body| body.contains("\"stream\":true"))
        .count();
    assert_eq!(
        streaming_requests, 1,
        "the step is not retried (title/connectivity one-shots are not steps)"
    );
    agent.shutdown();
}

/// Typed summary overflow: drop one more oldest complete segment and retry
/// once. The summary output reservation is capped at 2048. The prefix must
/// keep a second segment so the retry still has meaningful work to summarize.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summary_overflow_retries_after_dropping_a_segment() {
    let server = start_server(ServerMode::SummaryShrink);
    let (agent, session_id, _config, _dir, data_dir) =
        new_agent_for(&server, "summary-shrink", 128_000, 8_192).await;
    // The marker lives in the oldest segment; the retry drops that segment and
    // keeps the middle one. The newest segment is the retained tail.
    send_and_complete(
        &agent,
        &session_id,
        &format!("SUMMARY_DROP_MARKER first topic {}", "a".repeat(2_000)),
    )
    .await;
    send_and_complete(
        &agent,
        &session_id,
        &format!("middle topic {}", "b".repeat(4_000)),
    )
    .await;
    send_and_complete(&agent, &session_id, "third topic").await;
    agent
        .ops
        .send(Op::Compact {
            session_id: session_id.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let events = common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(event, Event::ContextCompacted { .. })
    })
    .await;
    let Some(Event::ContextCompacted { summary, note, .. }) = events.last() else {
        panic!("expected ContextCompacted")
    };
    assert_eq!(
        summary.as_deref(),
        Some("SERVER_SUMMARY"),
        "the retry produced the model summary: {events:#?}"
    );
    assert!(note.contains("model summary"), "{note}");

    let compacts = compact_finishes(&data_dir, &session_id);
    assert_eq!(compacts.len(), 2, "failed attempt + retry: {compacts:?}");
    assert_eq!(compacts[0].0, "error");
    assert_eq!(compacts[1].0, "stop");
    let bodies = server.bodies.lock().unwrap().clone();
    let summaries: Vec<&String> = bodies
        .iter()
        .filter(|body| body.contains("[COMPACTION]"))
        .collect();
    assert_eq!(summaries.len(), 2, "exactly one retry: {bodies:#?}");
    assert!(
        summaries[0].contains("SUMMARY_DROP_MARKER") && summaries[0].contains("middle topic"),
        "the first summary covers both omitted prefix segments"
    );
    assert!(
        !summaries[1].contains("SUMMARY_DROP_MARKER") && summaries[1].contains("middle topic"),
        "the retry dropped the oldest complete segment but kept the middle one"
    );
    assert!(
        summaries
            .iter()
            .all(|body| body.contains("\"max_tokens\":2048")),
        "summary output is capped at 2048: {summaries:#?}"
    );
    agent.shutdown();
}

/// Automatic compaction deliberately falls back to the note-only truncation
/// after its bounded summary attempts: the turn continues instead of failing
/// on a sidecar that cannot fit the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_summary_failure_falls_back_and_continues() {
    let server = start_server(ServerMode::AutoSummaryFallback);
    let (agent, session_id, _config, _dir, data_dir) =
        new_agent_for(&server, "auto-summary-fallback", 128_000, 8_192).await;
    let events = send_and_complete(
        &agent,
        &session_id,
        "AUTO_FALLBACK_SCENARIO read the mock file",
    )
    .await;
    let Some(Event::ContextCompacted {
        automatic,
        note,
        summary,
        ..
    }) = events
        .iter()
        .find(|event| matches!(event, Event::ContextCompacted { .. }))
    else {
        panic!("automatic fallback must compact: {events:#?}");
    };
    assert!(*automatic, "preflight compaction is automatic");
    assert_eq!(*summary, None, "the fallback has no model summary");
    assert!(
        note.contains("summary generation failed") && note.contains("untrusted"),
        "the truncation note is explicit: {note}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the turn continues after the automatic fallback: {events:#?}"
    );
    let compacts = compact_finishes(&data_dir, &session_id);
    assert!(
        !compacts.is_empty() && compacts.iter().all(|(finish, _)| finish == "error"),
        "bounded failed summary attempts: {compacts:?}"
    );
    agent.shutdown();
}

/// A manual summary whose retry still overflows is a failure: both sidecar
/// attempts are recorded, the history is not rewritten, and the user sees a
/// nonfatal error instead of a truncation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_summary_overflow_never_rewrites() {
    let server = start_server(ServerMode::SummaryFallback);
    let (agent, session_id, _config, _dir, data_dir) =
        new_agent_for(&server, "summary-manual-failure", 128_000, 8_192).await;
    send_and_complete(
        &agent,
        &session_id,
        &format!("first topic {}", "a".repeat(2_000)),
    )
    .await;
    // The marker lives in the middle segment, which the extra drop keeps.
    let events = send_and_complete(
        &agent,
        &session_id,
        &format!("SUMMARY_KEEP_MARKER second topic {}", "b".repeat(4_000)),
    )
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "{events:#?}"
    );
    send_and_complete(&agent, &session_id, "third topic").await;
    agent
        .ops
        .send(Op::Compact {
            session_id: session_id.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let events = common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(
            event,
            Event::TurnAborted { .. } | Event::ContextCompacted { .. }
        )
    })
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnAborted { .. })),
        "manual failure returns to idle: {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::ContextCompacted { .. })),
        "manual failure must not install a truncation: {events:#?}"
    );
    let compacts = compact_finishes(&data_dir, &session_id);
    assert_eq!(compacts.len(), 2, "one retry, both errors: {compacts:?}");
    assert_eq!(compacts[0].0, "error");
    assert_eq!(compacts[1].0, "error");
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .expect("rollout");
    assert!(
        !rollout.contains("\"type\":\"compact\""),
        "manual failure leaves no compact record: {rollout}"
    );
    let bodies = server.bodies.lock().unwrap().clone();
    let summaries: Vec<&String> = bodies
        .iter()
        .filter(|body| body.contains("[COMPACTION]"))
        .collect();
    assert_eq!(summaries.len(), 2);
    assert!(
        summaries
            .iter()
            .all(|body| body.contains("SUMMARY_KEEP_MARKER")),
        "the marker stayed in the kept prefix segment: {summaries:#?}"
    );
    agent.shutdown();
}
