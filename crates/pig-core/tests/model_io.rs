mod common;

use common::{new_session, recv_until, setup};
use pig_core::mock;
use pig_protocol::{Event, ExecMode, Op};
use std::time::Duration;

/// The main session persists one call-trace record per provider step
/// ({session}.model-io.jsonl): after one sent message there should be at least
/// two records (a tool-call step + a closing step), with complete fields, and
/// the input should contain the projection of the user message
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_io_written_per_step() {
    let (config_path, cwd, data_dir) = setup("model-io");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let sid = new_session(&agent, cwd.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: "Read the mock file and summarize".into(),
            files: vec![mock::MOCK_FILE_NAME.into()],
            images: vec![],
            mode: ExecMode::AutoEdit,
        })
        .await
        .unwrap();
    let _ = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;

    let path = pig_core::model_io::model_io_path(&data_dir.join("sessions"), &sid);
    assert!(
        path.exists(),
        "trace file should be persisted: {}",
        path.display()
    );
    let records = pig_core::model_io::read_all(&path);
    assert!(
        records.len() >= 2,
        "a turn with tool calls spans at least two steps, got {} records",
        records.len()
    );
    for record in &records {
        assert_eq!(record.source, "main");
        assert_eq!(record.provider, "Mock Provider");
        assert_eq!(record.model, "mock-model");
        assert!(
            record.turn.starts_with('t'),
            "turn id looks like {{turn}}-s{{step}}"
        );
        assert!(
            matches!(record.finish.as_str(), "stop" | "tool_calls"),
            "finish reason must be valid: {}",
            record.finish
        );
        assert!(
            !record.input.is_empty(),
            "input projection must not be empty"
        );
        assert!(record.duration_ms > 0 || record.usage.output > 0);
    }
    // The first record's input should contain the system prompt and the user message
    assert_eq!(records[0].input[0].role, "system");
    assert!(
        records[0].input.iter().any(|m| m.role == "user"
            && m.content
                .as_deref()
                .is_some_and(|c| c.contains("mock file"))),
        "input projection should contain the user message"
    );
    // Delta persistence: raw lines do not repeat the full context (from the
    // second record on, input_offset > 0 and only the new part is stored);
    // after read_all expansion each record is a full sequence (the second is
    // longer than the first)
    let raw = std::fs::read_to_string(&path).unwrap();
    let deltas: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(deltas.len() >= 2);
    assert_eq!(deltas[0]["input_offset"], 0, "first record is full context");
    assert!(
        deltas[1]["input_offset"].as_u64().unwrap_or(0) > 0,
        "records after the first should store only the delta: {:?}",
        deltas[1]["input_offset"]
    );
    assert!(
        deltas[1]["input"].as_array().map(|a| a.len()).unwrap_or(0) < records[1].input.len(),
        "raw delta entry count should be fewer than the expanded full input"
    );
    assert!(
        records[1].input.len() > records[0].input.len(),
        "expanded second input should contain the new messages"
    );
    let _ = std::fs::remove_dir_all(data_dir.parent().unwrap().parent().unwrap());
}
