mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

fn send(
    events: &async_channel::Receiver<Event>,
    agent: &pig_core::AgentHandle,
    sid: &str,
    text: &str,
) {
    send_with(events, agent, sid, text, ExecMode::AutoEdit)
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
            mode,
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

/// Summary request fails → falls back to plain truncation; the session does not die.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_failure_falls_back() {
    let (config_path, cwd, data_dir) = setup("m5-fallback");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
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
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    let Some(Event::ContextCompacted { omitted, note, .. }) = collected.last() else {
        panic!("expected ContextCompacted")
    };
    assert!(*omitted > 0);
    assert!(note.contains("Earlier context compacted"), "{note}");
    assert!(
        !note.contains("model summary"),
        "summary failure should fall back to truncation: {note}"
    );

    // The session can still continue
    send(&events, &agent, &sid, "ECHO_HISTORY");
    let collected = wait_turn(&events).await;
    assert!(
        collected.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains("HISTORY_COUNT:")
        )),
        "session should continue after compact failure fallback: {collected:#?}"
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
        ExecMode::AutoEdit,
    );
    // kimi file-semantics closed loop: Write the plan file (pass-through,
    // approval-free) → ExitPlanMode pops an approval → after approval the
    // scenario B tool chain (AutoEdit: only Bash pops an approval) → wrap up
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
