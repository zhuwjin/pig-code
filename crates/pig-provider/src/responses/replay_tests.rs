use super::request::to_responses_input;
use crate::chat::{AssistantReplay, ChatMsg, ResolvedModel, ToolCall};

fn finish_frames(
    frames: Vec<serde_json::Value>,
) -> (ResolvedModel, AssistantReplay, Vec<ToolCall>) {
    let config = crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses);
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let mut tool_stream = crate::tool_stream::ToolCallStream::new();
    let mut replay_items = Vec::new();
    let mut live = super::live::LiveResponsesText::default();
    let mut finish_reason = None;
    let mut api = crate::api_log::ApiCall::new("test", "test", "test");
    let mut metadata = crate::ProviderResponseMetadata::default();
    let mut completed_output = None;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        for frame in frames {
            super::stream::handle_frame(
                &frame.to_string(),
                &tx,
                &mut tool_stream,
                &mut replay_items,
                &mut live,
                &mut finish_reason,
                &config,
                &mut api,
                &mut metadata,
                &mut completed_output,
            )
            .await
            .expect("frame accepted");
        }
        super::stream::finish_responses_stream(
            tool_stream,
            replay_items,
            live,
            finish_reason,
            completed_output,
            &config,
            &mut api,
            &tx,
        )
        .await
        .expect("stream finishes");
    });

    let mut replay = None;
    let mut calls = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            crate::ProviderEvent::AssistantReplay(snapshot) => replay = Some(snapshot),
            crate::ProviderEvent::ToolCalls(final_calls) => calls = final_calls,
            _ => {}
        }
    }
    (config, replay.expect("native replay"), calls)
}

#[test]
fn encrypted_include_merges_without_duplicates() {
    let mut config = crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses);
    config.base_url = "https://api.openai.com/v1".into();
    let mut body = serde_json::json!({
        "reasoning": {"effort": "high"},
        "include": ["reasoning.custom"],
    });
    super::search::ensure_include(&mut body, &config, true);
    super::search::ensure_include(&mut body, &config, true);
    assert_eq!(
        body["include"],
        serde_json::json!([
            "reasoning.custom",
            "reasoning.encrypted_content",
            "web_search_call.action.sources"
        ]),
        "include entries merge without duplicates"
    );

    config.base_url = "http://localhost/v1".into();
    let mut compat = serde_json::json!({"reasoning": {"effort": "high"}});
    super::search::ensure_include(&mut compat, &config, false);
    assert!(compat.get("include").is_none());
}

#[test]
fn native_replay_filters_official_and_cross_origin_items() {
    let mut config = crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses);
    config.base_url = "https://api.openai.com/v1".into();
    let replay = crate::chat::AssistantReplay::new(
        &config,
        vec![
            crate::chat::AssistantReplayItem::ResponsesReasoning {
                id: Some("rs_plain".into()),
                summary: vec!["plain".into()],
                content: vec![],
                encrypted_content: None,
            },
            crate::chat::AssistantReplayItem::ResponsesReasoning {
                id: Some("rs_encrypted".into()),
                summary: vec!["summary".into()],
                content: vec!["content".into()],
                encrypted_content: Some("secret".into()),
            },
            crate::chat::AssistantReplayItem::Text {
                text: "answer".into(),
                citations: Vec::new(),
            },
            crate::chat::AssistantReplayItem::AnthropicThinking {
                thinking: "wrong-format".into(),
                signature: Some("must-not-leak".into()),
            },
        ],
    );
    let messages = vec![ChatMsg::assistant_with_replay(
        String::new(),
        vec![],
        Some("summary content".into()),
        Some(replay),
    )];
    let (_, input) = to_responses_input(&config, &messages);
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["id"], "rs_encrypted");
    assert_eq!(input[0]["encrypted_content"], "secret");
    assert_eq!(input[0]["content"][0]["type"], "reasoning_text");
    assert_eq!(input[1]["type"], "message");
    let serialized = serde_json::to_string(&input).unwrap();
    assert!(!serialized.contains("must-not-leak"));

    let (_, legacy) = to_responses_input(
        &config,
        &[ChatMsg::assistant(
            "answer".into(),
            vec![],
            Some("legacy plaintext".into()),
        )],
    );
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0]["type"], "message");

    config.base_url = "https://compatible.example/v1".into();
    let (_, cross_origin) = to_responses_input(&config, &messages);
    assert_eq!(cross_origin.len(), 1);
    assert_eq!(cross_origin[0]["type"], "message");
    let cross_serialized = serde_json::to_string(&cross_origin).unwrap();
    assert!(!cross_serialized.contains("secret"));
}

#[test]
fn model_or_credential_change_degrades_native_replay() {
    let mut config = crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses);
    config.base_url = "https://api.openai.com/v1".into();
    config.api_key = "key-a".into();
    config.model = "gpt-test".into();
    let replay = crate::chat::AssistantReplay::new(
        &config,
        vec![
            crate::chat::AssistantReplayItem::ResponsesReasoning {
                id: Some("rs_1".into()),
                summary: vec!["summary".into()],
                content: vec![],
                encrypted_content: Some("encrypted".into()),
            },
            crate::chat::AssistantReplayItem::Text {
                text: "answer".into(),
                citations: Vec::new(),
            },
        ],
    );
    let messages = vec![ChatMsg::assistant_with_replay(
        "answer".into(),
        vec![],
        Some("summary".into()),
        Some(replay),
    )];

    let (_, same) = to_responses_input(&config, &messages);
    assert!(
        same.iter().any(|item| item["type"] == "reasoning"),
        "identical identity keeps native reasoning: {same:?}"
    );

    let mut model_changed = config.clone();
    model_changed.model = "gpt-other".into();
    let (_, degraded) = to_responses_input(&model_changed, &messages);
    assert!(
        degraded.iter().all(|item| item["type"] != "reasoning"),
        "a model change drops native reasoning: {degraded:?}"
    );
    assert!(
        degraded.iter().any(|item| item["type"] == "message"),
        "the deterministic text projection remains: {degraded:?}"
    );

    let mut key_changed = config.clone();
    key_changed.api_key = "key-b".into();
    let (_, degraded) = to_responses_input(&key_changed, &messages);
    assert!(
        degraded.iter().all(|item| item["type"] != "reasoning"),
        "a credential change drops native reasoning: {degraded:?}"
    );
    assert!(degraded.iter().any(|item| item["type"] == "message"));
}

#[tokio::test]
async fn authoritative_items_preserve_order_and_metadata() {
    let config = crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses);
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let mut tool_stream = crate::tool_stream::ToolCallStream::new();
    let mut replay_items = Vec::new();
    let mut live = super::live::LiveResponsesText::default();
    let mut finish_reason = None;
    let mut api = crate::api_log::ApiCall::new("test", "test", "test");
    let mut metadata = crate::ProviderResponseMetadata::default();
    let mut completed_output = None;
    for frame in [
        serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "type": "reasoning", "id": "rs_a", "summary": [{"type": "summary_text", "text": "a"}],
        }}),
        serde_json::json!({"type": "response.output_item.done", "output_index": 1, "item": {
            "type": "reasoning", "id": "rs_b", "content": [{"type": "reasoning_text", "text": "b"}],
            "encrypted_content": "encrypted-b",
        }}),
        serde_json::json!({"type": "response.output_item.done", "output_index": 2, "item": {
            "type": "message", "content": [{"type": "output_text", "text": "answer"}],
        }}),
        serde_json::json!({"type": "response.output_item.done", "output_index": 3, "item": {
            "type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{}",
        }}),
        serde_json::json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ] {
        super::stream::handle_frame(
            &frame.to_string(),
            &tx,
            &mut tool_stream,
            &mut replay_items,
            &mut live,
            &mut finish_reason,
            &config,
            &mut api,
            &mut metadata,
            &mut completed_output,
        )
        .await
        .expect("frame accepted");
    }
    super::stream::finish_responses_stream(
        tool_stream,
        replay_items,
        live,
        finish_reason,
        completed_output,
        &config,
        &mut api,
        &tx,
    )
    .await
    .expect("stream finishes");

    let replay = loop {
        match rx.try_recv() {
            Ok(crate::ProviderEvent::AssistantReplay(replay)) => break replay,
            Ok(_) => {}
            Err(error) => panic!("missing native replay: {error}"),
        }
    };
    assert_eq!(replay.items.len(), 4);
    match &replay.items[..] {
        [
            crate::chat::AssistantReplayItem::ResponsesReasoning { id: first, .. },
            crate::chat::AssistantReplayItem::ResponsesReasoning {
                id: second,
                content,
                encrypted_content,
                ..
            },
            crate::chat::AssistantReplayItem::Text { text, .. },
            crate::chat::AssistantReplayItem::ToolCall { call },
        ] => {
            assert_eq!(first.as_deref(), Some("rs_a"));
            assert_eq!(second.as_deref(), Some("rs_b"));
            assert_eq!(content, &vec!["b".to_string()]);
            assert_eq!(encrypted_content.as_deref(), Some("encrypted-b"));
            assert_eq!(text, "answer");
            assert_eq!(call.id, "call_1");
        }
        other => panic!("unexpected ordered items: {other:?}"),
    }
}

/// The durable replay uses the finalize-time effective call: streamed
/// fragments plus the done item's missing suffix, without duplication.
#[test]
fn authoritative_done_replays_the_reconciled_call() {
    let (_, replay, calls) = finish_frames(vec![
        serde_json::json!({"type": "response.output_item.added", "output_index": 0, "item": {
            "type": "function_call", "call_id": "call_1", "name": "Write", "arguments": "",
        }}),
        serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"content\":\"line\\n",
        }),
        serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "type": "function_call", "call_id": "call_1", "name": "Write",
            "arguments": "{\"content\":\"line\\n\",\"path\":\"a.txt\"}",
        }}),
        // A repeated done item is a protocol anomaly: it cannot replace the
        // first authoritative call or add a second replay item.
        serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "type": "function_call", "call_id": "call_1", "name": "Write",
            "arguments": "{\"other\":true}",
        }}),
        serde_json::json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ]);
    let expected = "{\"content\":\"line\\n\",\"path\":\"a.txt\"}";
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].arguments, expected);
    let replayed: Vec<&str> = replay
        .items
        .iter()
        .filter_map(|item| match item {
            crate::chat::AssistantReplayItem::ToolCall { call } => {
                Some(call.function.arguments.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        replayed,
        vec![expected],
        "one replay call with the effective arguments: {replay:?}"
    );
}

#[test]
fn missing_done_still_replays_the_function_call() {
    let (config, replay, calls) = finish_frames(vec![
        serde_json::json!({"type": "response.output_item.added", "output_index": 0, "item": {
            "type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "",
        }}),
        serde_json::json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"path\":\"a\"}"}),
        serde_json::json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ]);
    assert_eq!(calls.len(), 1);
    assert!(replay.items.iter().any(|item| matches!(
        item,
        crate::chat::AssistantReplayItem::ToolCall { call } if call.id == "call_1"
    )));

    let mut history = vec![ChatMsg::assistant_with_replay(
        String::new(),
        calls.clone(),
        None,
        Some(replay),
    )];
    history.push(ChatMsg::tool_result("call_1", "result".into()));
    let (_, input) = to_responses_input(&config, &history);
    assert!(input.iter().any(|item| item["type"] == "function_call"));
    assert!(
        input
            .iter()
            .any(|item| item["type"] == "function_call_output")
    );
}

#[test]
fn completed_output_backfills_missing_native_items_and_annotations() {
    let (_, replay, _) = finish_frames(vec![
        serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 1,
            "item_id": "msg_1",
            "delta": "answer",
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "output": [
                    {
                        "type": "web_search_call",
                        "id": "ws_1",
                        "status": "completed",
                        "action": {"type": "search", "query": "pig"},
                        "encrypted_content": "credential",
                    },
                    {
                        "type": "message",
                        "id": "msg_1",
                        "content": [{
                            "type": "output_text",
                            "text": "answer",
                            "annotations": [{
                                "type": "url_citation",
                                "url": "https://example.com/a",
                                "title": "A",
                                "start_index": 0,
                                "end_index": 6,
                            }],
                        }],
                    },
                ],
            },
        }),
    ]);
    assert_eq!(replay.items.len(), 2, "{replay:?}");
    assert!(matches!(
        &replay.items[0],
        crate::chat::AssistantReplayItem::ResponsesWebSearchCall {
            encrypted_content: Some(value), ..
        } if value == "credential"
    ));
    match &replay.items[1] {
        crate::chat::AssistantReplayItem::Text { text, citations } => {
            assert_eq!(text, "answer");
            assert_eq!(citations.len(), 1);
            assert_eq!(citations[0].url, "https://example.com/a");
        }
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn annotations_fallback_uses_the_output_position_for_duplicate_text() {
    let (_, replay, _) = finish_frames(vec![
        serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "item_id": "msg_a",
            "delta": "same",
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "output_index": 1,
            "item_id": "msg_b",
            "delta": "same",
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "output": [
                    {"type": "message", "id": "msg_a", "content": [{"type": "output_text", "text": "same"}]},
                    {"type": "message", "id": "msg_b", "content": [{
                        "type": "output_text",
                        "text": "same",
                        "annotations": [{
                            "type": "url_citation",
                            "url": "https://example.com/b",
                            "title": "B",
                        }],
                    }]},
                ],
            },
        }),
    ]);
    let texts: Vec<_> = replay
        .items
        .iter()
        .filter_map(|item| match item {
            crate::chat::AssistantReplayItem::Text { citations, .. } => Some(citations.len()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec![0, 1],
        "citations attach to the second identical text"
    );
    match &replay.items[1] {
        crate::chat::AssistantReplayItem::Text { citations, .. } => {
            assert_eq!(citations[0].url, "https://example.com/b");
        }
        other => panic!("expected second text, got {other:?}"),
    }
}

#[test]
fn idless_delta_is_claimed_by_the_done_message() {
    let (config, replay, _) = finish_frames(vec![
        serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "same text",
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": {
                "type": "message",
                "id": "msg_1",
                "content": [{"type": "output_text", "text": "same text"}],
            },
        }),
        serde_json::json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ]);
    let texts: Vec<_> = replay
        .items
        .iter()
        .filter_map(|item| match item {
            crate::chat::AssistantReplayItem::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["same text"],
        "the anonymous delta is claimed by the done item"
    );

    let messages = vec![
        ChatMsg::assistant_with_replay(String::new(), vec![], None, Some(replay)),
        ChatMsg::user("next".into()),
    ];
    let (_, input) = to_responses_input(&config, &messages);
    let assistant_messages = input
        .iter()
        .filter(|item| item["type"] == "message" && item["role"] == "assistant")
        .count();
    assert_eq!(
        assistant_messages, 1,
        "the next request has one assistant message: {input:?}"
    );
}

#[test]
fn mixed_done_and_delta_items_keep_order_without_duplicates() {
    let (_, replay, _) = finish_frames(vec![
        serde_json::json!({"type": "response.output_text.delta", "output_index": 0, "item_id": "msg_before", "delta": "before"}),
        serde_json::json!({"type": "response.output_item.done", "output_index": 1, "item": {
            "type": "function_call", "call_id": "call_1", "name": "Read", "arguments": "{}",
        }}),
        serde_json::json!({"type": "response.output_item.done", "output_index": 2, "item": {
            "type": "message", "content": [{"type": "output_text", "text": "after"}],
        }}),
        serde_json::json!({"type": "response.output_text.delta", "output_index": 3, "item_id": "msg_tail", "delta": "tail"}),
        serde_json::json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ]);
    let labels: Vec<String> = replay
        .items
        .iter()
        .map(|item| match item {
            crate::chat::AssistantReplayItem::Text { text, .. } => format!("text:{text}"),
            crate::chat::AssistantReplayItem::ToolCall { call } => format!("call:{}", call.id),
            _ => "other".into(),
        })
        .collect();
    assert_eq!(
        labels,
        vec!["text:before", "call:call_1", "text:after", "text:tail"]
    );
}

#[test]
fn mixed_reasoning_done_and_delta_does_not_drop_or_duplicate() {
    let (_, replay, _) = finish_frames(vec![
        serde_json::json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "item_id": "rs_a", "delta": "A"}),
        serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": {
            "type": "reasoning", "id": "rs_a", "summary": [{"type": "summary_text", "text": "A"}],
        }}),
        serde_json::json!({"type": "response.reasoning_summary_text.delta", "output_index": 1, "item_id": "rs_b", "delta": "B"}),
        serde_json::json!({"type": "response.completed", "response": {"id": "resp_1"}}),
    ]);
    let reasoning: Vec<_> = replay
        .items
        .iter()
        .filter_map(|item| match item {
            crate::chat::AssistantReplayItem::ResponsesReasoning { summary, .. } => {
                Some(summary.join(""))
            }
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, vec!["A", "B"]);
}
