use super::*;
use pig_provider::{complete_messages, complete_text, mock};

// ---------------- Sidecars ----------------

/// Live requests, sidecars and the connectivity probe all go through the one
/// provider-local URL join helper, so trailing slashes cannot select different
/// paths.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_sidecar_and_connectivity_join_urls_identically() {
    for format in [
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
    ] {
        let expected = match format {
            ApiFormat::OpenAiChat => "/v1/chat/completions",
            ApiFormat::OpenAiResponses => "/v1/responses",
            ApiFormat::AnthropicMessages => "/v1/messages",
        };
        let (port, log, paths) = mock::start_mock_server_with_paths();
        for suffix in ["", "/", "///"] {
            let base = format!("http://127.0.0.1:{port}/v1{suffix}");
            let mut config = model(port, format);
            config.base_url = base.clone();

            let before = paths.lock().expect("paths").len();
            let (body, events) = run(
                config.clone(),
                vec![ChatMsg::user("hello".into())],
                vec![],
                &log,
            )
            .await;
            assert!(body.is_some(), "{format:?}{suffix}: live body");
            assert_failure_free(&events);

            complete_text(&config, "title".into(), None, &CancellationToken::new())
                .await
                .expect("sidecar succeeds");
            assert!(
                matches!(
                    pig_provider::test_provider(&base, "mock-key", format, &config.model).await,
                    pig_protocol::ConnTestResult::Connected { .. }
                ),
                "{format:?}{suffix}: connectivity succeeds"
            );

            let logged = paths.lock().expect("paths").clone();
            let selected = &logged[before..before + 3];
            assert_eq!(
                selected,
                [expected, expected, expected],
                "{format:?} base {base:?}: live/sidecar/connectivity select the same path"
            );
            assert!(
                !selected.iter().any(|path| path.contains("//")),
                "{format:?} base {base:?}: no duplicated slash: {selected:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connectivity_chat_uses_the_endpoint_token_field() {
    let (port, log) = mock::start_mock_server_with_log();
    let result = pig_provider::test_provider(
        &format!("http://127.0.0.1:{port}/v1"),
        "mock-key",
        ApiFormat::OpenAiChat,
        "mock-model",
    )
    .await;
    assert!(
        matches!(result, pig_protocol::ConnTestResult::Connected { .. }),
        "{result:?}"
    );
    let body = last_sidecar_body(&log, 0);
    assert_eq!(body["max_tokens"], 1, "{body}");
    assert!(
        body.get("max_completion_tokens").is_none(),
        "compatible hosts keep max_tokens: {body}"
    );
}

/// The connectivity probe's error body is read with a short, cancellable
/// deadline: a stalled 500 returns the HTTP failure while the mock is still
/// holding the body, instead of inheriting the stream-scale idle timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connectivity_error_body_read_is_short_and_bounded() {
    let (port, _log, mut observer) = mock::start_mock_server_with_stall_observer();
    let task = tokio::spawn(async move {
        pig_provider::test_provider(
            &format!("http://127.0.0.1:{port}/v1"),
            "mock-key",
            ApiFormat::OpenAiChat,
            mock::CONN_TEST_ERROR_BODY_STALL_TRIGGER,
        )
        .await
    });
    assert!(
        observer
            .wait_stall_started(std::time::Duration::from_secs(15))
            .await,
        "the mock never entered its error-body stall"
    );
    match task.await.expect("probe joins") {
        pig_protocol::ConnTestResult::Failed { detail } => {
            assert!(detail.contains("500"), "the status is preserved: {detail}");
        }
        other => panic!("expected an HTTP failure, got {other:?}"),
    }
    assert!(
        observer.is_stalled(),
        "the probe returned while the mock was still holding the error body"
    );
    observer.release();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_sidecar_accepts_an_empty_tool_calls_array() {
    let (port, _log) = mock::start_mock_server_with_log();
    let config = model(port, ApiFormat::OpenAiChat);
    let call = complete_messages(
        &config,
        &[ChatMsg::user(
            mock::SIDECAR_EMPTY_TOOL_CALLS_TRIGGER.to_string(),
        )],
        &[],
        &CancellationToken::new(),
    )
    .await
    .expect("an empty tool_calls declaration is not a tool call");
    assert_eq!(call.text, mock::SIDECAR_EMPTY_TOOL_CALLS_TEXT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_sidecar_accepts_a_text_parts_array() {
    let (port, _log) = mock::start_mock_server_with_log();
    let config = model(port, ApiFormat::OpenAiChat);
    let call = complete_text(
        &config,
        mock::SIDECAR_CONTENT_ARRAY_TRIGGER.to_string(),
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("array-content sidecar succeeds");
    assert_eq!(call.text, mock::SIDECAR_CONTENT_ARRAY_TEXT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_sidecar_drops_thinking_that_cannot_fit_the_summary_cap() {
    let (port, log) = mock::start_mock_server_with_log();
    let mut config = model(port, ApiFormat::AnthropicMessages);
    config.max_output_tokens = 2048;
    config.reasoning_params = Some(json!({
        "thinking": {"type": "enabled", "budget_tokens": 4096},
        "output_config": {"effort": "high"},
    }));
    let before = log.lock().expect("log").len();
    complete_messages(
        &config,
        &[
            ChatMsg::system("s".into()),
            ChatMsg::user("history".into()),
            ChatMsg::user("summarize".into()),
        ],
        &[],
        &CancellationToken::new(),
    )
    .await
    .expect("summary must not fail on an impossible thinking budget");
    let body = last_sidecar_body(&log, before);
    assert!(
        body.get("thinking").is_none(),
        "the sidecar budget is dropped: {body}"
    );
    assert_eq!(body["output_config"]["effort"], "high");
    assert_eq!(body["max_tokens"], 2048);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_sidecar_uses_the_main_tool_and_reasoning_settings() {
    let (port, log) = mock::start_mock_server_with_log();
    let mut config = model(port, ApiFormat::OpenAiChat);
    config.prompt_cache_key = Some("pig:session-1".into());
    config.reasoning_params = Some(json!({
        "reasoning_effort": "high",
        "thinking": {"type": "enabled"},
    }));
    config.tool_settings = ToolCallSettings::new(false);
    let history = populated_history();
    let tools = vec![read_tool()];

    let (live, _) = run(config.clone(), history.clone(), tools.clone(), &log).await;
    let live = live.expect("live body");
    let mut sidecar_messages = history;
    sidecar_messages.push(ChatMsg::user("summarize".into()));
    let before = log.lock().expect("log").len();
    complete_messages(
        &config,
        &sidecar_messages,
        &tools,
        &CancellationToken::new(),
    )
    .await
    .expect("sidecar succeeds");
    let sidecar = last_sidecar_body(&log, before);

    let live_messages = live["messages"].as_array().unwrap();
    let sidecar_history = sidecar["messages"].as_array().unwrap();
    assert_eq!(
        &sidecar_history[..live_messages.len()],
        &live_messages[..],
        "the compact request repeats the same history bytes"
    );
    assert_eq!(sidecar["tools"], live["tools"]);
    assert_eq!(sidecar["parallel_tool_calls"], false, "{sidecar}");
    assert_eq!(sidecar["reasoning_effort"], "high", "{sidecar}");
    assert_eq!(sidecar["thinking"]["type"], "enabled", "{sidecar}");
    assert_eq!(sidecar["prompt_cache_key"], "pig:session-1");
    assert_eq!(sidecar["max_tokens"], live["max_tokens"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_sidecar_uses_the_main_input_tools_and_include() {
    let (port, log) = mock::start_mock_server_with_log();
    let mut config = model(port, ApiFormat::OpenAiResponses);
    config.cap_web_search = true;
    config.prompt_cache_key = Some("pig:session-1".into());
    config.reasoning_params = Some(json!({"reasoning": {"effort": "high", "summary": "auto"}}));
    let history = populated_history();
    let tools = vec![read_tool()];

    let (live, _) = run(config.clone(), history.clone(), tools.clone(), &log).await;
    let live = live.expect("live body");
    let mut sidecar_messages = history;
    sidecar_messages.push(ChatMsg::user("summarize".into()));
    let before = log.lock().expect("log").len();
    complete_messages(
        &config,
        &sidecar_messages,
        &tools,
        &CancellationToken::new(),
    )
    .await
    .expect("sidecar succeeds");
    let sidecar = last_sidecar_body(&log, before);

    let live_input = live["input"].as_array().unwrap();
    let sidecar_input = sidecar["input"].as_array().unwrap();
    assert_eq!(&sidecar_input[..live_input.len()], &live_input[..]);
    assert_eq!(sidecar["instructions"], live["instructions"]);
    assert_eq!(sidecar["tools"], live["tools"]);
    assert_eq!(sidecar["include"], live["include"]);
    assert_eq!(sidecar["reasoning"], live["reasoning"]);
    assert_eq!(sidecar["tool_choice"], "auto");
    assert_eq!(sidecar["parallel_tool_calls"], true);
    assert_eq!(sidecar["prompt_cache_key"], "pig:session-1");
}
