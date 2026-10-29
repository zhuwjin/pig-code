//! Stream-hardening regressions against the real in-process HTTP mock:
//! discarded empty attempts never double-count usage, unknown/missing terminal
//! labels keep streamed text but never execute uncertain tool calls, a stalled
//! stream hits an idle (not overall) deadline, and a stalled *error* body is
//! bounded by the same policy instead of hanging the turn.

use std::time::Duration;

use pig_protocol::{ApiFormat, CoreError, FinishReason};
use pig_provider::mock;
use pig_provider::{CallControl, ChatMsg, ProviderEvent, ResolvedModel, stream_chat};
use tokio_util::sync::CancellationToken;

fn model(port: u16, format: ApiFormat) -> ResolvedModel {
    ResolvedModel {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "mock-key".into(),
        model: match format {
            ApiFormat::AnthropicMessages => "claude-sonnet-4-5".into(),
            _ => "mock-model".into(),
        },
        context_window: 128_000,
        max_output_tokens: 8_192,
        api_format: format,
        reasoning_params: None,
        cap_structured: false,
        cap_strict_tools: false,
        cap_web_search: false,
        web_search_tool: None,
        input_image: false,
        input_pdf: false,
        anthropic_cache_control: false,
        replay_native_search: false,
        prompt_cache_key: None,
        provider_name: "mock".into(),
        tool_settings: Default::default(),
    }
}

fn formats() -> [ApiFormat; 3] {
    [
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
    ]
}

async fn run(config: ResolvedModel, content: &str, control: CallControl) -> Vec<ProviderEvent> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(stream_chat(
        config,
        vec![ChatMsg::user(content.to_string())],
        Vec::new(),
        tx,
        control,
    ));
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    task.await.expect("stream task joins");
    events
}

fn texts(events: &[ProviderEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn failed(events: &[ProviderEvent]) -> Option<&CoreError> {
    events.iter().find_map(|event| match event {
        ProviderEvent::Failed(error) => Some(error),
        _ => None,
    })
}

fn has_tool_calls(events: &[ProviderEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, ProviderEvent::ToolCalls(_)))
}

fn has_finished(events: &[ProviderEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, ProviderEvent::Finished(_)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_completion_retry_drops_the_discarded_attempts_usage() {
    let (port, log) = mock::start_mock_server_with_log();
    let events = run(
        model(port, ApiFormat::OpenAiChat),
        mock::EMPTY_USAGE_ONCE_TRIGGER,
        CallControl::new(CancellationToken::new()),
    )
    .await;

    let usages: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::Usage {
                input,
                output,
                used,
                ..
            } => Some((*input, *output, *used)),
            _ => None,
        })
        .collect();
    assert_eq!(
        usages,
        vec![(12, 5, 17)],
        "only the successful attempt's usage is forwarded: {events:?}"
    );
    assert!(
        texts(&events)
            .iter()
            .any(|text| text.contains(mock::EMPTY_USAGE_ONCE_MARKER)),
        "the retried attempt's text arrives: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProviderEvent::Retrying(_))),
        "the retry stays visible"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProviderEvent::Finished(FinishReason::Stop)))
    );
    assert_eq!(
        log.lock().expect("log").len(),
        2,
        "the empty attempt plus its retry"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_terminal_labels_keep_text_but_never_run_tool_calls() {
    for format in formats() {
        let (port, _log) = mock::start_mock_server_with_log();
        let trigger = match format {
            ApiFormat::AnthropicMessages => mock::UNKNOWN_STOP_TRIGGER,
            _ => mock::UNKNOWN_FINISH_TRIGGER,
        };
        let events = run(
            model(port, format),
            trigger,
            CallControl::new(CancellationToken::new()),
        )
        .await;
        assert!(
            texts(&events)
                .iter()
                .any(|text| text.contains(mock::UNKNOWN_TERMINAL_TEXT)),
            "{format:?}: streamed text must survive the unknown label: {events:?}"
        );
        let error = failed(&events).unwrap_or_else(|| {
            panic!("{format:?}: an unknown terminal label must fail: {events:?}")
        });
        let detail = format!("{error:?}");
        assert!(
            detail.to_ascii_lowercase().contains("unknown")
                || detail.contains("Incomplete response returned"),
            "{format:?}: {detail}"
        );
        assert!(
            !has_tool_calls(&events) && !has_finished(&events),
            "{format:?}: no healthy completion and no tool execution: {events:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_finish_is_strict_but_keeps_streamed_text() {
    for format in formats() {
        let (port, _log) = mock::start_mock_server_with_log();
        let events = run(
            model(port, format),
            mock::MISSING_FINISH_TRIGGER,
            CallControl::new(CancellationToken::new()),
        )
        .await;
        assert!(
            texts(&events)
                .iter()
                .any(|text| text.contains("unterminated answer")),
            "{format:?}: partial text is preserved: {events:?}"
        );
        assert!(
            failed(&events).is_some(),
            "{format:?}: a malformed stream never becomes a healthy stop"
        );
        assert!(
            !has_tool_calls(&events) && !has_finished(&events),
            "{format:?}: {events:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_streams_hit_an_idle_deadline_not_an_overall_one() {
    for format in formats() {
        let (port, _log, mut observer) = mock::start_mock_server_with_stall_observer();
        let task = tokio::spawn(run(
            model(port, format),
            mock::STALL_STREAM_TRIGGER,
            CallControl::new(CancellationToken::new())
                .with_idle_timeout(Duration::from_millis(150)),
        ));
        assert!(
            observer.wait_stall_started(Duration::from_secs(15)).await,
            "{format:?}: the mock never entered its stall"
        );
        let events = task.await.expect("stream task joins");
        assert!(
            texts(&events)
                .iter()
                .any(|text| text.contains(mock::STALL_STREAM_TEXT)),
            "{format:?}: the frame before the stall is forwarded: {events:?}"
        );
        match failed(&events) {
            Some(CoreError::StreamRead { detail }) if detail.contains("idle") => {}
            other => panic!("{format:?}: expected an idle StreamRead error, got {other:?}"),
        }
        assert!(!has_finished(&events), "{format:?}: {events:?}");
        // The deterministic handshake replaces a wall-clock bound: the mock
        // still holds the response, so the client must have hit its own
        // per-chunk deadline rather than the server closing.
        assert!(
            observer.is_stalled(),
            "{format:?}: the client returned while the mock still held the response"
        );
        observer.release();
    }
}

/// A non-2xx response whose body never finishes must not hang the turn: the
/// error body is read with the same per-chunk idle deadline, and the HTTP
/// status still decides the failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_error_bodies_hit_the_idle_deadline() {
    for format in formats() {
        let (port, _log, mut observer) = mock::start_mock_server_with_stall_observer();
        let task = tokio::spawn(run(
            model(port, format),
            mock::ERROR_BODY_STALL_TRIGGER,
            CallControl::new(CancellationToken::new())
                .with_idle_timeout(Duration::from_millis(150)),
        ));
        assert!(
            observer.wait_stall_started(Duration::from_secs(15)).await,
            "{format:?}: the mock never entered its error-body stall"
        );
        let events = task.await.expect("stream task joins");
        let detail = format!(
            "{:?}",
            failed(&events).unwrap_or_else(|| panic!("{format:?}: a 400 must fail: {events:?}"))
        );
        assert!(
            detail.contains("400"),
            "{format:?}: the HTTP status decides the failure: {detail}"
        );
        assert!(
            !has_finished(&events),
            "{format:?}: an error body is never a successful stream: {events:?}"
        );
        assert!(
            observer.is_stalled(),
            "{format:?}: the error body was still stalled when the client returned"
        );
        observer.release();
    }
}

/// The bytes already received for a stalled error body still carry a typed
/// overflow: the idle failure must not erase that classification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_error_body_keeps_its_context_window_classification() {
    for format in formats() {
        let (port, _log, mut observer) = mock::start_mock_server_with_stall_observer();
        let task = tokio::spawn(run(
            model(port, format),
            mock::CONTEXT_OVERFLOW_STALL_TRIGGER,
            CallControl::new(CancellationToken::new())
                .with_idle_timeout(Duration::from_millis(150)),
        ));
        assert!(
            observer.wait_stall_started(Duration::from_secs(15)).await,
            "{format:?}: the mock never entered its error-body stall"
        );
        let events = task.await.expect("stream task joins");
        match failed(&events) {
            Some(CoreError::ContextWindowExceeded { detail }) => {
                assert!(
                    detail.contains("400") && detail.contains("context"),
                    "{format:?}: the partial body still classifies: {detail}"
                );
            }
            other => panic!("{format:?}: expected a context-window classification, got {other:?}"),
        }
        assert!(!has_finished(&events), "{format:?}: {events:?}");
        assert!(
            observer.is_stalled(),
            "{format:?}: the error body was still stalled when the client returned"
        );
        observer.release();
    }
}
