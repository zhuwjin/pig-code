mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

/// Session fork: a two-turn session forked with turns=1 opens the new session
/// via the cold path (SessionConfigured + replay); the replay contains only the
/// first turn; store meta inherits the model choice, the title carries the
/// " (fork)" suffix, and title_custom is set (auto-naming must not overwrite).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_session_truncates_and_switches() {
    let (config_path, cwd, data_dir) = setup("fork");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
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
                mode: ExecMode::AutoEdit,
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
        .send(Op::ForkSession {
            session_id: session_id.clone(),
            turns: 1,
        })
        .await
        .unwrap();
    let configured = recv_until(
        &events,
        Duration::from_secs(10),
        |e| matches!(e, Event::SessionConfigured { session_id: id, .. } if *id != session_id),
    )
    .await;
    let Some(Event::SessionConfigured {
        session_id: fork_id,
        model,
        provider_name,
        ..
    }) = configured.last()
    else {
        panic!("fork should produce a SessionConfigured for the new session: {configured:?}");
    };
    let fork_id = fork_id.clone();
    assert_eq!(
        model, "mock-model",
        "model resolution should inherit the source session (config default)"
    );
    assert_eq!(provider_name, "Mock Provider");

    // Let the replay (including the first turn's TurnStats-carrying TurnComplete
    // and the closing TurnComplete with duration_ms=0) drain fully before
    // asserting content
    let replay = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnComplete { stats: None, .. })
    })
    .await;
    let user_msgs: Vec<&str> = replay
        .iter()
        .filter_map(|e| match e {
            Event::UserMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_msgs.len(),
        1,
        "replay should contain only the first turn's user message: {user_msgs:?}"
    );
    assert!(user_msgs[0].contains("Read the mock"));
    assert!(
        replay.iter().any(|e| matches!(e, Event::TextDone { full_text, .. } if full_text.contains(mock::MOCK_REPLY_MARKER))),
        "replay should contain the first turn's assistant reply"
    );
    // The second turn's records must not enter the forked rollout
    let rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{fork_id}.jsonl")))
            .unwrap();
    assert!(
        !rollout.contains("Summarize once more"),
        "fork rollout must not contain the second turn"
    );

    // Index: the SessionList re-emitted for the fork arrives in the C1 batch (before SessionConfigured)
    let listed = configured
        .iter()
        .find_map(|e| match e {
            Event::SessionList { sessions } => Some(sessions),
            _ => None,
        })
        .expect("fork should re-emit SessionList");
    let meta = listed
        .iter()
        .find(|s| s.id == fork_id)
        .expect("list should contain the forked session");
    assert!(
        meta.title.ends_with(" (fork)"),
        "title should carry the fork suffix (persisted English constant): {}",
        meta.title
    );
    assert!(
        meta.title_custom,
        "fork title should be fixed (auto-naming must not overwrite)"
    );
    assert!(!meta.pinned, "pinned flag must not be inherited");

    agent.shutdown();
}
