use super::{StructuredOutput, apply_structured, message_has_tool_calls};
use pig_protocol::ApiFormat;

fn output() -> StructuredOutput<'static> {
    StructuredOutput {
        name: "session_title",
        schema: serde_json::json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"],
            "additionalProperties": false,
        }),
    }
}

#[test]
fn empty_tool_call_declarations_are_not_tool_calls() {
    assert!(!message_has_tool_calls(None));
    assert!(!message_has_tool_calls(Some(&serde_json::Value::Null)));
    assert!(!message_has_tool_calls(Some(&serde_json::json!([]))));
    assert!(message_has_tool_calls(Some(&serde_json::json!([{
        "id": "call_1", "type": "function", "function": {"name": "Read"}
    }]))));
    assert!(message_has_tool_calls(Some(
        &serde_json::json!({"any": "shape"})
    )));
}

/// The three native shapes: chat response_format (strict), responses
/// text.format, anthropic output_config.format
#[test]
fn structured_shapes_per_format() {
    let out = output();
    let mut chat = serde_json::json!({"model": "m"});
    apply_structured(&mut chat, ApiFormat::OpenAiChat, &out);
    assert_eq!(chat["response_format"]["type"], "json_schema");
    assert_eq!(
        chat["response_format"]["json_schema"]["name"],
        "session_title"
    );
    assert_eq!(chat["response_format"]["json_schema"]["strict"], true);

    let mut responses = serde_json::json!({"model": "m"});
    apply_structured(&mut responses, ApiFormat::OpenAiResponses, &out);
    assert_eq!(responses["text"]["format"]["type"], "json_schema");
    assert_eq!(responses["text"]["format"]["strict"], true);
    assert!(responses.get("response_format").is_none());

    let mut anthropic = serde_json::json!({"model": "m"});
    apply_structured(&mut anthropic, ApiFormat::AnthropicMessages, &out);
    assert_eq!(anthropic["output_config"]["format"]["type"], "json_schema");
    assert_eq!(
        anthropic["output_config"]["format"]["schema"]["required"][0],
        "title"
    );
    assert!(anthropic.get("response_format").is_none());
}

/// The anthropic shim assigns nested: a reasoning output_config.effort
/// merged earlier survives
#[test]
fn anthropic_structured_preserves_output_config() {
    let out = output();
    let mut body = serde_json::json!({"output_config": {"effort": "high"}});
    apply_structured(&mut body, ApiFormat::AnthropicMessages, &out);
    assert_eq!(body["output_config"]["effort"], "high");
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
}

fn mock_model(port: u16) -> crate::chat::ResolvedModel {
    let mut config = crate::chat::test_model(ApiFormat::OpenAiChat);
    config.base_url = format!("http://127.0.0.1:{port}/v1");
    config
}

/// The deterministic proof is a handshake, not a wall clock: the mock
/// announces that it entered its silent phase and remains blocked (holding
/// the connection) until the test releases it. The client must return an idle
/// error while the mock is still blocked; otherwise the test fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_body_reads_have_an_idle_deadline() {
    let (port, _log, mut observer) = crate::mock::start_mock_server_with_stall_observer();
    let config = mock_model(port);
    let task = tokio::spawn(async move {
        let mut details = super::CompleteCallDetails::default();
        super::complete_text_inner(
            &config,
            crate::mock::SIDECAR_STALL_BODY_TRIGGER.to_string(),
            None,
            &tokio_util::sync::CancellationToken::new(),
            &mut details,
            std::time::Duration::from_millis(150),
        )
        .await
    });
    assert!(
        observer
            .wait_stall_started(std::time::Duration::from_secs(15))
            .await,
        "the mock never entered its body stall"
    );
    let error = task
        .await
        .expect("sidecar task joins")
        .expect_err("a stalled body must time out");
    assert!(
        matches!(
            error,
            pig_protocol::CoreError::StreamRead { ref detail } if detail.contains("idle")
        ),
        "unexpected idle error: {error:?}"
    );
    assert!(
        observer.is_stalled(),
        "the client returned while the mock was still holding the response"
    );
    observer.release();
}

/// A stalled error body that already delivered a complete typed overflow must
/// keep that classification; the read failure never replaces it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_error_body_idle_failure_keeps_context_window_classification() {
    let (port, _log, mut observer) = crate::mock::start_mock_server_with_stall_observer();
    let config = mock_model(port);
    let task = tokio::spawn(async move {
        let mut details = super::CompleteCallDetails::default();
        super::complete_text_inner(
            &config,
            crate::mock::CONTEXT_OVERFLOW_STALL_TRIGGER.to_string(),
            None,
            &tokio_util::sync::CancellationToken::new(),
            &mut details,
            std::time::Duration::from_millis(150),
        )
        .await
    });
    assert!(
        observer
            .wait_stall_started(std::time::Duration::from_secs(15))
            .await,
        "the mock never entered its error-body stall"
    );
    let error = task
        .await
        .expect("sidecar task joins")
        .expect_err("a stalled overflow error body must fail");
    assert!(
        matches!(
            error,
            pig_protocol::CoreError::ContextWindowExceeded { ref detail }
                if detail.contains("400") && detail.contains("context")
        ),
        "the partial body still classifies the overflow: {error:?}"
    );
    assert!(
        observer.is_stalled(),
        "the client returned while the mock was still holding the error body"
    );
    observer.release();
}

/// The same handshake covers cancellation: once the mock is confirmed to be
/// silent (headers or mid-body), the token is cancelled and the client must
/// return while the mock is still blocked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_cancellation_interrupts_headers_and_body_reads() {
    for trigger in [
        crate::mock::SIDECAR_STALL_HEADERS_TRIGGER,
        crate::mock::SIDECAR_STALL_BODY_TRIGGER,
    ] {
        let (port, _log, mut observer) = crate::mock::start_mock_server_with_stall_observer();
        let config = mock_model(port);
        let cancel = tokio_util::sync::CancellationToken::new();
        let token = cancel.clone();
        let task = tokio::spawn(async move {
            let mut details = super::CompleteCallDetails::default();
            super::complete_text_inner(
                &config,
                trigger.to_string(),
                None,
                &token,
                &mut details,
                std::time::Duration::from_secs(5),
            )
            .await
        });
        assert!(
            observer
                .wait_stall_started(std::time::Duration::from_secs(15))
                .await,
            "{trigger}: the mock never entered its stall"
        );
        cancel.cancel();
        let error = task
            .await
            .expect("sidecar task joins")
            .expect_err("cancellation must win over a stalled response");
        assert!(
            matches!(
                error,
                pig_protocol::CoreError::Internal { ref detail } if detail == "Cancelled"
            ),
            "{trigger}: {error:?}"
        );
        assert!(
            observer.is_stalled(),
            "{trigger}: the client returned while the mock was still holding the response"
        );
        observer.release();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_cancelled_before_send_makes_no_request() {
    let (port, log) = crate::mock::start_mock_server_with_log();
    let config = mock_model(port);
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let mut details = super::CompleteCallDetails::default();
    let error = super::complete_text_inner(
        &config,
        "never sent".to_string(),
        None,
        &cancel,
        &mut details,
        std::time::Duration::from_secs(1),
    )
    .await
    .expect_err("pre-cancelled call");
    assert!(
        matches!(
            error,
            pig_protocol::CoreError::Internal { ref detail } if detail == "Cancelled"
        ),
        "{error:?}"
    );
    assert_eq!(log.lock().expect("log").len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_retries_a_rate_limited_request() {
    let (port, log) = crate::mock::start_mock_server_with_log();
    let config = mock_model(port);
    let call = super::complete_text(
        &config,
        format!(
            "{} {}",
            crate::mock::RETRY_429_ONCE_TRIGGER,
            crate::mock::MOCK_REPLY_MARKER
        ),
        None,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("the 429 is retried and succeeds");
    assert!(!call.text.is_empty());
    assert_eq!(log.lock().expect("log").len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_retries_a_transient_500_once() {
    let (port, log) = crate::mock::start_mock_server_with_log();
    let config = mock_model(port);
    let call = super::complete_text(
        &config,
        format!("FAIL_ONCE_500 {}", crate::mock::MOCK_REPLY_MARKER),
        None,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("the transient 500 is retried and succeeds");
    assert!(!call.text.is_empty());
    assert_eq!(
        log.lock().expect("log").len(),
        2,
        "one retry after the 500 attempt"
    );
}
