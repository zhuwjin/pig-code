use super::*;
use pig_provider::{complete_messages, mock};

// ---------------- PDF documents ----------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn documents_project_to_each_wire_format() {
    let (port, log) = mock::start_mock_server_with_log();
    let messages = vec![
        {
            let mut user = ChatMsg::user("see attached".into());
            user.documents = vec![pdf("first.pdf")];
            user
        },
        ChatMsg::assistant(
            String::new(),
            vec![ToolCall {
                id: "call-1".into(),
                name: "Read".into(),
                arguments: "{}".into(),
            }],
            None,
        ),
        {
            let mut tool = ChatMsg::tool_result("call-1", "ok".into());
            tool.documents = vec![pdf("second.pdf")];
            tool
        },
    ];

    for format in [
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
    ] {
        let config = model(port, format);
        let (body, events) = run(config, messages.clone(), vec![read_tool()], &log).await;
        let body = body.expect("body");
        assert_failure_free(&events);
        match format {
            ApiFormat::OpenAiChat => {
                let wire = body["messages"].as_array().unwrap();
                let user_parts = wire[0]["content"].as_array().unwrap();
                assert_eq!(user_parts[0]["type"], "text");
                assert_eq!(user_parts[1]["type"], "file");
                assert_eq!(user_parts[1]["file"]["filename"], "first.pdf");
                assert_eq!(
                    user_parts[1]["file"]["file_data"],
                    "data:application/pdf;base64,JVBERi0xLjQK"
                );
                assert_eq!(wire[2]["role"], "tool");
                assert_eq!(wire[3]["role"], "user");
                assert_eq!(wire[3]["content"][0]["type"], "file");
                assert_eq!(wire[3]["content"][0]["file"]["filename"], "second.pdf");
                assert!(!body.to_string().contains("\"documents\""), "{body}");
            }
            ApiFormat::OpenAiResponses => {
                let wire = body["input"].as_array().unwrap();
                let user_parts = wire[0]["content"].as_array().unwrap();
                assert_eq!(user_parts[0]["type"], "input_text");
                assert_eq!(user_parts[1]["type"], "input_file");
                assert_eq!(user_parts[1]["filename"], "first.pdf");
                assert_eq!(
                    user_parts[1]["file_data"], "data:application/pdf;base64,JVBERi0xLjQK",
                    "Responses file_data is a data URL"
                );
                let output = wire
                    .iter()
                    .find(|item| item["type"] == "function_call_output")
                    .expect("function_call_output");
                assert_eq!(output["output"], "ok");
                let document_message = wire
                    .iter()
                    .rev()
                    .find(|item| item["type"] == "message" && item["role"] == "user")
                    .expect("document message");
                assert_eq!(document_message["content"][0]["type"], "input_file");
                assert_eq!(document_message["content"][0]["filename"], "second.pdf");
            }
            ApiFormat::AnthropicMessages => {
                let wire = body["messages"].as_array().unwrap();
                let user_blocks = wire[0]["content"].as_array().unwrap();
                assert_eq!(user_blocks[0]["type"], "document");
                assert_eq!(user_blocks[0]["source"]["type"], "base64");
                assert_eq!(user_blocks[0]["source"]["media_type"], "application/pdf");
                assert_eq!(user_blocks[0]["source"]["data"], "JVBERi0xLjQK");
                assert_eq!(user_blocks[0]["title"], "first.pdf");
                assert_eq!(user_blocks[1]["type"], "text");
                let tool_blocks = wire[2]["content"].as_array().unwrap();
                assert_eq!(
                    tool_blocks.len(),
                    1,
                    "the document belongs to the tool result: {tool_blocks:?}"
                );
                let result = &tool_blocks[0];
                assert_eq!(result["type"], "tool_result");
                let content = result["content"].as_array().expect("nested content");
                assert_eq!(content[0]["type"], "document");
                assert_eq!(content[0]["source"]["data"], "JVBERi0xLjQK");
                assert_eq!(content[1]["type"], "text");
                assert_eq!(content[1]["text"], "ok");
                assert!(
                    !body.to_string().contains("data:application/pdf"),
                    "Anthropic documents are bare base64: {body}"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_media_only_turns_have_no_empty_text_blocks_on_the_wire() {
    let (port, log) = mock::start_mock_server_with_log();
    let image = pig_provider::ChatImage {
        media_type: "image/png".into(),
        data_base64: "QUJD".into(),
        label: None,
    };
    let mut user = ChatMsg::user(String::new());
    user.images = vec![image.clone()];
    let mut result = ChatMsg::tool_result_with_images("call-1", String::new(), vec![image]);
    result.documents = vec![pdf_with("empty-output.pdf", "JVBERi0xLjQK", None)];
    let messages = vec![
        user,
        ChatMsg::assistant(
            String::new(),
            vec![ToolCall {
                id: "call-1".into(),
                name: "ReadMediaFile".into(),
                arguments: "{}".into(),
            }],
            None,
        ),
        result,
    ];
    let (body, events) = run(
        model(port, ApiFormat::AnthropicMessages),
        messages,
        vec![],
        &log,
    )
    .await;
    let body = body.expect("body");
    assert_failure_free(&events);
    let wire = body["messages"].as_array().unwrap();
    let user_blocks = wire[0]["content"].as_array().unwrap();
    assert_eq!(user_blocks.len(), 1, "{user_blocks:?}");
    assert_eq!(user_blocks[0]["type"], "image");
    let result_blocks = wire[2]["content"].as_array().unwrap();
    assert_eq!(
        result_blocks.len(),
        1,
        "no sibling document: {result_blocks:?}"
    );
    assert_eq!(result_blocks[0]["type"], "tool_result");
    let content = result_blocks[0]["content"].as_array().unwrap();
    assert_eq!(
        content
            .iter()
            .map(|block| block["type"].as_str().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["image", "document"],
        "media only, no empty text: {content:?}"
    );
    mock::validate_anthropic_request(&body).expect("valid Anthropic wire shape");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_tool_results_with_documents_stay_paired() {
    let (port, log) = mock::start_mock_server_with_log();
    let messages = vec![
        ChatMsg::user("read two pdfs".into()),
        ChatMsg::assistant(
            String::new(),
            vec![
                ToolCall {
                    id: "call-1".into(),
                    name: "ReadMediaFile".into(),
                    arguments: "{}".into(),
                },
                ToolCall {
                    id: "call-2".into(),
                    name: "ReadMediaFile".into(),
                    arguments: "{}".into(),
                },
            ],
            None,
        ),
        tool_result_with_pdf(
            "call-1",
            "first ok",
            pdf_with("first.pdf", "JVBERi0xLjQK", Some("First PDF")),
        ),
        tool_result_with_pdf(
            "call-2",
            "second ok",
            pdf_with("second.pdf", "JVBERi0xLjUK", Some("Second PDF")),
        ),
    ];
    let tools = vec![read_tool()];

    for format in [
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
    ] {
        let (body, events) = run(model(port, format), messages.clone(), tools.clone(), &log).await;
        let body = body.expect("body");
        assert_failure_free(&events);
        match format {
            ApiFormat::OpenAiChat => {
                let wire = body["messages"].as_array().unwrap();
                assert_eq!(wire.len(), 6, "two tool texts + two media messages: {body}");
                let calls = wire[1]["tool_calls"].as_array().unwrap();
                assert_eq!(calls[0]["id"], "call-1");
                assert_eq!(calls[1]["id"], "call-2");
                assert_eq!(wire[2]["role"], "tool");
                assert_eq!(wire[2]["tool_call_id"], "call-1");
                assert_eq!(wire[2]["content"], "first ok");
                assert_eq!(
                    wire[3]["content"],
                    json!([{
                        "type": "file",
                        "file": {
                            "filename": "first.pdf",
                            "file_data": "data:application/pdf;base64,JVBERi0xLjQK",
                        },
                    }])
                );
                assert_eq!(wire[4]["role"], "tool");
                assert_eq!(wire[4]["tool_call_id"], "call-2");
                assert_eq!(wire[4]["content"], "second ok");
                assert_eq!(
                    wire[5]["content"],
                    json!([{
                        "type": "file",
                        "file": {
                            "filename": "second.pdf",
                            "file_data": "data:application/pdf;base64,JVBERi0xLjUK",
                        },
                    }])
                );
            }
            ApiFormat::OpenAiResponses => {
                let input = body["input"].as_array().unwrap();
                let call_ids: Vec<_> = input
                    .iter()
                    .filter(|item| item["type"] == "function_call")
                    .map(|item| item["call_id"].as_str().unwrap())
                    .collect();
                assert_eq!(call_ids, vec!["call-1", "call-2"]);

                let first_output = input
                    .iter()
                    .position(|item| {
                        item["type"] == "function_call_output" && item["call_id"] == "call-1"
                    })
                    .expect("first output");
                let second_output = input
                    .iter()
                    .position(|item| {
                        item["type"] == "function_call_output" && item["call_id"] == "call-2"
                    })
                    .expect("second output");
                assert!(first_output < second_output);
                assert_eq!(input[first_output]["output"], "first ok");
                assert_eq!(input[first_output + 1]["role"], "user");
                assert_eq!(
                    input[first_output + 1]["content"],
                    json!([{
                        "type": "input_file",
                        "filename": "first.pdf",
                        "file_data": "data:application/pdf;base64,JVBERi0xLjQK",
                    }])
                );
                assert_eq!(input[second_output]["output"], "second ok");
                assert_eq!(input[second_output + 1]["role"], "user");
                assert_eq!(
                    input[second_output + 1]["content"],
                    json!([{
                        "type": "input_file",
                        "filename": "second.pdf",
                        "file_data": "data:application/pdf;base64,JVBERi0xLjUK",
                    }])
                );
            }
            ApiFormat::AnthropicMessages => {
                let wire = body["messages"].as_array().unwrap();
                assert_eq!(wire.len(), 3, "the two tool results share one user turn");
                let calls = wire[1]["content"].as_array().unwrap();
                assert_eq!(calls[0]["id"], "call-1");
                assert_eq!(calls[1]["id"], "call-2");
                let blocks = wire[2]["content"].as_array().unwrap();
                assert_eq!(
                    blocks.len(),
                    2,
                    "one tool_result per call, no sibling documents: {blocks:?}"
                );
                for (block, expected_id, expected_title, expected_data) in [
                    (&blocks[0], "call-1", "first.pdf", "JVBERi0xLjQK"),
                    (&blocks[1], "call-2", "second.pdf", "JVBERi0xLjUK"),
                ] {
                    assert_eq!(block["type"], "tool_result");
                    assert_eq!(block["tool_use_id"], expected_id);
                    let content = block["content"].as_array().expect("owned content");
                    assert_eq!(content.len(), 2, "document + text: {content:?}");
                    assert_eq!(content[0]["type"], "document");
                    assert_eq!(content[0]["title"], expected_title);
                    assert_eq!(content[0]["source"]["data"], expected_data);
                    assert_eq!(content[1]["type"], "text");
                    assert!(
                        content[1]["text"]
                            .as_str()
                            .is_some_and(|text| text.ends_with("ok")),
                        "the owning result text stays nested: {content:?}"
                    );
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_and_compact_pdf_prefixes_are_exact() {
    let (port, log) = mock::start_mock_server_with_log();
    let history = vec![
        ChatMsg::system("frozen".into()),
        user_with_pdf(
            "see spec",
            pdf_with("spec.pdf", "JVBERi0xLjQK", Some("Spec PDF")),
        ),
        ChatMsg::assistant(
            String::new(),
            vec![ToolCall {
                id: "call-1".into(),
                name: "ReadMediaFile".into(),
                arguments: "{}".into(),
            }],
            None,
        ),
        tool_result_with_pdf(
            "call-1",
            "ok",
            pdf_with("result.pdf", "JVBERi0xLjUK", Some("Result PDF")),
        ),
    ];
    let tools = vec![read_tool()];

    for format in [
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
    ] {
        let mut config = model(port, format);
        config.input_pdf = true;
        if format == ApiFormat::AnthropicMessages {
            config.anthropic_cache_control = true;
        }
        let (live, live_events) = run(config.clone(), history.clone(), tools.clone(), &log).await;
        let live = live.expect("live body");
        assert_failure_free(&live_events);

        let mut sidecar_messages = history.clone();
        sidecar_messages.push(ChatMsg::user("summarize".into()));
        let before = log.lock().expect("log").len();
        complete_messages(
            &config,
            &sidecar_messages,
            &tools,
            &CancellationToken::new(),
        )
        .await
        .expect("summary succeeds");
        let sidecar = last_sidecar_body(&log, before);

        match format {
            ApiFormat::OpenAiChat => {
                let live_wire = live["messages"].as_array().unwrap();
                let sidecar_wire = sidecar["messages"].as_array().unwrap();
                assert_eq!(sidecar_wire.len(), live_wire.len() + 1);
                assert_eq!(
                    &sidecar_wire[..live_wire.len()],
                    &live_wire[..],
                    "Chat keeps the full document prefix byte-identical"
                );
                assert_eq!(
                    live_wire[1]["content"],
                    json!([
                        {"type": "text", "text": "see spec"},
                        {"type": "file", "file": {
                            "filename": "spec.pdf",
                            "file_data": "data:application/pdf;base64,JVBERi0xLjQK",
                        }},
                    ])
                );
                assert_eq!(live_wire[3]["role"], "tool");
                assert_eq!(live_wire[3]["tool_call_id"], "call-1");
                assert_eq!(
                    live_wire[4]["content"],
                    json!([{
                        "type": "file",
                        "file": {
                            "filename": "result.pdf",
                            "file_data": "data:application/pdf;base64,JVBERi0xLjUK",
                        },
                    }])
                );
                assert_eq!(sidecar["tools"], live["tools"]);
            }
            ApiFormat::OpenAiResponses => {
                let live_input = live["input"].as_array().unwrap();
                let sidecar_input = sidecar["input"].as_array().unwrap();
                assert_eq!(sidecar_input.len(), live_input.len() + 1);
                assert_eq!(
                    &sidecar_input[..live_input.len()],
                    &live_input[..],
                    "Responses keeps the full document prefix byte-identical"
                );
                assert_eq!(sidecar["instructions"], live["instructions"]);
                assert_eq!(sidecar["tools"], live["tools"]);
                assert_eq!(
                    live_input[0]["content"],
                    json!([
                        {"type": "input_text", "text": "see spec"},
                        {"type": "input_file", "filename": "spec.pdf",
                         "file_data": "data:application/pdf;base64,JVBERi0xLjQK"},
                    ])
                );
                let output = live_input
                    .iter()
                    .position(|item| item["type"] == "function_call_output")
                    .expect("function output");
                assert_eq!(
                    live_input[output + 1]["content"],
                    json!([{
                        "type": "input_file",
                        "filename": "result.pdf",
                        "file_data": "data:application/pdf;base64,JVBERi0xLjUK",
                    }])
                );
            }
            ApiFormat::AnthropicMessages => {
                let live_messages = live["messages"].as_array().unwrap();
                let sidecar_messages = sidecar["messages"].as_array().unwrap();
                assert_eq!(sidecar_messages.len(), live_messages.len() + 1);
                assert_eq!(
                    &sidecar_messages[..live_messages.len()],
                    &live_messages[..],
                    "Anthropic keeps the document prefix (including the cache anchor) identical"
                );
                assert_eq!(sidecar["system"], live["system"]);
                assert_eq!(sidecar["tools"], live["tools"]);
                assert_eq!(
                    live_messages[0]["content"],
                    json!([
                        {"type": "text", "text": "[Spec PDF]"},
                        {"type": "document", "source": {
                            "type": "base64",
                            "media_type": "application/pdf",
                            "data": "JVBERi0xLjQK",
                        }, "title": "spec.pdf"},
                        {"type": "text", "text": "see spec"},
                    ])
                );
                assert_eq!(
                    live_messages[2]["content"],
                    json!([{
                        "type": "tool_result",
                        "tool_use_id": "call-1",
                        "content": [
                            {"type": "document", "source": {
                                "type": "base64",
                                "media_type": "application/pdf",
                                "data": "JVBERi0xLjUK",
                            }, "title": "result.pdf"},
                            {"type": "text", "text": "ok"},
                        ],
                        "cache_control": {"type": "ephemeral"},
                    }])
                );
                let instruction = sidecar_messages.last().unwrap();
                assert!(
                    !instruction.to_string().contains("cache_control"),
                    "the appended compact instruction stays unmarked: {instruction}"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn documents_capability_off_fails_before_http() {
    let (port, log) = mock::start_mock_server_with_log();
    for format in [
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
    ] {
        let mut config = model(port, format);
        config.input_pdf = false;
        let mut user = ChatMsg::user("see attached".into());
        user.documents = vec![pdf("first.pdf")];
        let before = log.lock().expect("log").len();
        let (body, events) = run(config, vec![user], vec![], &log).await;
        assert!(body.is_none(), "{format:?}: must fail before HTTP");
        assert_eq!(
            log.lock().expect("log").len(),
            before,
            "{format:?}: no request may reach the mock"
        );
        let detail = format!("{:?}", failed_error(&events).expect("failure"));
        assert!(
            detail.contains("PDF") && detail.contains("input_pdf"),
            "{format:?}: {detail}"
        );
    }

    let config = model(port, ApiFormat::OpenAiChat);
    let mut user = ChatMsg::user("see attached".into());
    user.documents = vec![ChatDocument {
        media_type: "text/plain".into(),
        data_base64: "aGVsbG8=".into(),
        filename: "notes.txt".into(),
        label: None,
        token_estimate: None,
    }];
    let (body, events) = run(config, vec![user], vec![], &log).await;
    assert!(body.is_none(), "non-PDF documents must fail before HTTP");
    let detail = format!("{:?}", failed_error(&events).expect("failure"));
    assert!(
        detail.contains("Unsupported document media type"),
        "{detail}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_pdf_beta_merges_with_the_thinking_beta_over_http() {
    let (port, log, headers) = mock::start_mock_server_with_headers();
    let mut config = model(port, ApiFormat::AnthropicMessages);
    config.reasoning_params = Some(json!({"thinking": {"type": "enabled"}}));
    let mut user = ChatMsg::user("see attached".into());
    user.documents = vec![pdf("first.pdf")];
    let (body, events) = run(config, vec![user], vec![], &log).await;
    assert!(body.is_some());
    assert_failure_free(&events);
    let index = log.lock().expect("log").len() - 1;
    let beta = headers.lock().expect("headers")[index]
        .iter()
        .find(|(name, _)| name == "anthropic-beta")
        .map(|(_, value)| value.clone())
        .expect("anthropic-beta header");
    assert_eq!(
        beta, "interleaved-thinking-2025-05-14,pdfs-2024-09-25",
        "PDF and thinking betas share one merged header"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_with_documents_keeps_the_document_blocks() {
    let (port, log) = mock::start_mock_server_with_log();
    for format in [
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
    ] {
        let config = model(port, format);
        let mut user = ChatMsg::user("see attached".into());
        user.documents = vec![pdf("first.pdf")];
        let before = log.lock().expect("log").len();
        complete_messages(
            &config,
            &[ChatMsg::system("s".into()), user],
            &[],
            &CancellationToken::new(),
        )
        .await
        .expect("summary sidecar with a document succeeds");
        let body = last_sidecar_body(&log, before);
        assert!(
            body.to_string().contains("first.pdf"),
            "{format:?}: the compact projection keeps the document: {body}"
        );
    }
}
