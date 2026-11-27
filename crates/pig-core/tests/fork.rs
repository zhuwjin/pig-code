use crate::common;

use common::{new_session, recv_until, setup, setup_with_log};
use pig_protocol::{Event, ExecMode, Op};
use pig_provider::mock;
use std::time::Duration;

fn pdf_fixture() -> Vec<u8> {
    b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n".to_vec()
}

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
    let search_store = pig_core::store::Store::open(&data_dir).expect("open search store");
    assert!(
        search_store
            .search_sessions(&pig_core::store::SessionSearchQuery::new("once"))
            .unwrap()
            .iter()
            .all(|hit| hit.session_id != fork_id),
        "the truncated second turn is absent from the fork index"
    );
    assert!(
        search_store
            .search_sessions(&pig_core::store::SessionSearchQuery::new("mock"))
            .unwrap()
            .iter()
            .any(|hit| hit.session_id == fork_id),
        "the retained first turn is indexed for the fork"
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

/// Forking after a split-turn compaction copies the Compact record (including
/// the preserved-user index), so the forked request keeps the note, the
/// original request and the reduced history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_after_split_turn_compaction_keeps_the_boundary() {
    let (config_path, cwd, data_dir, log) = setup_with_log("fork-split-compact");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Read the mock file and summarize".into(),
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
    agent
        .ops
        .send(Op::Compact {
            session_id: session_id.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let compacted = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextCompacted { .. })
    })
    .await;
    assert!(
        compacted.iter().any(|e| matches!(
            e,
            Event::ContextCompacted {
                omitted,
                summary: Some(_),
                ..
            } if *omitted > 0
        )),
        "the split-turn compaction succeeded: {compacted:#?}"
    );

    agent
        .ops
        .send(Op::ForkSession {
            session_id: session_id.clone(),
            turns: 1,
        })
        .await
        .unwrap();
    let source_id = session_id.clone();
    let configured = recv_until(
        &events,
        Duration::from_secs(10),
        |e| matches!(e, Event::SessionConfigured { session_id: id, .. } if *id != source_id),
    )
    .await;
    let fork_id = configured
        .iter()
        .find_map(|e| match e {
            Event::SessionConfigured { session_id: id, .. } if *id != source_id => Some(id.clone()),
            _ => None,
        })
        .expect("fork SessionConfigured");
    recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnComplete { stats: None, .. })
    })
    .await;
    let forked_rollout =
        std::fs::read_to_string(data_dir.join("sessions").join(format!("{fork_id}.jsonl")))
            .unwrap();
    let compact: serde_json::Value = serde_json::from_str(
        forked_rollout
            .lines()
            .find(|line| line.contains("\"type\":\"compact\""))
            .expect("the fork keeps the compact record"),
    )
    .unwrap();
    assert!(
        compact["preserved_user"].is_u64(),
        "the preserved-user index survives the fork: {compact}"
    );

    agent
        .ops
        .send(Op::SendMessage {
            session_id: fork_id.clone(),
            content: "continue after fork split compact".into(),
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
    let body = log
        .lock()
        .unwrap()
        .iter()
        .find(|body| body.contains("continue after fork split compact"))
        .cloned()
        .expect("fork request after the split compaction");
    let request: serde_json::Value = serde_json::from_str(&body).unwrap();
    let messages = request["messages"].as_array().expect("messages");
    assert_eq!(
        messages.len(),
        4,
        "note + preserved user + compacted tail + the new user: {body}"
    );
    assert!(
        messages[1]["content"]
            .as_str()
            .is_some_and(|content| content.contains("Earlier context compacted")),
        "the fork keeps the compact note: {body}"
    );
    assert_eq!(
        messages[2]["role"], "user",
        "the preserved active request follows the note: {body}"
    );
    assert!(
        messages[2]["content"]
            .as_str()
            .is_some_and(|content| content.contains("Read the mock file and summarize")),
        "the original request survives the fork: {body}"
    );
    assert!(
        !body.contains("A known file used by"),
        "the omitted tool work stays out of the forked context: {body}"
    );
    agent.shutdown();
}

/// A PDF read in the source session is copied into the fork's media directory
/// and the replayed request prefix is byte-identical to the source's live
/// projection (filename/label/payload all survive the fork).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_copies_pdf_and_preserves_request_prefix() {
    let (config_path, cwd, data_dir, log) = setup_with_log("fork-pdf");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str("\ninput_pdf = true\n");
    std::fs::write(&config_path, config).unwrap();
    std::fs::write(cwd.join("pic.png"), pdf_fixture()).unwrap();
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{} read the document", mock::SCENARIO_MEDIA_TRIGGER),
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
    let live = log
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|body| body.contains("\"file_data\":\"data:application/pdf;base64,"))
        .cloned()
        .expect("source session sends the document");
    let before_fork = log.lock().unwrap().len();
    let source_pdf = data_dir
        .join("sessions")
        .join(format!("{session_id}.media"))
        .join("1.pdf");
    assert_eq!(std::fs::read(&source_pdf).unwrap(), pdf_fixture());

    agent
        .ops
        .send(Op::ForkSession {
            session_id: session_id.clone(),
            turns: 1,
        })
        .await
        .unwrap();
    let source_id = session_id.clone();
    let configured = recv_until(
        &events,
        Duration::from_secs(10),
        |e| matches!(e, Event::SessionConfigured { session_id: id, .. } if *id != source_id),
    )
    .await;
    let fork_id = configured
        .iter()
        .find_map(|e| match e {
            Event::SessionConfigured { session_id: id, .. } if *id != source_id => Some(id.clone()),
            _ => None,
        })
        .expect("fork SessionConfigured");
    recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::TurnComplete { stats: None, .. })
    })
    .await;
    let fork_pdf = data_dir
        .join("sessions")
        .join(format!("{fork_id}.media"))
        .join("1.pdf");
    assert!(
        fork_pdf.is_file(),
        "the fork copies the referenced PDF: {fork_pdf:?}"
    );
    assert_eq!(std::fs::read(&fork_pdf).unwrap(), pdf_fixture());

    agent
        .ops
        .send(Op::SendMessage {
            session_id: fork_id.clone(),
            content: "continue after fork".into(),
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
    let bodies = log.lock().unwrap().clone();
    let forked = bodies[before_fork..]
        .iter()
        .rev()
        .find(|body| body.contains("\"file_data\":\"data:application/pdf;base64,"))
        .expect("fork request carries the document");
    let live_json: serde_json::Value = serde_json::from_str(&live).unwrap();
    let fork_json: serde_json::Value = serde_json::from_str(forked).unwrap();
    let live_messages = live_json["messages"].as_array().expect("live messages");
    let fork_messages = fork_json["messages"].as_array().expect("fork messages");
    assert!(fork_messages.len() > live_messages.len());
    assert_eq!(
        &fork_messages[..live_messages.len()],
        live_messages,
        "the forked history prefix must equal the source projection byte for byte"
    );
    agent.shutdown();
}
