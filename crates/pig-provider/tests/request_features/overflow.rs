use super::*;
use pig_protocol::FinishReason;
use pig_provider::{complete_messages, mock};

// ---------------- Context-window classification ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_window_overflow_is_typed_for_http_and_in_band_errors() {
    let (port, log) = mock::start_mock_server_with_log();
    for format in [
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
    ] {
        for trigger in [
            mock::CONTEXT_OVERFLOW_HTTP_TRIGGER,
            mock::CONTEXT_OVERFLOW_STREAM_TRIGGER,
            mock::CONTEXT_OVERFLOW_KIMI_TRIGGER,
            mock::CONTEXT_OVERFLOW_KIMI_STREAM_TRIGGER,
            mock::CONTEXT_OVERFLOW_GLM_TRIGGER,
            mock::CONTEXT_OVERFLOW_GLM_STREAM_TRIGGER,
        ] {
            let (body, events) = run(
                model(port, format),
                vec![ChatMsg::user(trigger.to_string())],
                vec![],
                &log,
            )
            .await;
            assert!(body.is_some(), "{format:?}/{trigger}: the request is sent");
            assert!(
                matches!(
                    failed_error(&events),
                    Some(CoreError::ContextWindowExceeded { .. })
                ),
                "{format:?}/{trigger}: {:?}",
                failed_error(&events)
            );
        }

        let before = log.lock().expect("log").len();
        let error = complete_messages(
            &model(port, format),
            &[ChatMsg::user(
                mock::CONTEXT_OVERFLOW_HTTP_TRIGGER.to_string(),
            )],
            &[],
            &CancellationToken::new(),
        )
        .await
        .expect_err("sidecar overflow");
        assert!(
            matches!(error.error, CoreError::ContextWindowExceeded { .. }),
            "{format:?}: {:?}",
            error.error
        );
        assert_eq!(log.lock().expect("log").len(), before + 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_failures_and_length_completions_are_not_overflow() {
    let (port, log) = mock::start_mock_server_with_log();
    for format in [
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
    ] {
        let (_, events) = run(
            model(port, format),
            vec![ChatMsg::user(mock::FINISH_LENGTH_TRIGGER.to_string())],
            vec![],
            &log,
        )
        .await;
        assert!(
            failed_error(&events).is_none(),
            "{format:?}: a length completion is not an error: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderEvent::Finished(FinishReason::Length))),
            "{format:?}: length must stay a finish reason: {events:?}"
        );
    }

    let error = complete_messages(
        &model(port, ApiFormat::OpenAiChat),
        &[ChatMsg::user("FAIL_COMPACT".into())],
        &[],
        &CancellationToken::new(),
    )
    .await
    .expect_err("ordinary 500");
    assert!(
        matches!(error.error, CoreError::Internal { .. }),
        "an ordinary 500 is not an overflow: {:?}",
        error.error
    );
}
