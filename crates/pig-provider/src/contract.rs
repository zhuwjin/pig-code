use std::time::Duration;

use pig_protocol::{ApiFormat, CoreError, FinishReason};
use tokio_util::sync::CancellationToken;

use crate::{CallControl, ProviderEvent, RetryReason, stream_chat};

fn contract_model(port: u16, format: ApiFormat) -> crate::ResolvedModel {
    let mut model = crate::chat::test_model(format);
    model.base_url = format!("http://127.0.0.1:{port}/v1");
    model.api_key = "mock-key".into();
    model.cap_structured = true;
    model
}

async fn collect(
    model: crate::ResolvedModel,
    messages: Vec<crate::ChatMsg>,
    control: CallControl,
) -> Vec<ProviderEvent> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    stream_chat(model, messages, Vec::new(), tx, control).await;
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

fn text_events(events: &[ProviderEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn run_contract(format: ApiFormat) {
    let (port, requests) = crate::mock::start_mock_server_with_log();
    let model = contract_model(port, format);
    let normal = collect(
        model.clone(),
        vec![
            crate::ChatMsg::user("read the mock file and summarize".into()),
            crate::ChatMsg::tool_result("call_read", crate::mock::MOCK_FILE_CONTENT.into()),
        ],
        CallControl::new(CancellationToken::new()),
    )
    .await;
    assert!(text_events(&normal).contains(crate::mock::MOCK_REPLY_MARKER));
    let reasoning = collect(
        model.clone(),
        vec![crate::ChatMsg::user(
            "read the mock file and summarize".into(),
        )],
        CallControl::new(CancellationToken::new()),
    )
    .await;
    assert!(
        reasoning
            .iter()
            .any(|event| matches!(event, ProviderEvent::Reasoning(_)))
    );
    assert!(
        reasoning
            .iter()
            .any(|event| matches!(event, ProviderEvent::ToolCalls(_)))
    );
    assert!(
        normal
            .iter()
            .any(|event| matches!(event, ProviderEvent::Usage { .. }))
    );
    assert!(
        normal
            .iter()
            .any(|event| matches!(event, ProviderEvent::Finished(FinishReason::Stop)))
    );

    let mut media = crate::ChatMsg::user("media input".into());
    media.images.push(crate::ChatImage {
        media_type: "image/png".into(),
        data_base64: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNoaGj4DwAFhAKAjM1mJgAAAABJRU5ErkJggg==".into(),
        label: Some("pixel.png".into()),
    });
    collect(
        model.clone(),
        vec![media],
        CallControl::new(CancellationToken::new()),
    )
    .await;
    assert!(requests.lock().expect("request log").iter().any(|body| {
        body.contains("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNoaGj4DwAFhAKAjM1mJgAAAABJRU5ErkJggg==")
    }));

    let structured = collect(
        model.clone(),
        vec![crate::ChatMsg::user(
            crate::mock::STRUCTURED_OUTPUT_TRIGGER.into(),
        )],
        CallControl::new(CancellationToken::new()).with_structured_output(serde_json::json!({
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"]
        })),
    )
    .await;
    assert!(
        text_events(&structured).contains("\"answer\":\"ok\""),
        "structured events: {structured:#?}"
    );
    let requests = requests.lock().expect("request log");
    let schema_registered = requests.iter().any(|body| {
        if format == ApiFormat::AnthropicMessages {
            body.contains("output_config")
                && body.contains("json_schema")
                && body.contains("answer")
        } else {
            body.contains("agent_result") && body.contains("answer")
        }
    });
    assert!(schema_registered, "structured requests: {requests:#?}");
    drop(requests);

    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(stream_chat(
        model.clone(),
        vec![crate::ChatMsg::user(
            crate::mock::STALL_STREAM_TRIGGER.into(),
        )],
        Vec::new(),
        tx,
        CallControl::new(cancel.clone()),
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Some(ProviderEvent::Text(_)) => break,
                Some(ProviderEvent::Failed(error)) => {
                    panic!("stream failed before cancel: {error:?}")
                }
                Some(_) => {}
                None => panic!("stream ended before cancellation"),
            }
        }
    })
    .await
    .expect("text before cancellation");
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("cancellation must return the provider call")
        .expect("cancelled stream task");

    let timeout = collect(
        model.clone(),
        vec![crate::ChatMsg::user(
            crate::mock::STALL_STREAM_TRIGGER.into(),
        )],
        CallControl::new(CancellationToken::new()).with_idle_timeout(Duration::from_millis(50)),
    )
    .await;
    assert!(timeout.iter().any(|event| matches!(
        event,
        ProviderEvent::Failed(CoreError::StreamRead { detail }) if detail.contains("idle")
    )));

    let business = collect(
        model.clone(),
        vec![crate::ChatMsg::user(
            crate::mock::BUSINESS_ERROR_TRIGGER.into(),
        )],
        CallControl::new(CancellationToken::new()),
    )
    .await;
    assert!(business.iter().any(|event| matches!(
        event,
        ProviderEvent::Failed(CoreError::Internal { detail })
            if detail.contains(crate::mock::BUSINESS_ERROR_MARKER)
    )));

    let retry_port = retry_server(format);
    let retried = collect(
        contract_model(retry_port, format),
        vec![crate::ChatMsg::user("retry contract".into())],
        CallControl::new(CancellationToken::new()),
    )
    .await;
    assert!(retried.iter().any(|event| matches!(
        event,
        ProviderEvent::Retrying(notice) if matches!(notice.reason, RetryReason::RateLimit { .. })
    )));
    assert!(text_events(&retried).contains("contract retry ok"));
    assert!(
        retried
            .iter()
            .any(|event| matches!(event, ProviderEvent::Finished(FinishReason::Stop)))
    );
}

#[tokio::test]
async fn openai_chat_provider_contract() {
    tokio::time::timeout(Duration::from_secs(30), run_contract(ApiFormat::OpenAiChat))
        .await
        .expect("OpenAI Chat contract timeout");
}

#[tokio::test]
async fn openai_responses_provider_contract() {
    tokio::time::timeout(
        Duration::from_secs(30),
        run_contract(ApiFormat::OpenAiResponses),
    )
    .await
    .expect("OpenAI Responses contract timeout");
}

#[tokio::test]
async fn anthropic_messages_provider_contract() {
    tokio::time::timeout(
        Duration::from_secs(30),
        run_contract(ApiFormat::AnthropicMessages),
    )
    .await
    .expect("Anthropic Messages contract timeout");
}

fn retry_server(format: ApiFormat) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("retry listener");
    let port = listener.local_addr().expect("retry address").port();
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("retry connection");
            let mut request = vec![0; 4096];
            let _ = stream.read(&mut request).await;
            let response = if attempt == 0 {
                "HTTP/1.1 429 Too Many Requests\r\nretry-after: 0\r\ncontent-length: 2\r\n\r\n{}"
                    .to_string()
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{}",
                    retry_payload(format)
                )
            };
            stream
                .write_all(response.as_bytes())
                .await
                .expect("retry response");
            let _ = stream.shutdown().await;
        }
    });
    port
}

fn retry_payload(format: ApiFormat) -> String {
    match format {
        ApiFormat::OpenAiChat => concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"contract retry ok\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        )
        .into(),
        ApiFormat::OpenAiResponses => concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"contract retry ok\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-retry\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
        )
        .into(),
        ApiFormat::AnthropicMessages => concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-retry\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"mock-model\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"contract retry ok\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        )
        .into(),
    }
}

#[cfg(test)]
mod backpressure_tests {
    use super::*;

    #[tokio::test]
    async fn provider_events_wait_for_bounded_capacity() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let permit = tx.reserve().await.expect("reserve the only slot");
        let sender = tx.clone();
        let pending = tokio::spawn(async move {
            crate::chat::finish(&sender, Vec::new(), pig_protocol::FinishReason::Stop).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!pending.is_finished(), "the event bypassed a full queue");
        drop(permit);
        assert!(matches!(
            rx.recv().await,
            Some(ProviderEvent::Finished(pig_protocol::FinishReason::Stop))
        ));
        pending.await.expect("event sent after capacity");
    }
}
