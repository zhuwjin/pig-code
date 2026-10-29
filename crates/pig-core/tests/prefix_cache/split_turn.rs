use super::*;

/// Seed one user segment with an oversized first tool round and a small
/// retained second round. The manual compaction therefore cuts at the second
/// assistant message and preserves the active user separately.
fn seed_split_turn(fixture: &Fixture, session_id: &str, format: Format) {
    use std::io::Write as _;
    let path = fixture
        .data_dir
        .join("sessions")
        .join(format!("{session_id}.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open seeded rollout");
    let call = |id: &str| {
        serde_json::json!({
            "id": id,
            "type": "function",
            "function": {"name": "Read", "arguments": "{\"path\":\"README.mock.md\"}"},
        })
    };
    let replay = |id: &str| match format {
        Format::Chat => serde_json::Value::Null,
        Format::Responses => {
            // Durable replay identity: endpoint + model + credential
            // fingerprint. A bare base_url is a legacy origin and would never
            // replay natively.
            let origin = pig_provider::ReplayOrigin::new(
                pig_protocol::ApiFormat::OpenAiResponses,
                &fixture.base_url,
                "mock-model",
                "mock-key",
            );
            serde_json::json!({
                "origin": origin,
                "items": [
                    {
                        "type": "responses_reasoning",
                        "id": "rs_split",
                        "summary": ["SPLIT_NATIVE_REASONING"],
                        "content": [],
                    },
                    {"type": "tool_call", "call": call(id)},
                ],
            })
        }
        Format::Anthropic => {
            let origin = pig_provider::ReplayOrigin::new(
                pig_protocol::ApiFormat::AnthropicMessages,
                &fixture.base_url,
                "mock-model",
                "mock-key",
            );
            serde_json::json!({
                "origin": origin,
                "items": [
                    {
                        "type": "anthropic_thinking",
                        "thinking": "SPLIT_NATIVE_REASONING",
                        "signature": "split-signature",
                    },
                    {"type": "tool_call", "call": call(id)},
                ],
            })
        }
    };
    let assistant = |id: &str| {
        serde_json::json!({
            "type": "assistant",
            "text": "",
            "reasoning": null,
            "replay": replay(id),
            "tool_calls": [call(id)],
        })
    };
    let tool = |output: String| {
        serde_json::json!({
            "type": "tool_call",
            "tool": "Read",
            "summary": "README.mock.md",
            "arguments": "{\"path\":\"README.mock.md\"}",
            "output": output,
            "is_error": false,
            "edit": null,
            "agent_card": null,
            "agent_cards": [],
            "images": [],
            "documents": [],
        })
    };
    let user_text = "SPLIT_TURN_USER inspect the project";
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "user",
            "text": user_text,
            "files": [],
            "images": [],
            "sent_text": user_text,
            "sent_images": [],
        })
    )
    .unwrap();
    writeln!(file, "{}", assistant("split_call_1")).unwrap();
    writeln!(
        file,
        "{}",
        tool(format!("SPLIT_OMITTED_TOOL_OUTPUT {}", "o".repeat(8_000)))
    )
    .unwrap();
    writeln!(file, "{}", assistant("split_call_2")).unwrap();
    writeln!(file, "{}", tool("SPLIT_KEPT_TOOL_OUTPUT".into())).unwrap();
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "type": "assistant",
            "text": "SPLIT_FINAL_ANSWER",
            "reasoning": null,
            "tool_calls": [],
        })
    )
    .unwrap();
}

/// Tool call/result IDs of one request in the format's own shape.
fn request_tool_ids(request: &Value, format: Format) -> (Vec<String>, Vec<String>) {
    let mut calls = Vec::new();
    let mut results = Vec::new();
    match format {
        Format::Chat => {
            for message in request["messages"].as_array().expect("messages") {
                for call in message["tool_calls"].as_array().into_iter().flatten() {
                    if let Some(id) = call["id"].as_str() {
                        calls.push(id.to_string());
                    }
                }
                if let Some(id) = message["tool_call_id"].as_str() {
                    results.push(id.to_string());
                }
            }
        }
        Format::Responses => {
            for item in request["input"].as_array().expect("input") {
                match item["type"].as_str() {
                    Some("function_call") => {
                        if let Some(id) = item["call_id"].as_str() {
                            calls.push(id.to_string());
                        }
                    }
                    Some("function_call_output") => {
                        if let Some(id) = item["call_id"].as_str() {
                            results.push(id.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
        Format::Anthropic => {
            for message in request["messages"].as_array().expect("messages") {
                for block in message["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("tool_use") => {
                            if let Some(id) = block["id"].as_str() {
                                calls.push(id.to_string());
                            }
                        }
                        Some("tool_result") => {
                            if let Some(id) = block["tool_use_id"].as_str() {
                                results.push(id.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    (calls, results)
}

/// The summary prefix can end at a tool result (a split cut starts at the next
/// assistant round). The request must still be a valid user-terminated
/// conversation in each wire format: Chat tool -> instruction user, Responses
/// function_call_output -> user message, and Anthropic tool_result user turn
/// merged with the instruction (tool blocks first).
fn assert_summary_ends_after_a_tool_result(body: &str, format: Format) {
    let request: Value = serde_json::from_str(body).expect("summary request json");
    match format {
        Format::Chat => {
            let messages = request["messages"].as_array().expect("chat messages");
            let n = messages.len();
            assert_eq!(messages[n - 2]["role"], "tool", "{body}");
            assert_eq!(messages[n - 1]["role"], "user", "{body}");
            assert!(
                messages[n - 1]["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("[COMPACTION]")),
                "the instruction closes the summary request: {body}"
            );
        }
        Format::Responses => {
            let input = request["input"].as_array().expect("responses input");
            let n = input.len();
            assert_eq!(input[n - 2]["type"], "function_call_output", "{body}");
            assert_eq!(input[n - 1]["type"], "message", "{body}");
            assert_eq!(input[n - 1]["role"], "user", "{body}");
        }
        Format::Anthropic => {
            let messages = request["messages"].as_array().expect("anthropic messages");
            let n = messages.len();
            assert_eq!(messages[n - 2]["role"], "user", "{body}");
            assert_eq!(messages[n - 1]["role"], "user", "{body}");
            let blocks = messages[n - 2]["content"]
                .as_array()
                .expect("tool result blocks");
            assert_eq!(
                blocks[0]["type"], "tool_result",
                "the tool result must be the first block of the merged user turn: {body}"
            );
            assert!(
                messages[n - 1]["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("[COMPACTION]")),
                "the instruction follows as a consecutive user turn: {body}"
            );
        }
    }
}

async fn assert_split_turn_compact_restart(format: Format) {
    let fixture = Fixture::new(&format!("split-{}", format.label()), format, false);
    let agent = fixture.spawn();
    let session_id = common::new_session(&agent, fixture.workspace.clone()).await;
    shutdown(agent).await;
    seed_split_turn(&fixture, &session_id, format);

    let agent = fixture.spawn();
    open_replayed(&agent, &session_id).await;
    agent
        .ops
        .send(Op::Compact {
            session_id: session_id.clone(),
            instruction: None,
        })
        .await
        .unwrap();
    let compacted = common::recv_until(&agent.events, Duration::from_secs(10), |event| {
        matches!(event, Event::ContextCompacted { .. })
    })
    .await;
    let Some(Event::ContextCompacted {
        omitted,
        summary,
        note,
        ..
    }) = compacted.last()
    else {
        panic!("expected ContextCompacted: {compacted:#?}")
    };
    assert!(*omitted > 0, "{}: {compacted:#?}", format.label());
    assert!(
        summary
            .as_deref()
            .is_some_and(|summary| summary.contains(mock::SUMMARY_MARKER)),
        "{}: the summary sidecar succeeded: {compacted:#?}",
        format.label()
    );
    assert!(note.contains("model summary"), "{}: {note}", format.label());
    // The summary covers the omitted prefix only; the verbatim retained tail
    // is not sent to the sidecar again.
    let summary_request = fixture
        .log
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|body| body.contains("[COMPACTION]"))
        .cloned()
        .expect("summary request");
    assert!(
        summary_request.contains("SPLIT_OMITTED_TOOL_OUTPUT"),
        "{}: the omitted prefix is summarized",
        format.label()
    );
    assert!(
        !summary_request.contains("SPLIT_KEPT_TOOL_OUTPUT")
            && !summary_request.contains("SPLIT_FINAL_ANSWER"),
        "{}: the retained tail is not summarized again",
        format.label()
    );
    assert_summary_ends_after_a_tool_result(&summary_request, format);

    let before_count = main_requests(&fixture.log, format).len();
    send_and_complete(
        &agent,
        &agent.events,
        &session_id,
        "post-compact split turn",
    )
    .await;
    let requests = main_requests(&fixture.log, format);
    let before = requests
        .get(before_count)
        .expect("first post-compact request before restart")
        .clone();
    let before_text = before.to_string();
    assert!(
        before_text.contains("SPLIT_TURN_USER inspect the project"),
        "{}: the active request must survive the assistant-boundary cut",
        format.label()
    );
    assert!(
        !before_text.contains("SPLIT_OMITTED_TOOL_OUTPUT"),
        "{}: the oversized first round is omitted",
        format.label()
    );
    assert!(
        before_text.contains("SPLIT_KEPT_TOOL_OUTPUT"),
        "{}: the retained round keeps its tool result",
        format.label()
    );
    let (calls, results) = request_tool_ids(&before, format);
    assert_eq!(
        calls,
        vec!["split_call_2".to_string()],
        "{}: only the retained call remains",
        format.label()
    );
    assert_eq!(
        calls,
        results,
        "{}: every retained call has its result in order",
        format.label()
    );
    if !matches!(format, Format::Chat) {
        assert!(
            before_text.contains("SPLIT_NATIVE_REASONING"),
            "{}: the retained native replay survives compaction",
            format.label()
        );
    }
    shutdown(agent).await;

    let agent = fixture.spawn();
    open_replayed(&agent, &session_id).await;
    send_and_complete(
        &agent,
        &agent.events,
        &session_id,
        "new turn after split compact restart",
    )
    .await;
    let requests = main_requests(&fixture.log, format);
    let after = requests.last().expect("request after restart");
    assert_prefix_contract(&before, after, format, "split-turn compact");
    assert!(
        !after.to_string().contains("SPLIT_OMITTED_TOOL_OUTPUT"),
        "{}: the omitted round never resurrects",
        format.label()
    );
    let (after_calls, after_results) = request_tool_ids(after, format);
    assert!(
        after_calls.contains(&"split_call_2".to_string())
            && after_results.contains(&"split_call_2".to_string())
            && !after_calls.contains(&"split_call_1".to_string())
            && !after_results.contains(&"split_call_1".to_string()),
        "{}: restart keeps the retained pair and drops the omitted one: {after_calls:?} / {after_results:?}",
        format.label()
    );
    assert_eq!(
        after_calls
            .iter()
            .filter(|id| *id == "split_call_2")
            .count(),
        after_results
            .iter()
            .filter(|id| *id == "split_call_2")
            .count(),
        "{}: the retained tool pair stays intact after restart",
        format.label()
    );
    shutdown(agent).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_split_turn_compact_restart_preserves_prefix() {
    assert_split_turn_compact_restart(Format::Chat).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_split_turn_compact_restart_preserves_prefix() {
    assert_split_turn_compact_restart(Format::Responses).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_split_turn_compact_restart_preserves_prefix() {
    assert_split_turn_compact_restart(Format::Anthropic).await;
}
