use crate::common;

use common::{new_session, recv_until, setup, setup_with_log};
use pig_protocol::{CoreError, Event, ExecMode, Op};
use pig_provider::mock;
use std::time::Duration;

/// Recall the last user message after two completed turns: the recall tombstone
/// is appended to the rollout (the recalled content stays in the file), the
/// recalled turn leaves the live model context (the next request no longer
/// carries it), and after a restart the replay skips the recalled turn while
/// ALL turns' stats survive (TurnStats records are never touched).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_excludes_turn_and_keeps_stats() {
    let (config_path, cwd, data_dir, log) = setup_with_log("recall");
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        cwd.clone(),
        data_dir.clone(),
    );
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd.clone()).await;

    for content in ["Read the mock file and summarize", "Summarize once more"] {
        agent
            .ops
            .send(Op::SendMessage {
                session_id: session_id.clone(),
                content: content.into(),
                files: vec![],
                images: vec![],
                permissions: ExecMode::Auto.permissions(),
                delivery: pig_protocol::MessageDelivery::Queue,
            })
            .await
            .unwrap();
        recv_until(&events, Duration::from_secs(20), |e| {
            matches!(e, Event::TurnComplete { .. })
        })
        .await;
    }

    agent
        .ops
        .send(Op::RecallLastMessage {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::MessageRecalled { .. })
    })
    .await;

    // Tombstone semantics: marker appended, the recalled content NOT erased,
    // and both turns' TurnStats records intact
    let rollout_path = data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let rollout = std::fs::read_to_string(&rollout_path)
        .unwrap_or_else(|error| panic!("read rollout {}: {error}", rollout_path.display()));
    assert!(
        rollout.contains("\"type\":\"recall\""),
        "rollout should carry the recall marker: {rollout}"
    );
    assert!(
        rollout.contains("Summarize once more"),
        "tombstone recall must not physically delete the recalled record: {rollout}"
    );
    assert_eq!(
        rollout.matches("\"type\":\"turn_stats\"").count(),
        2,
        "both turns' TurnStats must survive the recall: {rollout}"
    );

    // Live context exclusion: the next turn's request must not carry the
    // recalled message (the earlier turn and the new message stay)
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Say hi again".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let last = log
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|body| body.contains("Say hi again"))
        .cloned()
        .expect("the third turn's request should be logged");
    assert!(
        !last.contains("Summarize once more"),
        "recalled message must leave the model context: {last}"
    );
    assert!(
        last.contains("Read the mock file and summarize"),
        "the earlier turn must stay in context: {last}"
    );
    agent.shutdown();

    // Restart and reopen: replay truncates the recalled turn at the marker,
    // while every turn's stats footer still replays (statistics survive)
    let agent2 = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events2 = agent2.events.clone();
    agent2
        .ops
        .send(Op::OpenSession {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let replay = recv_until(&events2, Duration::from_secs(20), |e| {
        matches!(
            e,
            Event::TurnComplete {
                stats: None,
                duration_ms: 0,
                ..
            }
        )
    })
    .await;
    // Replay re-emits the recalled turn's events and uses MessageRecalled as
    // the in-order truncation point: it must land after the recalled turn's
    // UserMessage and before the next surviving one (the UI truncates there)
    let pos_of = |pred: &dyn Fn(&Event) -> bool| {
        replay
            .iter()
            .position(pred)
            .expect("event should exist in replay")
    };
    let recalled_msg = pos_of(
        &|e| matches!(e, Event::UserMessage { text, .. } if text.contains("Summarize once more")),
    );
    let recall_marker = pos_of(&|e| matches!(e, Event::MessageRecalled { .. }));
    let surviving_msg =
        pos_of(&|e| matches!(e, Event::UserMessage { text, .. } if text.contains("Say hi again")));
    assert!(
        recalled_msg < recall_marker && recall_marker < surviving_msg,
        "MessageRecalled must arrive between the recalled and the next surviving user message: {replay:#?}"
    );
    assert_eq!(
        replay
            .iter()
            .filter(|e| matches!(e, Event::MessageRecalled { .. }))
            .count(),
        1,
        "replay should emit exactly one MessageRecalled: {replay:#?}"
    );
    let stats_footers = replay
        .iter()
        .filter(|e| matches!(e, Event::TurnComplete { stats: Some(_), .. }))
        .count();
    assert_eq!(
        stats_footers, 3,
        "all three turns' stats must replay (recall keeps statistics): {replay:#?}"
    );

    // The resumed session's rebuilt history excludes the recalled turn too:
    // a fresh request after reopen must not carry it
    agent2
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Final check".into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    recv_until(&events2, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let last = log
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|body| body.contains("Final check"))
        .cloned()
        .expect("the post-reopen request should be logged");
    assert!(
        !last.contains("Summarize once more"),
        "rebuilt history must exclude the recalled turn: {last}"
    );
    assert!(last.contains("Read the mock file and summarize"));
    assert!(last.contains("Say hi again"));
    agent2.shutdown();
}

/// Recall is idle-only: during a running turn the op is rejected with
/// RecallTurnInFlight; after the abort settles it succeeds (removing the
/// aborted turn), and a further recall with nothing left errors with
/// NothingToRecall.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_busy_rejected_and_empty_session() {
    let (config_path, cwd, data_dir) = setup("recall-busy");
    let agent = pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir);
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: mock::SCENARIO_SLOW_TRIGGER.into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnStarted { .. })
    })
    .await;

    // Busy: rejected
    agent
        .ops
        .send(Op::RecallLastMessage {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let rejected = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::Error { .. })
    })
    .await;
    assert!(
        rejected.iter().any(
            |e| matches!(e, Event::Error { error, .. } if *error == CoreError::RecallTurnInFlight)
        ),
        "busy recall should be rejected with RecallTurnInFlight: {rejected:#?}"
    );

    // Stop the slow turn; once the wrap-up returns the session, recall succeeds
    agent
        .ops
        .send(Op::Interrupt {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnAborted { .. })
    })
    .await;
    // TurnAborted is emitted before the wrap-up writes; sync with a GetConfig
    // round-trip so the session is back in the entry before recalling
    agent.ops.send(Op::GetConfig).await.unwrap();
    recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::ConfigSnapshot { .. })
    })
    .await;

    agent
        .ops
        .send(Op::RecallLastMessage {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::MessageRecalled { .. })
    })
    .await;

    // Nothing left to recall
    agent
        .ops
        .send(Op::RecallLastMessage {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let empty = recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::Error { .. })
    })
    .await;
    assert!(
        empty.iter().any(
            |e| matches!(e, Event::Error { error, .. } if *error == CoreError::NothingToRecall)
        ),
        "recall with no user message should error NothingToRecall: {empty:#?}"
    );
    agent.shutdown();
}

/// A recalled turn took its reminder injection with it. The next turn must
/// re-inject the current AGENTS.md state instead of believing it was already
/// delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_restores_reminder_state_for_the_next_turn() {
    let (config_path, cwd, data_dir, log) = setup_with_log("recall-reminder");
    std::fs::write(cwd.join("AGENTS.md"), "FIRST_AGENTS_CONTENT\n").unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd.clone()).await;
    let send = |content: &str| {
        agent.ops.send(Op::SendMessage {
            session_id: session_id.clone(),
            content: content.into(),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
    };
    send("first turn").await.unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    // Change AGENTS.md: the next turn carries the reminder and records it as
    // already delivered.
    std::fs::write(cwd.join("AGENTS.md"), "SECOND_AGENTS_MARKER\n").unwrap();
    send("turn carrying the reminder").await.unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let with_reminder = log
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|body| body.contains("turn carrying the reminder"))
        .cloned()
        .expect("reminder turn request");
    assert!(
        with_reminder.contains("SECOND_AGENTS_MARKER"),
        "the reminder was delivered once: {with_reminder}"
    );

    agent
        .ops
        .send(Op::RecallLastMessage {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    recv_until(&events, Duration::from_secs(5), |e| {
        matches!(e, Event::MessageRecalled { .. })
    })
    .await;

    send("after the recall").await.unwrap();
    recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let after = log
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|body| body.contains("after the recall"))
        .cloned()
        .expect("post-recall request");
    assert!(
        !after.contains("turn carrying the reminder"),
        "the recalled turn is out of context: {after}"
    );
    assert!(
        after.contains("SECOND_AGENTS_MARKER"),
        "the reminder state is re-injected after its carrier was recalled: {after}"
    );
    agent.shutdown();
}
