//! Streaming tool-input events (live pending tool cards): the provider
//! newline-batches argument fragments, the turn loop forwards them as
//! ToolInputStart/Delta under the execution-time item_id, and the pending
//! card becomes the executing card in place. Runs the same mock scenario
//! through all three wire formats.

use crate::common;

use common::{new_session, recv_until};
use pig_protocol::{ApiFormat, Event, ExecMode, Op};
use pig_provider::mock;
use std::time::Duration;

fn format_config(port: u16, format: ApiFormat) -> String {
    let format_str = match format {
        ApiFormat::OpenAiChat => "OpenAiChat",
        ApiFormat::AnthropicMessages => "AnthropicMessages",
        ApiFormat::OpenAiResponses => "OpenAiResponses",
    };
    format!(
        r#"default_provider = "mock"
default_model = "mock-model"

[[providers]]
id = "mock"
name = "Mock Provider"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "mock-key"
api_format = "{format_str}"
enabled = true

[[providers.models]]
id = "mock-model"
context_window = 128000
max_output_tokens = 8192
"#
    )
}

async fn run_stream_args_case(format: ApiFormat) {
    let port = mock::start_mock_server();
    let dir = std::env::temp_dir().join(format!(
        "pig-core-tool-stream-{format:?}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, format_config(port, format)).unwrap();
    let agent = pig_core::spawn_agent_with_data_dir(
        Some(config_path.clone()),
        dir.clone(),
        dir.join("data"),
    );
    let events = agent.events.clone();
    let sid = new_session(&agent, dir.clone()).await;
    agent
        .ops
        .send(Op::SendMessage {
            session_id: sid.clone(),
            content: format!("{} please", mock::STREAM_ARGS_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();
    let collected = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    // The scenario's second reply carries the marker (the Write actually ran)
    assert!(
        collected.iter().any(|e| {
            matches!(e, Event::TextDone { full_text, .. } if full_text.contains(mock::STREAM_ARGS_MARKER))
        }),
        "scenario did not complete for {format:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(mock::STREAM_ARGS_FILE)).unwrap(),
        mock::STREAM_ARGS_CONTENT,
        "the streamed Write must execute with fully reassembled arguments ({format:?})"
    );

    // Live pending events: Start before Begin, Deltas before Begin (all under
    // the same item_id), concatenated deltas equal the whole arguments JSON
    let item = |event: &Event| match event {
        Event::ToolInputStart { item_id, .. }
        | Event::ToolInputDelta { item_id, .. }
        | Event::ToolCallBegin { item_id, .. } => Some(item_id.clone()),
        _ => None,
    };
    let start_ix = collected
        .iter()
        .position(|e| matches!(e, Event::ToolInputStart { tool, .. } if tool == "Write"))
        .unwrap_or_else(|| panic!("no ToolInputStart in {format:?}"));
    let begin_ix = collected
        .iter()
        .position(|e| matches!(e, Event::ToolCallBegin { tool, .. } if tool == "Write"))
        .unwrap_or_else(|| panic!("no ToolCallBegin in {format:?}"));
    assert!(start_ix < begin_ix, "pending card must precede execution");
    let start_item = item(&collected[start_ix]).unwrap();
    let begin_item = item(&collected[begin_ix]).unwrap();
    assert_eq!(
        start_item, begin_item,
        "the pending card and the executing card must share the item_id ({format:?})"
    );
    let mut deltas = String::new();
    for event in &collected[start_ix + 1..begin_ix] {
        if let Event::ToolInputDelta { item_id, delta, .. } = event {
            assert_eq!(*item_id, start_item);
            deltas.push_str(delta);
        }
    }
    let expected_args =
        serde_json::json!({"path": mock::STREAM_ARGS_FILE, "content": mock::STREAM_ARGS_CONTENT})
            .to_string();
    assert_eq!(
        deltas, expected_args,
        "concatenated live deltas must equal the whole arguments JSON ({format:?})"
    );
    // More than one delta: the newline batching really split per content line
    let delta_count = collected[start_ix + 1..begin_ix]
        .iter()
        .filter(|e| matches!(e, Event::ToolInputDelta { .. }))
        .count();
    assert!(
        delta_count >= 3,
        "expected per-line deltas, got {delta_count} ({format:?})"
    );
    // No discard in the happy path
    assert!(
        !collected
            .iter()
            .any(|e| matches!(e, Event::ToolInputDiscard { .. })),
        "no ToolInputDiscard expected ({format:?})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_tool_inputs_ride_all_three_formats() {
    run_stream_args_case(ApiFormat::OpenAiChat).await;
    run_stream_args_case(ApiFormat::AnthropicMessages).await;
    run_stream_args_case(ApiFormat::OpenAiResponses).await;
}
