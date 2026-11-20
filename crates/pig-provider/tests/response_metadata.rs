use pig_protocol::ApiFormat;
use pig_provider::{
    CallControl, ChatMsg, ProviderEvent, ProviderResponseMetadata, ResolvedModel,
    complete_messages, complete_text, stream_chat,
};
use tokio_util::sync::CancellationToken;

fn mock_model(port: u16, api_format: ApiFormat) -> ResolvedModel {
    ResolvedModel {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "mock-key".into(),
        model: "mock-model".into(),
        context_window: 128_000,
        max_output_tokens: 8_192,
        api_format,
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

fn assert_transport_ids(metadata: &ProviderResponseMetadata) {
    assert_eq!(
        metadata.request_id.as_deref(),
        Some(pig_provider::mock::MOCK_REQUEST_ID)
    );
    assert_eq!(
        metadata.trace_id.as_deref(),
        Some(pig_provider::mock::MOCK_TRACE_ID)
    );
    assert_eq!(
        metadata.cf_ray.as_deref(),
        Some(pig_provider::mock::MOCK_CF_RAY)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_response_ids_cover_all_formats() {
    for (format, response_id) in [
        (ApiFormat::OpenAiChat, "chatcmpl-mock"),
        (ApiFormat::AnthropicMessages, "msg-mock"),
        (ApiFormat::OpenAiResponses, "resp-mock"),
    ] {
        let port = pig_provider::mock::start_mock_server();
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let task = tokio::spawn(stream_chat(
            mock_model(port, format),
            vec![ChatMsg::user("hello".to_string())],
            Vec::new(),
            tx,
            CallControl::new(CancellationToken::new()),
        ));
        let mut latest = ProviderResponseMetadata::default();
        while let Some(event) = rx.recv().await {
            match event {
                ProviderEvent::ResponseMetadata(metadata) => latest = metadata,
                ProviderEvent::Finished(_) => break,
                ProviderEvent::Failed(error) => panic!("stream failed for {format:?}: {error:?}"),
                _ => {}
            }
        }
        task.await.expect("stream_chat joins");
        assert_eq!(latest.http_status, Some(200));
        assert_transport_ids(&latest);
        assert_eq!(latest.response_id.as_deref(), Some(response_id));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_success_returns_usage_and_response_ids() {
    for (format, response_id) in [
        (ApiFormat::OpenAiChat, "chatcmpl-mock"),
        (ApiFormat::AnthropicMessages, "msg-mock"),
        (ApiFormat::OpenAiResponses, "resp-mock"),
    ] {
        let port = pig_provider::mock::start_mock_server();
        let call = complete_text(
            &mock_model(port, format),
            "hello".to_string(),
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("sidecar call succeeds");
        assert!(!call.text.is_empty());
        let usage = call.usage.expect("mock reports usage");
        assert_eq!(usage.input, 500);
        assert_eq!(usage.output, 30);
        assert_eq!(usage.used, 530);
        assert_eq!(usage.total, 128_000);
        assert_eq!(call.response.http_status, Some(200));
        assert_transport_ids(&call.response);
        assert_eq!(call.response.response_id.as_deref(), Some(response_id));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_http_error_keeps_transport_ids() {
    for format in [
        ApiFormat::OpenAiChat,
        ApiFormat::AnthropicMessages,
        ApiFormat::OpenAiResponses,
    ] {
        let port = pig_provider::mock::start_mock_server();
        let error = complete_messages(
            &mock_model(port, format),
            &[ChatMsg::user("FAIL_COMPACT".to_string())],
            &[],
            &CancellationToken::new(),
        )
        .await
        .expect_err("mock returns HTTP 500");
        assert!(error.usage.is_none());
        assert_eq!(error.response.http_status, Some(500));
        assert_transport_ids(&error.response);
        assert_eq!(error.response.response_id, None);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_usage_only_chunk_after_finish_is_not_lost() {
    let port = pig_provider::mock::start_mock_server();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let task = tokio::spawn(stream_chat(
        mock_model(port, ApiFormat::OpenAiChat),
        vec![ChatMsg::user(
            pig_provider::mock::USAGE_TAIL_TRIGGER.to_string(),
        )],
        Vec::new(),
        tx,
        CallControl::new(CancellationToken::new()),
    ));
    let mut usage = None;
    while let Some(event) = rx.recv().await {
        match event {
            ProviderEvent::Usage {
                input,
                output,
                used,
                ..
            } => usage = Some((input, output, used)),
            ProviderEvent::Finished(_) => break,
            ProviderEvent::Failed(error) => panic!("stream failed: {error:?}"),
            _ => {}
        }
    }
    task.await.expect("stream_chat joins");
    assert_eq!(usage, Some((12, 5, 17)));
}
