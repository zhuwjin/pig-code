//! In-session context byte stability (cache-prefix contract):
//! if AGENTS.md / the skills directory / subagent profiles are modified between
//! two real turns, the tools and system sent to the model (and the entire old
//! message prefix) must stay byte-identical — changes are only allowed in
//! newly appended tail messages (the reminder rides ahead of the newest user
//! message).
//! This is the regression guard for the "freeze + reminder" architecture:
//! whoever reintroduces a rescan into the hot path turns this red first.

mod common;

use pig_core::spawn_agent_with_data_dir;
use pig_protocol::{Event, Op};
use pig_provider::mock;
use std::time::Duration;

/// Main-loop request = messages[0] is the Pig Code system prompt and carries a
/// tools array (bypass requests such as title generation are also logged and
/// must be filtered out)
fn main_loop_requests(log: &[String]) -> Vec<serde_json::Value> {
    log.iter()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .filter(|req| {
            req.get("tools")
                .is_some_and(|t| t.as_array().is_some_and(|a| !a.is_empty()))
                && req["messages"][0]["role"] == "system"
                && req["messages"][0]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("Pig Code"))
        })
        .collect()
}

fn messages_of(req: &serde_json::Value) -> &Vec<serde_json::Value> {
    req["messages"].as_array().expect("messages array")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_prefix_stable_across_turns_and_env_changes() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-ctx-stable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ws = dir.join("ws");
    let data = dir.join("data");
    std::fs::create_dir_all(ws.join(".pigcode").join("agents")).unwrap();
    std::fs::create_dir_all(data.join("skills").join("demo")).unwrap();
    // Environment present before the session starts: AGENTS.md / skills / custom subagent profiles
    std::fs::write(ws.join("AGENTS.md"), "Original AGENTS rules").unwrap();
    std::fs::write(
        data.join("skills").join("demo").join("SKILL.md"),
        "---\nname: demo\ndescription: demo skill\n---\nSkill body",
    )
    .unwrap();
    std::fs::write(
        ws.join(".pigcode").join("agents").join("custom.md"),
        "---\nname: custom\ndescription: custom profile\n---\nProfile body.",
    )
    .unwrap();
    std::fs::write(ws.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock Provider"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
"#
        ),
    )
    .unwrap();

    let agent = spawn_agent_with_data_dir(Some(config_path), ws.clone(), data.clone());
    let session_id = common::new_session(&agent, ws.clone()).await;

    // ---- Turn 1 ----
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Read README.mock.md and summarize".to_string(),
            files: vec![],
            images: vec![],
            mode: pig_protocol::ExecMode::ConfirmBeforeEdit,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let requests = main_loop_requests(&log.lock().unwrap());
    assert!(
        !requests.is_empty(),
        "turn 1 should have main-loop requests"
    );
    let turn1 = requests.last().expect("last request of turn 1").clone();
    let turn1_messages = messages_of(&turn1);
    let turn1_len = turn1_messages.len();
    // The system prompt should contain the frozen environment section; the first user message carries the mode reminder
    assert!(
        turn1_messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("Original AGENTS rules")
    );
    assert!(
        turn1_messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("- demo: demo skill")
    );
    assert!(
        turn1_messages[1]["content"]
            .as_str()
            .unwrap()
            .contains("Current execution mode"),
        "turn 1 user message should carry the mode reminder"
    );

    // ---- Modify the environment mid-session: replace AGENTS.md content, add a skill, add a subagent profile ----
    std::fs::write(ws.join("AGENTS.md"), "Brand-new AGENTS rules").unwrap();
    std::fs::create_dir_all(data.join("skills").join("second")).unwrap();
    std::fs::write(
        data.join("skills").join("second").join("SKILL.md"),
        "---\nname: second\ndescription: new skill\n---\nBody",
    )
    .unwrap();
    std::fs::write(
        ws.join(".pigcode").join("agents").join("more.md"),
        "---\nname: more\ndescription: profile added mid-session\n---\nProfile body.",
    )
    .unwrap();

    // ---- Turn 2 ----
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Continue".to_string(),
            files: vec![],
            images: vec![],
            mode: pig_protocol::ExecMode::ConfirmBeforeEdit,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let requests = main_loop_requests(&log.lock().unwrap());
    let turn2 = requests.last().expect("last request of turn 2").clone();
    let turn2_messages = messages_of(&turn2);

    // ---- Cache-prefix contract: system, tools, and all old messages stay byte-identical ----
    assert_eq!(
        turn1_messages[0], turn2_messages[0],
        "system prompt must stay byte-stable across turns and env changes"
    );
    assert_eq!(
        turn1["tools"], turn2["tools"],
        "tools array must stay byte-stable across turns and env changes (incl. mid-session subagent profiles/skills)"
    );
    assert!(
        turn2_messages.len() >= turn1_len,
        "turn 2 must only append messages at the tail, never rewrite old ones"
    );
    for (i, msg) in turn1_messages.iter().enumerate() {
        assert_eq!(
            msg, &turn2_messages[i],
            "old message[{i}] was rewritten: cache prefix invalidates from there"
        );
    }

    // ---- Changes only appear at the tail: new user message = reminder (AGENTS.md change notification) + original text ----
    // (turn 1's closing assistant text is also in the prefix, so positional indexes drift; locate by content)
    let new_user = turn2_messages
        .iter()
        .rev()
        .find(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_str()
                    .is_some_and(|c| c.ends_with("Continue"))
        })
        .and_then(|m| m["content"].as_str())
        .expect("new user message of turn 2");
    assert!(
        new_user.starts_with("<system-reminder>"),
        "reminder should be prepended to the new user message, actual: {new_user}"
    );
    assert!(
        !new_user.contains("Current execution mode"),
        "unchanged mode must not be re-notified (once on first turn + once on switch), actual: {new_user}"
    );
    assert!(new_user.contains("AGENTS.md content has been updated"));
    assert!(
        new_user.contains("Brand-new AGENTS rules"),
        "reminder should carry the latest AGENTS.md content"
    );
    assert!(
        new_user.ends_with("Continue"),
        "original text should stay after the reminder"
    );
    // New skills/profiles do not enter the frozen section (implied by the system assertion), nor tools (asserted above)

    // ---- Turn 3: switch execution mode → the next turn's reminder carries only the new mode line, the prefix stays stable ----
    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: "Summarize once more".to_string(),
            files: vec![],
            images: vec![],
            mode: pig_protocol::ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    common::recv_until(&agent.events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    let requests = main_loop_requests(&log.lock().unwrap());
    let turn3 = requests.last().expect("last request of turn 3").clone();
    let turn3_messages = messages_of(&turn3);
    for (i, msg) in turn2_messages.iter().enumerate() {
        assert_eq!(
            msg, &turn3_messages[i],
            "turn 3 rewrote old message[{i}]: cache prefix invalidates from there"
        );
    }
    let new_user3 = turn3_messages
        .iter()
        .rev()
        .find(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_str()
                    .is_some_and(|c| c.ends_with("Summarize once more"))
        })
        .and_then(|m| m["content"].as_str())
        .expect("new user message of turn 3");
    assert!(
        new_user3.starts_with("<system-reminder>"),
        "mode switch should produce a reminder, actual: {new_user3}"
    );
    assert!(
        new_user3.contains("Current execution mode: auto-edit"),
        "mode switch should notify the new mode, actual: {new_user3}"
    );
    assert!(
        !new_user3.contains("AGENTS.md content has been updated"),
        "AGENTS.md was already notified and must not repeat, actual: {new_user3}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
