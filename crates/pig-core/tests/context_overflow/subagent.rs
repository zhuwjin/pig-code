//! Child context compaction: a long single-task subagent now compacts its own
//! JSONL context (preflight + complete-boundary selection + summary sidecar)
//! and continues, while the parent session never compacts for a child. The
//! compacted prefix survives a restart replay.

use std::path::{Path, PathBuf};

use pig_protocol::{Event, Op};
use pig_provider::mock;

use crate::common;
use crate::fixtures::{send_and_complete, workspace};

const CHILD_ID: &str = "a9-1";

fn config(base_url: &str) -> String {
    format!(
        r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock"
base_url = "{base_url}"
api_key = "mock-key"
api_format = "OpenAiChat"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192

[[providers.models]]
id = "small-model"
context_window = 30000
max_output_tokens = 2000
"#
    )
}

/// One long delegated task: several complete user segments plus a complete
/// assistant/tool round whose result carries the typed-overflow trigger.
fn long_child_history() -> Vec<pig_provider::ChatMsg> {
    use pig_provider::{ChatMsg, ToolCall};
    let call = ToolCall {
        id: "call_old_round".into(),
        name: "Read".into(),
        arguments: "{\"path\":\"old.txt\"}".into(),
    };
    vec![
        ChatMsg::system("You are a subagent.".into()),
        ChatMsg::user(format!("old topic A {}", "a".repeat(8_000))),
        ChatMsg::assistant("A done".into(), vec![], None),
        ChatMsg::user(format!("old topic B {}", "b".repeat(6_000))),
        ChatMsg::assistant(String::new(), vec![call], None),
        ChatMsg::tool_result(
            "call_old_round",
            format!(
                "{}{}",
                mock::CONTEXT_OVERFLOW_HTTP_TRIGGER,
                "b".repeat(6_000)
            ),
        ),
        ChatMsg::assistant("B done".into(), vec![], None),
        ChatMsg::user(format!("old topic C {}", "c".repeat(20_000))),
        ChatMsg::assistant(format!("C done {}", "c".repeat(20_000)), vec![], None),
    ]
}

fn seed_child(data_dir: &Path, session_id: &str) -> PathBuf {
    let jsonl = data_dir
        .join("sessions")
        .join(format!("{session_id}.agents"))
        .join(format!("{CHILD_ID}.jsonl"));
    std::fs::create_dir_all(jsonl.parent().expect("agent dir")).unwrap();
    let mut lines = vec![
        serde_json::json!({
            "type": "meta",
            "agent_id": CHILD_ID,
            "profile": "overflow",
            "description": "long child compaction fixture",
            "model": "small-model",
            "provider": "Mock",
            "created_at": 1,
        })
        .to_string(),
    ];
    for msg in long_child_history() {
        lines.push(
            serde_json::json!({
                "type": "msg",
                "msg": msg,
                "replay": null,
                "reasoning": null,
                "images": [],
                "documents": [],
            })
            .to_string(),
        );
    }
    std::fs::write(&jsonl, format!("{}\n", lines.join("\n"))).unwrap();
    jsonl
}

fn child_stream_requests(log: &mock::RequestLog, marker: &str) -> Vec<serde_json::Value> {
    log.lock()
        .unwrap()
        .iter()
        .filter_map(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .filter(|request| {
            request["stream"] == true
                && request.to_string().contains(marker)
                && request["messages"][0]["content"]
                    .as_str()
                    .is_some_and(|system| !system.contains("Pig Code"))
        })
        .collect()
}

fn summary_requests(log: &mock::RequestLog) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|body| body.contains("[COMPACTION]") && body.contains("\"stream\":false"))
        .cloned()
        .collect()
}

/// The old assertion ("a child overflow never compacts") is replaced by the
/// positive contract: a long single-task child compacts at preflight, keeps
/// its active request, continues, and replays the compacted prefix after a
/// restart. The parent session itself never emits a compaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_compacts_a_long_task_and_continues_across_restart() {
    let (port, log) = mock::start_mock_server_with_log();
    let dir = workspace("subagent");
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config(&format!("http://127.0.0.1:{port}/v1"))).unwrap();
    let data_dir = dir.join("data");
    let agents_dir = data_dir.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    // The child profile pins the small-context model; resume re-resolves it
    // from the current profile (the seeded meta's model string is metadata).
    std::fs::write(
        agents_dir.join("overflow.md"),
        "---\nname: overflow\ndescription: overflow test profile\ntools: [Read]\n\
         model: mock/small-model\n---\nYou are a subagent.\n",
    )
    .unwrap();
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        data_dir.clone(),
    );
    let session_id = common::new_session(&agent, dir.clone()).await;
    let jsonl = seed_child(&data_dir, &session_id);

    // First turn only leaves the generated id in the parent history; the mock
    // RESUME scenario reads it from there and resumes the seeded context.
    let greeting = send_and_complete(
        &agent,
        &session_id,
        &format!("{} GREP agent_id: {CHILD_ID}", mock::SUBAGENT_TRIGGER),
    )
    .await;
    assert!(
        greeting
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "{greeting:#?}"
    );

    let events = send_and_complete(
        &agent,
        &session_id,
        &format!("{} RESUME", mock::SUBAGENT_TRIGGER),
    )
    .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnComplete { .. })),
        "the parent turn completes with the child receipt: {events:#?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::ContextCompacted { .. } | Event::CompactStarted { .. }
        )),
        "the parent does not compact for a child: {events:#?}"
    );
    let receipt = events
        .iter()
        .find_map(|event| match event {
            Event::ToolCallEnd {
                output, is_error, ..
            } if output.contains(mock::SUBAGENT_RESUMED_DONE) => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("the resumed child settles the Agent card");
    assert!(!receipt.1, "the compacted child succeeds: {}", receipt.0);
    assert!(receipt.0.contains("status: completed"), "{}", receipt.0);
    assert!(
        receipt.0.contains(&format!("agent_id: {CHILD_ID}")),
        "{}",
        receipt.0
    );

    // Canonical child marker: boundary math, note, no base64.
    let raw = std::fs::read_to_string(&jsonl).unwrap();
    let compact_line = raw
        .lines()
        .find(|line| line.contains("\"type\":\"compact\""))
        .expect("the child JSONL carries a canonical compaction marker");
    let compact: serde_json::Value = serde_json::from_str(compact_line).unwrap();
    assert_eq!(compact["trigger"], "watermark", "{compact}");
    assert!(
        compact["omitted"]
            .as_u64()
            .is_some_and(|omitted| omitted > 0),
        "{compact}"
    );
    assert!(
        compact["tail_len"].as_u64().is_some_and(|tail| tail > 0),
        "{compact}"
    );
    assert!(
        compact["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains(mock::SUMMARY_MARKER)),
        "the summary sidecar produced the note: {compact}"
    );
    assert!(
        compact["note"]
            .as_str()
            .is_some_and(|note| note.contains("Earlier context compacted")),
        "{compact}"
    );
    assert!(
        !raw.contains("data_base64"),
        "child JSONL must never store media base64: {raw}"
    );
    let rollout = std::fs::read_to_string(
        data_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl")),
    )
    .unwrap();
    assert!(
        !rollout.contains("\"type\":\"compact\""),
        "the parent rollout has no child compaction: {rollout}"
    );

    // The retried/continued child request carries the compacted context and no
    // longer contains the omitted round's trigger; the summary retry stayed
    // bounded (one typed-overflow retry at most).
    let requests = child_stream_requests(&log, "SUBAGENT_CHILD:RESUMED");
    assert_eq!(requests.len(), 1, "{requests:#?}");
    let request = requests[0].to_string();
    assert!(request.contains("Earlier context compacted"), "{request}");
    assert!(request.contains(mock::SUMMARY_MARKER), "{request}");
    assert!(
        !request.contains(mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        "the omitted round left the child context: {request}"
    );
    assert!(
        request.contains("SUBAGENT_CHILD:RESUMED"),
        "the active task survives: {request}"
    );
    let summaries = summary_requests(&log);
    assert!(!summaries.is_empty(), "a child summary sidecar ran");
    assert!(
        summaries.len() <= 2,
        "the summary retry is bounded: {}",
        summaries.len()
    );
    assert!(
        !summaries
            .last()
            .expect("summary request")
            .contains(mock::CONTEXT_OVERFLOW_HTTP_TRIGGER),
        "the retried summary dropped the overflowing round"
    );

    // Restart: the child context is read back through the compact marker. The
    // omitted work is no longer in the model context; the resumed task and its
    // conclusion are.
    agent.shutdown();
    let agent2 =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), data_dir.clone());
    agent2
        .ops
        .send(Op::LoadSubagent {
            session_id: session_id.clone(),
            agent_id: CHILD_ID.to_string(),
        })
        .await
        .unwrap();
    let loaded = common::recv_until(
        &agent2.events,
        std::time::Duration::from_secs(10),
        |event| matches!(event, Event::SubagentHistory { .. } | Event::Error { .. }),
    )
    .await;
    let items = loaded
        .iter()
        .find_map(|event| match event {
            Event::SubagentHistory { items, .. } => Some(items.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("restart should load the child context: {loaded:#?}"));
    let text = items
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains(mock::SUBAGENT_RESUMED_DONE),
        "the replayed child context keeps the resumed conclusion: {text}"
    );
    assert!(
        !text.contains("old topic B"),
        "the omitted round no longer enters the child context: {text}"
    );
    agent2.shutdown();
}
