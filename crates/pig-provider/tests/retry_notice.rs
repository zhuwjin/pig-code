//! Retry visibility at the wire level: a retryable 500 emits a structured
//! Retrying notice before its wait, and CallControl::retry_now skips the
//! remaining backoff so the next attempt starts immediately.

use std::time::Duration;

use pig_protocol::ApiFormat;
use pig_provider::{CallControl, ChatMsg, ProviderEvent, ResolvedModel, RetryReason, stream_chat};
use tokio_util::sync::CancellationToken;

fn mock_model(port: u16) -> ResolvedModel {
    ResolvedModel {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "mock-key".into(),
        model: "mock-model".into(),
        context_window: 128_000,
        max_output_tokens: 8_192,
        api_format: ApiFormat::OpenAiChat,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_notice_emitted_and_retry_now_skips_wait() {
    let port = pig_provider::mock::start_mock_server();
    let control = CallControl::new(CancellationToken::new());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(stream_chat(
        mock_model(port),
        vec![ChatMsg::user("FAIL_ONCE_500 read the mock file".into())],
        Vec::new(),
        tx,
        control.clone(),
    ));

    // The 500'd attempt surfaces as a notice before any content: attempt 1 of
    // the 10-retry budget, 1s exponential base, structured 5xx reason
    let notice = match rx.recv().await {
        Some(ProviderEvent::Retrying(notice)) => notice,
        other => panic!("expected Retrying first, got {other:?}"),
    };
    assert_eq!(notice.attempt, 1);
    assert_eq!(notice.max_attempts, 10);
    assert_eq!(notice.delay, Duration::from_secs(1));
    assert_eq!(notice.reason, RetryReason::Server(500));

    // Skipping the wait is proven deterministically by the paused-clock unit
    // test in retry.rs; here the retried attempt must still complete normally
    // after the trigger, with its content intact.
    control.retry_now();
    let mut saw_content = false;
    while let Some(event) = rx.recv().await {
        match event {
            ProviderEvent::ToolCalls(_) | ProviderEvent::Text(_) => saw_content = true,
            ProviderEvent::Finished(_) => break,
            ProviderEvent::Failed(error) => panic!("the retried attempt should succeed: {error:?}"),
            _ => {}
        }
    }
    assert!(saw_content, "the retried attempt should produce content");
    task.await.expect("stream_chat joins");
}
