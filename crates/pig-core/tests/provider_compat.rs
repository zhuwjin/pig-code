//! Provider wire-level compat: business errors inside HTTP-200 streams, and
//! the silent retry of empty completions (both mirror ZCode's handling; the
//! scenarios live in mock.rs).

use crate::common;

use common::{new_session, recv_until, setup};
use pig_protocol::{CoreError, Event, ExecMode, Op};
use pig_provider::mock;
use std::time::Duration;

/// An OpenAI-compatible gateway reporting a business error as a 200 SSE data
/// frame must surface as a structured error (not a silent empty turn)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn business_error_in_200_stream_surfaces() {
    let (config_path, cwd, data_dir) = setup("provider-bizerr");
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), cwd.clone(), data_dir.clone());
    let events = agent.events.clone();
    let session_id = new_session(&agent, cwd).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id,
            content: format!(
                "{BUSINESS_ERROR_200} please",
                BUSINESS_ERROR_200 = mock::BUSINESS_ERROR_TRIGGER
            ),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let events = recv_until(&events, Duration::from_secs(10), |e| {
        matches!(e, Event::Error { .. })
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Error { error, .. }
                if matches!(error.semantic_error(), CoreError::Internal { detail } if detail.contains(mock::BUSINESS_ERROR_MARKER))
        )),
        "the business error detail should surface: {events:#?}"
    );
    agent.shutdown();
}

/// A 200 stream that ends with no text/reasoning/tool calls gets exactly one
/// silent retry; the turn then completes with the retry's reply
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_completion_retried_once() {
    // Own setup (not common::setup): the request log proves the retry happened
    let (port, log) = mock::start_mock_server_with_log();
    let dir = std::env::temp_dir().join(format!("pig-core-empty-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(mock::MOCK_FILE_NAME), mock::MOCK_FILE_CONTENT).unwrap();
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
    let agent =
        pig_core::spawn_agent_with_data_dir(Some(config_path), dir.clone(), dir.join("data"));
    let events = agent.events.clone();
    let session_id = new_session(&agent, dir.clone()).await;

    agent
        .ops
        .send(Op::SendMessage {
            session_id: session_id.clone(),
            content: format!("{EMPTY_ONCE} please", EMPTY_ONCE = mock::EMPTY_ONCE_TRIGGER),
            files: vec![],
            images: vec![],
            permissions: ExecMode::Auto.permissions(),
            delivery: pig_protocol::MessageDelivery::Queue,
        })
        .await
        .unwrap();

    let events = recv_until(&events, Duration::from_secs(20), |e| {
        matches!(e, Event::TurnComplete { .. })
    })
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TextDone { full_text, .. } if full_text.contains(mock::EMPTY_ONCE_MARKER)
        )),
        "the retry's reply should complete the turn: {events:#?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error { .. })),
        "the empty first attempt must stay silent (no error): {events:#?}"
    );
    // The retry is a second streaming request carrying the same marker (the
    // title sidecar's request is non-streaming and does not count)
    let streaming_requests = log
        .lock()
        .expect("log lock")
        .iter()
        .filter(|body| body.contains(mock::EMPTY_ONCE_TRIGGER) && body.contains("\"stream\":true"))
        .count();
    assert_eq!(streaming_requests, 2, "empty attempt + exactly one retry");

    let db = rusqlite::Connection::open(dir.join("data/store.sqlite")).unwrap();
    let row = db.query_row(
        "SELECT COUNT(*), SUM(retry_count), SUM(usage_reported) FROM api_usage
         WHERE session_id = ?1 AND source = 'main'",
        rusqlite::params![session_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        },
    );
    let (calls, retries, reported) = row.unwrap();
    assert_eq!(calls, 1, "two HTTP attempts remain one logical API row");
    assert_eq!(retries, Some(1));
    assert_eq!(reported, Some(0), "neither attempt reported usage");
    agent.shutdown();
}
