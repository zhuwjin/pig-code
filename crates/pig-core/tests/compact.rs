use crate::common;

use common::{new_session, recv_until, setup, setup_with_log};
use pig_protocol::{Event, ExecMode, Op};
use pig_provider::mock;
use std::time::Duration;

fn send(
    events: &async_channel::Receiver<Event>,
    agent: &pig_core::AgentHandle,
    sid: &str,
    text: &str,
) {
    send_with(events, agent, sid, text, ExecMode::Auto)
}

fn send_with(
    _events: &async_channel::Receiver<Event>,
    agent: &pig_core::AgentHandle,
    sid: &str,
    text: &str,
    mode: ExecMode,
) {
    agent
        .ops
        .send_blocking(Op::SendMessage {
            session_id: sid.into(),
            content: text.into(),
            files: vec![],
            images: vec![],
            permissions: mode.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .unwrap();
}

async fn wait_turn(events: &async_channel::Receiver<Event>) -> Vec<Event> {
    recv_until(events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. } | Event::TurnAborted { .. })
    })
    .await
}

/// Model-summary compact: history is replaced by the summary, the rollout contains a compact record, and follow-up history is shorter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_summary_compact() {
    let (config_path, cwd, data_dir) = setup("m5-compact");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    // Two conversation rounds to accumulate history (7 entries)
    send(&events, &agent, &sid, "Read the mock file and summarize");
    wait_turn(&events).await;
    send(&events, &agent, &sid, "Summarize again");
    wait_turn(&events).await;

    agent
        .ops
        .send(Op::Compact {
            session_id: sid.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    // CompactStarted must arrive before ContextCompacted (pairs with the UI's "compacting" state)
    let started_pos = collected.iter().position(|e| {
        matches!(
            e,
            Event::CompactStarted {
                automatic: false,
                ..
            }
        )
    });
    assert!(
        started_pos.is_some() && started_pos < collected.len().checked_sub(1),
        "CompactStarted must arrive before ContextCompacted: {collected:#?}"
    );
    assert!(collected.iter().any(|event| matches!(
        event,
        Event::CompactStarted { run: Some(run), .. }
            if run.turn_kind == pig_protocol::TurnKind::Compact
    )));
    assert!(collected.iter().all(|event| match event {
        Event::CompactStarted { run: Some(run), .. }
        | Event::ContextCompacted { run: Some(run), .. } => {
            run.agent_id == sid && run.turn_kind == pig_protocol::TurnKind::Compact
        }
        _ => true,
    }));
    let Some(Event::ContextCompacted {
        omitted,
        note,
        automatic,
        used_before,
        used_after,
        summary,
        ..
    }) = collected.last()
    else {
        panic!("expected ContextCompacted")
    };
    // After cutting the tail back to a user boundary, 2 entries remain (window [T,A,u,A] drops the first two)
    assert_eq!(*omitted, 4);
    assert!(!automatic, "manual compact");
    // The divider's "before → after" data: the mock reports a usage sample every
    // turn, and the post-compact estimate is present (their ordering is not
    // meaningful here — the mock's 142-token samples sit below the real
    // estimate of the compacted history; ordering is covered by the auto-compact test)
    assert!(
        used_before.is_some(),
        "a real Usage sample preceded compact"
    );
    assert!(
        used_after.is_some_and(|after| after > 0),
        "post-compact estimate should be present: {used_after:?}"
    );
    // The bare summary rides along for the UI's "view summary" panel (the note embeds
    // the same text wrapped in model-facing guidance)
    let summary = summary.as_deref().expect("model summary succeeded");
    assert!(summary.contains(mock::SUMMARY_MARKER), "{summary}");
    assert!(
        !summary.contains("Earlier context compacted"),
        "the bare summary carries no note wrapper: {summary}"
    );
    assert!(
        note.contains("model summary"),
        "should use model summary: {note}"
    );
    assert!(
        note.contains(mock::SUMMARY_MARKER),
        "should contain summary text: {note}"
    );

    // The rollout should contain a compact record
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(rollout.contains("\"type\":\"compact\""), "{rollout}");
    assert!(rollout.contains(mock::SUMMARY_MARKER));
    assert!(rollout.contains("\"used_after\""), "{rollout}");
    assert!(rollout.contains("\"used_before\""), "{rollout}");
    assert!(rollout.contains("\"summary\""), "{rollout}");

    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).unwrap();
    let row = db.query_row(
        "SELECT usage_reported, input_tokens, output_tokens, request_id, response_id
         FROM api_usage WHERE session_id = ?1 AND source = 'compact'",
        rusqlite::params![sid],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        },
    );
    let (reported, input, output, request_id, response_id) = row.unwrap();
    assert_eq!((reported, input, output), (1, 500, 30));
    assert_eq!(request_id.as_deref(), Some(mock::MOCK_REQUEST_ID));
    assert_eq!(response_id.as_deref(), Some("chatcmpl-mock"));

    // Post-compact history = system + summary + 2 entries after the user boundary + new user = 5
    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    let count = collected.iter().find_map(|e| match e {
        Event::TextDone { full_text, .. } => full_text
            .strip_prefix("HISTORY_COUNT:")
            .and_then(|n| n.parse::<usize>().ok()),
        _ => None,
    });
    assert_eq!(count, Some(5), "{collected:#?}");
    agent.shutdown();
}

/// One user turn with tool work is now compactable: the cut moves inside the
/// user segment, the original request is preserved verbatim, the tool round
/// is omitted whole, and the summary sidecar plus continuation see the same
/// reduced context that a restart rebuilds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_turn_manual_compact_preserves_the_request_and_compacts_work() {
    let (config_path, cwd, data_dir, log) = setup_with_log("compact-single-turn");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    send(&events, &agent, &sid, "Read the mock file and summarize");
    wait_turn(&events).await;

    agent
        .ops
        .send(Op::Compact {
            session_id: sid.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::CompactStarted { .. })),
        "the summary sidecar runs for a single user segment: {collected:#?}"
    );
    let Some(Event::ContextCompacted {
        omitted,
        note,
        summary,
        automatic,
        ..
    }) = collected.last()
    else {
        panic!("expected ContextCompacted: {collected:#?}")
    };
    assert_eq!(
        *omitted, 3,
        "the tool round and older turn work are omitted"
    );
    assert!(!*automatic, "manual compact");
    let summary = summary.as_deref().expect("the summary succeeded");
    assert!(summary.contains(mock::SUMMARY_MARKER), "{summary}");
    assert!(note.contains("model summary"), "{note}");

    // The conservative split persists only an index for the active user (no
    // duplicated text/media) and keeps the replay math consistent.
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl"))).unwrap();
    let compact: serde_json::Value = serde_json::from_str(
        rollout
            .lines()
            .find(|line| line.contains("\"type\":\"compact\""))
            .expect("compact record"),
    )
    .unwrap();
    assert_eq!(compact["preserved_user"], 1, "{compact}");
    let tail_len = compact["tail_len"].as_u64().expect("tail_len");
    assert_eq!(
        compact["omitted"].as_u64().expect("omitted") + tail_len + 2,
        5,
        "the pre-compact history was system + user + call + result + answer: {compact}"
    );

    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM api_usage WHERE session_id = ?1 AND source = 'compact'",
            rusqlite::params![sid],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "one summary request was made");

    // The original request and the note survive; the omitted tool round and
    // its answer do not leak back into the model context.
    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    let count = collected.iter().find_map(|e| match e {
        Event::TextDone { full_text, .. } => full_text
            .strip_prefix("HISTORY_COUNT:")
            .and_then(|n| n.parse::<usize>().ok()),
        _ => None,
    });
    assert_eq!(
        count,
        Some(4),
        "note + preserved user + the new turn: {collected:#?}"
    );
    let bodies = log.lock().unwrap().clone();
    let after = bodies
        .iter()
        .rev()
        .find(|body| body.contains("ECHO_HISTORY"))
        .expect("post-compact request");
    assert!(
        after.contains("Read the mock file and summarize"),
        "the active real request is preserved verbatim: {after}"
    );
    assert!(
        after.contains("Earlier context compacted"),
        "the compact note anchors the reduced context: {after}"
    );
    assert!(
        !after.contains(mock::MOCK_REPLY_MARKER) && !after.contains("A known file used by"),
        "the omitted tool work is gone: {after}"
    );
    agent.shutdown();
}

/// An empty session has nothing to compact: the manual compact reports the
/// skip without starting a sidecar or rewriting history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_work_manual_compact_is_a_noop() {
    let (config_path, cwd, data_dir) = setup("compact-no-work");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::Compact {
            session_id: sid.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    let Some(Event::ContextCompacted {
        omitted,
        note,
        summary,
        automatic,
        ..
    }) = collected.last()
    else {
        panic!("expected ContextCompacted: {collected:#?}")
    };
    assert_eq!(*omitted, 0);
    assert!(!*automatic, "manual compact");
    assert_eq!(*summary, None, "skip must not claim a model summary");
    assert!(
        note.contains("no earlier completed work"),
        "the skip explains why: {note}"
    );
    assert!(
        !collected
            .iter()
            .any(|e| matches!(e, Event::CompactStarted { .. })),
        "a skipped manual compact never starts a sidecar: {collected:#?}"
    );

    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM api_usage WHERE session_id = ?1 AND source = 'compact'",
            rusqlite::params![sid],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "no summary request was made");
    agent.shutdown();
}

/// Auto compact: usage exceeds the threshold → automatically summarized before the next sampling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_compact_on_high_usage() {
    let (config_path, cwd, data_dir) = setup("m5-auto");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    // One tool-chain turn first to accumulate history (system + 4 entries)
    send(&events, &agent, &sid, "Read the mock file and summarize");
    wait_turn(&events).await;
    // mock reports usage=120000 (threshold = 128000-8192-13000 = 106808)
    send(&events, &agent, &sid, "ECHO_USAGE 120000");
    wait_turn(&events).await;

    // Next turn: auto compact should run before sampling (history is 7 entries > kept 4+1 at this point)
    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    let compact_pos = collected
        .iter()
        .position(|e| matches!(e, Event::ContextCompacted { automatic: true, note, .. } if note.contains(mock::SUMMARY_MARKER)));
    let complete_pos = collected
        .iter()
        .position(|e| matches!(e, Event::TurnComplete { .. }));
    assert!(compact_pos.is_some(), "should auto-compact: {collected:#?}");
    assert!(
        compact_pos < complete_pos,
        "compact must happen before TurnComplete"
    );
    // The divider's "before → after": before = the 120000 watermark that triggered the
    // auto-compact, after = the estimate of the compacted history (well below)
    let Some(Event::ContextCompacted {
        used_before,
        used_after,
        summary,
        ..
    }) = collected.get(compact_pos.expect("checked above"))
    else {
        unreachable!()
    };
    assert_eq!(*used_before, Some(120_000));
    assert!(
        used_after.is_some_and(|after| after < 120_000),
        "post-compact estimate should be well below the trigger watermark: {used_after:?}"
    );
    assert!(
        summary
            .as_deref()
            .is_some_and(|s| s.contains(mock::SUMMARY_MARKER)),
        "the bare summary should ride along for the summary panel: {summary:?}"
    );
    let stats = collected
        .iter()
        .find_map(|event| match event {
            Event::TurnComplete {
                stats: Some(stats), ..
            } => Some(*stats),
            _ => None,
        })
        .expect("compacting turn stats");
    assert!(
        stats.compact_ms > 0,
        "compaction time is recorded: {stats:?}"
    );
    agent.shutdown();
}

/// Post-compact watermark reset: ContextUsage immediately drops to an estimated
/// value; the next turn no longer triggers a repeated auto-compact from the
/// stale high watermark; restart replay re-emits the compact separator and the
/// new watermark.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_usage_resets_and_replays() {
    let (config_path, cwd, data_dir) = setup("m5-usage-reset");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd.clone()).await;

    send(&events, &agent, &sid, "Read the mock file and summarize");
    wait_turn(&events).await;
    // mock reports usage=120000 (above the 106808 threshold), raising the watermark
    send(&events, &agent, &sid, "ECHO_USAGE 120000");
    let high = wait_turn(&events).await;
    assert!(
        high.iter()
            .any(|e| matches!(e, Event::ContextUsage { used: 120000, .. })),
        "high watermark should be in effect: {high:#?}"
    );

    agent
        .ops
        .send(Op::Compact {
            session_id: sid.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    // After compact, an estimated watermark should be re-emitted immediately (far below 120000)
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::ContextUsage { used, .. } if *used < 120000)),
        "estimated usage should be re-emitted right after compact: {collected:#?}"
    );

    // Next turn: the watermark is reset, no further auto-compact may trigger
    // (the old logic would re-compact using 120000 and, with history ≤5 entries,
    // early-return while emitting an extra "history is short" ContextCompacted)
    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    assert!(
        !collected
            .iter()
            .any(|e| matches!(e, Event::ContextCompacted { .. })),
        "must not auto-compact again after watermark reset: {collected:#?}"
    );
    // This turn's real usage = the final watermark after replay (the
    // post-compact step_usage overrides used_after; the more recent real value
    // wins)
    let Some(last_used) = collected.iter().find_map(|e| match e {
        Event::ContextUsage { used, .. } => Some(*used),
        _ => None,
    }) else {
        panic!("turn should have ContextUsage: {collected:#?}")
    };
    agent.shutdown();

    // Simulate a restart: replay should re-emit the "context compacted" separator event and the post-compact watermark
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd, data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: sid.clone(),
        })
        .await
        .unwrap();
    let replay = recv_until(&events2, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextUsage { .. })
    })
    .await;
    assert!(
        replay
            .iter()
            .any(|e| matches!(e, Event::ContextCompacted { omitted: 4, .. })),
        "replay should include the compact separator event: {replay:#?}"
    );
    assert!(
        replay
            .iter()
            .any(|e| matches!(e, Event::ContextUsage { used, .. } if *used == last_used)),
        "replayed usage should be the real value {last_used} from the post-compact turn (not the pre-compact 120000): {replay:#?}"
    );
    agent2.shutdown();
}

/// A manual summary failure does not rewrite history: the user gets a
/// nonfatal error, the session continues with the exact prior context, and no
/// compact record is written (automatic compaction, not manual, owns the
/// truncation fallback).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_summary_failure_keeps_history_and_reports() {
    let (config_path, cwd, data_dir, log) = setup_with_log("m5-manual-failure");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    // Carry the FAIL_COMPACT marker in history → mock returns 500 for the summary request
    send(&events, &agent, &sid, "FAIL_COMPACT read the mock file");
    wait_turn(&events).await;
    send(&events, &agent, &sid, "One more round");
    wait_turn(&events).await;

    agent
        .ops
        .send(Op::Compact {
            session_id: sid.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::TurnAborted { .. } | Event::ContextCompacted { .. }
        )
    })
    .await;
    assert!(
        collected
            .iter()
            .any(|event| matches!(event, Event::TurnAborted { .. })),
        "a failed manual compaction clears the UI state via TurnAborted: {collected:#?}"
    );
    assert!(
        collected.iter().any(|event| matches!(
            event,
            Event::Error { fatal: false, error, .. }
                if format!("{error:?}").contains("was not changed")
        )),
        "the failure is visible: {collected:#?}"
    );
    assert!(
        !collected
            .iter()
            .any(|event| matches!(event, Event::ContextCompacted { .. })),
        "manual failure must not claim a compaction: {collected:#?}"
    );
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(
        !rollout.contains("\"type\":\"compact\""),
        "manual failure must not rewrite durable history: {rollout}"
    );

    let db = rusqlite::Connection::open(data_dir.join("store.sqlite")).unwrap();
    let row = db.query_row(
        "SELECT usage_reported, finish, http_status, request_id, error
         FROM api_usage WHERE session_id = ?1 AND source = 'compact'",
        rusqlite::params![sid],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        },
    );
    let (reported, finish, status, request_id, error) = row.unwrap();
    assert_eq!(reported, 0);
    assert_eq!(finish, "error");
    assert_eq!(status, 500);
    assert_eq!(request_id.as_deref(), Some(mock::MOCK_REQUEST_ID));
    assert!(error.is_some());

    // The session can still continue and the failed compact left no partial
    // context rewrite behind.
    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains("HISTORY_COUNT:")
        )),
        "session should continue after the manual failure: {collected:#?}"
    );
    let bodies = log.lock().unwrap().clone();
    let after = bodies
        .iter()
        .rev()
        .find(|body| body.contains("ECHO_HISTORY"))
        .expect("post-failure request");
    assert!(
        after.contains("FAIL_COMPACT read the mock file"),
        "the original context is intact: {after}"
    );
    assert!(
        !after.contains("Earlier context compacted"),
        "no truncation note was installed: {after}"
    );
    agent.shutdown();
}

/// A single-turn manual summary failure likewise leaves the tool work and the
/// active request untouched; the user sees the error and can keep working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_turn_manual_summary_failure_keeps_the_user_and_work() {
    let (config_path, cwd, data_dir, log) = setup_with_log("compact-single-manual-failure");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    send(&events, &agent, &sid, "FAIL_COMPACT read the mock file");
    wait_turn(&events).await;

    agent
        .ops
        .send(Op::Compact {
            session_id: sid.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::TurnAborted { .. } | Event::ContextCompacted { .. }
        )
    })
    .await;
    assert!(
        collected
            .iter()
            .any(|event| matches!(event, Event::TurnAborted { .. })),
        "manual failure returns to idle: {collected:#?}"
    );
    assert!(
        !collected
            .iter()
            .any(|event| matches!(event, Event::ContextCompacted { .. })),
        "no compaction divider is emitted: {collected:#?}"
    );
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(
        !rollout.contains("\"type\":\"compact\""),
        "no durable compact record: {rollout}"
    );

    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::TextDone { .. })),
        "the session continues after the failure: {collected:#?}"
    );
    let bodies = log.lock().unwrap().clone();
    let after = bodies
        .iter()
        .rev()
        .find(|body| body.contains("ECHO_HISTORY"))
        .expect("post-failure request");
    assert!(
        after.contains("FAIL_COMPACT read the mock file"),
        "the active request stays verbatim: {after}"
    );
    assert!(
        !after.contains("Earlier context compacted"),
        "no note-only fallback was installed: {after}"
    );
    agent.shutdown();
}

/// A manual compact with no resolvable model has no summary to install. It
/// must reject with a typed nonfatal error and keep the history intact instead
/// of silently rewriting it with a truncation note.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_compact_without_a_model_keeps_history() {
    let (config_path, cwd, data_dir, log) = setup_with_log("compact-no-model");
    let original = pig_core::config::load(&config_path).expect("original config");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd).await;

    // Two sizeable turns so a truncating compaction would have older work to
    // omit and the assertion would see a real difference.
    send(
        &events,
        &agent,
        &sid,
        &format!("OLD_ALPHA {}", "a".repeat(2_000)),
    );
    wait_turn(&events).await;
    send(
        &events,
        &agent,
        &sid,
        &format!("OLD_BETA {}", "b".repeat(2_000)),
    );
    wait_turn(&events).await;

    // Drop the model while the session stays open: the next manual compact
    // cannot summarize.
    agent
        .ops
        .send(Op::SaveConfig {
            config: pig_protocol::AppConfig::default(),
        })
        .await
        .unwrap();
    agent
        .ops
        .send(Op::Compact {
            session_id: sid.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::Error { error, .. }
                if matches!(error.semantic_error(), pig_protocol::CoreError::NoModelConfigured)
        )
    })
    .await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::Error { error, fatal: false, .. }
                if matches!(error.semantic_error(), pig_protocol::CoreError::NoModelConfigured)
        )),
        "the rejection is typed and nonfatal: {collected:#?}"
    );
    assert!(
        !collected.iter().any(|e| matches!(
            e,
            Event::CompactStarted { .. } | Event::ContextCompacted { .. }
        )),
        "no compaction may start or claim success without a model: {collected:#?}"
    );
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{sid}.jsonl"))).unwrap();
    assert!(
        !rollout.contains("\"type\":\"compact\""),
        "no compact record is installed: {rollout}"
    );

    // Restore the model: the next request still carries the older turn, so the
    // history was never truncated.
    agent
        .ops
        .send(Op::SaveConfig { config: original })
        .await
        .unwrap();
    send(
        &events,
        &agent,
        &sid,
        "ECHO_HISTORY after the rejected compact",
    );
    wait_turn(&events).await;
    let bodies = log.lock().unwrap().clone();
    let after = bodies
        .iter()
        .rev()
        .find(|body| body.contains("ECHO_HISTORY after the rejected compact"))
        .expect("post-rejection request");
    assert!(
        after.contains("OLD_ALPHA"),
        "the rejected compact left the history untouched: {after}"
    );
    assert!(
        !after.contains("Earlier context compacted"),
        "no truncation note was installed: {after}"
    );
    agent.shutdown();
}

/// Scenario C: plan mode outputs plan text (no tool calls).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scenario_c_plan_mode() {
    let (config_path, cwd, data_dir) = setup("m5-plan");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd.clone()).await;

    agent
        .ops
        .send(Op::SetPlanMode {
            session_id: sid.clone(),
            enabled: true,
        })
        .await
        .unwrap();
    send_with(
        &events,
        &agent,
        &sid,
        "SCENARIO_C give me a refactoring plan",
        ExecMode::Auto,
    );
    // kimi file-semantics closed loop: Write the plan file (pass-through,
    // approval-free) → ExitPlanMode pops an approval → after approval the
    // scenario B tool chain (Auto: only Bash pops an approval) → wrap up
    let mut collected = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            deadline.elapsed() < Duration::from_secs(30),
            "timed out: {collected:#?}"
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
                    decision: pig_protocol::ApprovalDecision::Allow,
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
    // The plan file is actually written (pass-through, no approval card)
    let plan_file = cwd.join(".pigcode/plans/plan-mock.md");
    assert!(
        plan_file.exists(),
        "Write of the plan file should go through without approval: {collected:#?}"
    );
    // The ExitPlanMode popup detail contains the full plan
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::ApprovalRequested { tool, detail, .. }
            if tool == "ExitPlanMode" && detail.contains(mock::PLAN_MARKER)
        )),
        "ExitPlanMode approval dialog should contain the full plan: {collected:#?}"
    );
    // After approval the plan turns off, the mode tier stays unchanged
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::PlanModeChanged { enabled, .. } if !enabled)),
        "plan toggle should turn off after approval"
    );
    assert!(
        collected
            .iter()
            .any(|e| matches!(e, Event::TextDone { full_text, .. } if full_text.contains(mock::SCENARIO_B_MARKER))),
        "after approval the scenario B tool chain should finish the turn: {collected:#?}"
    );
    agent.shutdown();
}
