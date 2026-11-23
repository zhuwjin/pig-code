use pig_protocol::ApiFormat;

use super::{ChatImage, ChatMsg, ResolvedModel, ToolCall, finish};
use crate::anthropic::{anthropic_request_tools, to_anthropic_messages};
use crate::events::ProviderEvent;
use crate::openai::{openai_request_tools, to_openai_messages};

#[tokio::test]
async fn finish_filters_nameless_tool_calls() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let calls = vec![
        ToolCall {
            id: "a".into(),
            name: String::new(),
            arguments: "{}".into(),
        },
        ToolCall {
            id: "b".into(),
            name: "ExecCommand".into(),
            arguments: "{}".into(),
        },
    ];
    finish(&tx, calls, pig_protocol::FinishReason::Stop).await;
    match rx.try_recv() {
        Ok(ProviderEvent::ToolCalls(calls)) => {
            assert_eq!(calls.len(), 1, "nameless calls should be filtered");
            assert_eq!(calls[0].name, "ExecCommand");
        }
        other => panic!("remaining named call should come through: {other:?}"),
    }
    assert!(matches!(rx.try_recv(), Ok(ProviderEvent::Finished(_))));
}

#[test]
fn legacy_tool_message_defaults_to_success() {
    // Old subagent contexts have no is_error field: readable, and the
    // default is the pre-existing success semantics.
    let msg: ChatMsg = serde_json::from_value(serde_json::json!({
        "role": "tool",
        "content": "ok",
        "tool_call_id": "c1",
    }))
    .expect("legacy tool message parses");
    assert!(!msg.is_error);
    // Failures persist the marker (subagent contexts rebuild it verbatim)
    let value = serde_json::to_value(ChatMsg::tool_result("c1", "boom".into()).with_error(true))
        .expect("serialize");
    assert_eq!(value["is_error"], true);
    let success = serde_json::to_value(ChatMsg::tool_result("c1", "ok".into())).expect("ser");
    assert!(success.get("is_error").is_none(), "success stays compact");
}

#[test]
fn anthropic_tool_result_with_images_becomes_blocks() {
    let messages = vec![ChatMsg::tool_result_with_images(
        "c1",
        "Read image x.png".into(),
        vec![ChatImage {
            media_type: "image/png".into(),
            data_base64: "QUJD".into(),
            label: Some("x.png".into()),
        }],
    )];
    let (_system, out) = to_anthropic_messages(
        &crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages),
        &messages,
    );
    let content = out[0]["content"].as_array().expect("user message blocks");
    assert_eq!(content[0]["type"], "tool_result");
    let blocks = content[0]["content"]
        .as_array()
        .expect("content is an array when images present");
    assert_eq!(blocks[0]["type"], "image");
    assert_eq!(blocks[0]["source"]["type"], "base64");
    assert_eq!(blocks[0]["source"]["media_type"], "image/png");
    assert_eq!(blocks[0]["source"]["data"], "QUJD");
    assert_eq!(blocks[1]["type"], "text");
    assert_eq!(blocks[1]["text"], "Read image x.png");
}

#[test]
fn anthropic_tool_result_without_images_stays_string() {
    // Regression: the imageless path matches the old behavior (content is a string)
    let messages = vec![ChatMsg::tool_result("c1", "ok".into())];
    let (_system, out) = to_anthropic_messages(
        &crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages),
        &messages,
    );
    let content = out[0]["content"].as_array().expect("blocks");
    assert_eq!(content[0]["type"], "tool_result");
    assert_eq!(
        content[0]["content"], "ok",
        "content stays a string without images"
    );
}

#[test]
fn openai_tool_result_with_images_splits_into_two_messages() {
    let messages = vec![ChatMsg::tool_result_with_images(
        "c1",
        "Read image x.png".into(),
        vec![ChatImage {
            media_type: "image/jpeg".into(),
            data_base64: "QUJD".into(),
            label: Some("x.png".into()),
        }],
    )];
    let out = to_openai_messages(&messages);
    assert_eq!(out.len(), 2, "splits into tool text + user image messages");
    assert_eq!(out[0]["role"], "tool");
    assert_eq!(out[0]["tool_call_id"], "c1");
    assert_eq!(out[0]["content"], "Read image x.png");
    assert!(
        out[0].get("images").is_none(),
        "tool message carries no images"
    );
    assert_eq!(out[1]["role"], "user");
    let parts = out[1]["content"].as_array().expect("content parts");
    assert_eq!(parts[0]["type"], "text");
    assert!(parts[0]["text"].as_str().unwrap().contains("x.png"));
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(parts[1]["image_url"]["url"], "data:image/jpeg;base64,QUJD");
}

#[test]
fn openai_no_image_matches_plain_serde() {
    // Regression: without images the custom construction is byte-identical to direct serde serialization
    let messages = vec![
        ChatMsg::system("s".into()),
        ChatMsg::user("u".into()),
        ChatMsg::tool_result("c1", "ok".into()),
    ];
    let built = to_openai_messages(&messages);
    let direct: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect();
    assert_eq!(built, direct);
}

#[test]
fn user_message_with_images_both_formats() {
    let mut msg = ChatMsg::user("view image".into());
    msg.images = vec![ChatImage {
        media_type: "image/png".into(),
        data_base64: "QUJD".into(),
        label: Some("1.png".into()),
    }];
    // OpenAI: user content becomes parts (text first, then a "[label]"
    // marker + image_url per labeled image)
    let out = to_openai_messages(&[msg.clone()]);
    assert_eq!(out.len(), 1, "user with images is not split");
    let parts = out[0]["content"].as_array().expect("parts");
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], "view image");
    assert_eq!(parts[1]["text"], "[1.png]");
    assert_eq!(parts[2]["image_url"]["url"], "data:image/png;base64,QUJD");
    // Anthropic: user content becomes blocks (marker + image first, text after)
    let (_s, out) = to_anthropic_messages(
        &crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages),
        &[msg],
    );
    let blocks = out[0]["content"].as_array().expect("blocks");
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "[1.png]");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["data"], "QUJD");
    assert_eq!(blocks[2]["type"], "text");
    assert_eq!(blocks[2]["text"], "view image");
}

#[tokio::test]
async fn finish_all_nameless_emits_no_tool_calls() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let calls = vec![ToolCall::default()];
    finish(&tx, calls, pig_protocol::FinishReason::Stop).await;
    assert!(matches!(rx.try_recv(), Ok(ProviderEvent::Finished(_))));
    assert!(
        rx.try_recv().is_err(),
        "all-nameless calls should emit no ToolCalls"
    );
}

#[test]
fn anthropic_messages_echo_thinking_first() {
    let messages = vec![
        ChatMsg::system("s".into()),
        ChatMsg::user("u".into()),
        ChatMsg::assistant(
            "answer".into(),
            vec![ToolCall {
                id: "c1".into(),
                name: "ExecCommand".into(),
                arguments: "{}".into(),
            }],
            Some("pondered".into()),
        ),
        ChatMsg::tool_result("c1", "ok".into()),
    ];
    let (_system, out) = to_anthropic_messages(
        &crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages),
        &messages,
    );
    let assistant = &out[1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"][0]["type"], "thinking");
    assert_eq!(assistant["content"][0]["thinking"], "pondered");
    assert_eq!(assistant["content"][1]["type"], "text");
    assert_eq!(assistant["content"][2]["type"], "tool_use");
}

#[test]
fn anthropic_messages_skip_empty_thinking() {
    let messages = vec![ChatMsg::assistant("answer".into(), vec![], None)];
    let (_system, out) = to_anthropic_messages(
        &crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages),
        &messages,
    );
    let content = out[0]["content"].as_array().expect("blocks");
    assert_eq!(
        content.len(),
        1,
        "no thinking block without thinking content"
    );
    assert_eq!(content[0]["type"], "text");
}

fn search_model(cap: bool, tool: Option<serde_json::Value>) -> ResolvedModel {
    ResolvedModel {
        base_url: "http://localhost".into(),
        api_key: String::new(),
        model: "m".into(),
        context_window: 0,
        max_output_tokens: 0,
        api_format: ApiFormat::AnthropicMessages,
        reasoning_params: None,
        cap_structured: false,
        cap_strict_tools: false,
        cap_web_search: cap,
        web_search_tool: tool,
        input_image: false,
        input_pdf: false,
        anthropic_cache_control: false,
        replay_native_search: false,
        prompt_cache_key: None,
        provider_name: "p".into(),
        tool_settings: Default::default(),
    }
}

#[test]
fn anthropic_web_search_tool_injection() {
    use crate::anthropic::anthropic_web_search_tool;
    // cap off -> no injection
    assert!(anthropic_web_search_tool(&search_model(false, None)).is_none());
    // cap on, no customization -> default web_search_20250305
    let tool =
        anthropic_web_search_tool(&search_model(true, None)).expect("cap enabled should inject");
    assert_eq!(
        tool,
        serde_json::json!({"type": "web_search_20250305", "name": "web_search"})
    );
    // cap on, with customization -> use the custom JSON
    let custom =
        serde_json::json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3});
    assert_eq!(
        anthropic_web_search_tool(&search_model(true, Some(custom.clone()))).unwrap(),
        custom
    );
}

/// Chat Completions has no hosted-search standard: no configured
/// `web_search_tool` may leak into the request.
#[test]
fn chat_never_injects_a_hosted_search_tool() {
    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {"name": "Read", "description": "d", "parameters": {}}
    })];
    let custom = serde_json::json!({"type": "web_search", "web_search": {"enable": true}});
    let out = openai_request_tools(&search_model(true, Some(custom)), &tools);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["function"]["name"], "Read");
    assert!(
        out.iter().all(|tool| tool["type"] == "function"),
        "Chat must not carry a hosted tool: {out:?}"
    );
}

fn tool_wire(strict: bool) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {"name": "Read", "description": "d", "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string"}, "offset": {"type": "integer"}},
            "required": ["path"]
        }, "strict": strict}
    })
}

/// Strict is a per-tool preference: only a declared tool on a capable
/// endpoint is strictified; a declaration we cannot honor is omitted.
#[test]
fn openai_request_tools_only_strictifies_declared_tools() {
    let mut model = search_model(false, None);
    let undeclared = openai_request_tools(&model, &[tool_wire(false)]);
    assert!(undeclared[0]["function"].get("strict").is_none());
    model.cap_strict_tools = true;
    let undeclared = openai_request_tools(&model, &[tool_wire(false)]);
    assert!(
        undeclared[0]["function"].get("strict").is_none(),
        "an undeclared tool is never strictified: {undeclared:?}"
    );
    let strict = openai_request_tools(&model, &[tool_wire(true)]);
    assert_eq!(strict[0]["function"]["strict"], true);
    let params = &strict[0]["function"]["parameters"];
    assert!(
        params["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "offset"),
        "every field becomes required: {params}"
    );
    assert_eq!(params["additionalProperties"], false);
    // Capability off downgrades a declaration instead of failing.
    model.cap_strict_tools = false;
    let downgraded = openai_request_tools(&model, &[tool_wire(true)]);
    assert!(
        downgraded[0]["function"].get("strict").is_none(),
        "capability off must omit the declaration: {downgraded:?}"
    );
}

#[test]
fn anthropic_request_tools_strict_is_declared_and_supported() {
    let mut model = search_model(false, None);
    model.cap_strict_tools = true;
    model.model = "claude-sonnet-4-5".into();
    let supported = anthropic_request_tools(&model, &[tool_wire(true)]);
    assert_eq!(supported[0]["strict"], true);
    assert_eq!(supported[0]["input_schema"]["additionalProperties"], false);

    model.model = "anthropic/claude-sonnet-4.5".into();
    let router = anthropic_request_tools(&model, &[tool_wire(true)]);
    assert_eq!(router[0]["strict"], true);

    // A known-unsupported model downgrades rather than failing.
    model.model = "claude-sonnet-4".into();
    let downgraded = anthropic_request_tools(&model, &[tool_wire(true)]);
    assert!(downgraded[0].get("strict").is_none());

    // Undeclared tools are never strictified even on a capable model.
    model.model = "claude-sonnet-4-5".into();
    let undeclared = anthropic_request_tools(&model, &[tool_wire(false)]);
    assert!(undeclared[0].get("strict").is_none());

    model.cap_strict_tools = false;
    let off = anthropic_request_tools(&model, &[tool_wire(true)]);
    assert!(off[0].get("strict").is_none());
}

/// Request-level tool assembly (shared by streaming/non-streaming): the Anthropic shape must be
/// name/description/input_schema -- leftover OpenAI wire shape gets a 422 from compatible endpoints
#[test]
fn anthropic_request_tools_converts_openai_wire_shape() {
    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {"name": "Read", "description": "read files", "parameters": {"type": "object"}}
    })];
    let out = anthropic_request_tools(&search_model(false, None), &tools);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["name"], "Read");
    assert_eq!(out[0]["description"], "read files");
    assert_eq!(out[0]["input_schema"]["type"], "object");
    assert!(
        out[0].get("function").is_none(),
        "OpenAI wire shape must not remain: {out:?}"
    );
    assert!(
        out[0].get("type").is_none(),
        "custom tool carries no type: {out:?}"
    );
    // cap on -> append the server-side search tool (same assembly as the streaming path)
    let out = anthropic_request_tools(&search_model(true, None), &tools);
    assert_eq!(out.len(), 2);
    assert_eq!(out[1]["type"], "web_search_20250305");
}

/// OpenAI side: pass the wire shape through verbatim; no hosted search tool.
#[test]
fn openai_request_tools_keeps_wire_shape() {
    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {"name": "Read", "description": "d", "parameters": {}}
    })];
    let out = openai_request_tools(&search_model(false, None), &tools);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["type"], "function");
}

#[test]
fn replay_sources_merge_dedupe_and_never_leak_credentials() {
    let replay = crate::chat::AssistantReplay {
        origin: crate::chat::ReplayOrigin::new(
            ApiFormat::OpenAiResponses,
            "https://api.openai.com/v1",
            "gpt-5",
            "test-key",
        ),
        items: vec![
            crate::chat::AssistantReplayItem::ResponsesWebSearchCall {
                id: Some("ws_1".into()),
                status: Some("completed".into()),
                action: serde_json::json!({"type": "search", "query": "q", "sources": [
                    {"type": "url", "url": "https://example.com/a", "title": "A"},
                    {"type": "url", "url": "ftp://example.com/skip", "title": "skip"},
                ]}),
                encrypted_content: Some("opaque-credential".into()),
            },
            crate::chat::AssistantReplayItem::Text {
                text: "answer".into(),
                citations: vec![
                    crate::chat::TextCitation {
                        url: "https://example.com/a".into(),
                        title: String::new(),
                        snippet: None,
                        start_index: Some(0),
                        end_index: Some(6),
                        encrypted_index: None,
                        cited_text: Some("answer".into()),
                    },
                    crate::chat::TextCitation {
                        url: "https://example.com/b".into(),
                        title: "B".into(),
                        snippet: None,
                        start_index: None,
                        end_index: None,
                        encrypted_index: Some("secret-index".into()),
                        cited_text: None,
                    },
                ],
            },
        ],
    };
    let sources = crate::chat::replay_sources(&replay);
    assert_eq!(sources.len(), 2, "{sources:?}");
    assert_eq!(sources[0].url, "https://example.com/a");
    assert_eq!(sources[0].title, "A", "duplicate fills the missing title");
    assert_eq!(sources[0].snippet.as_deref(), Some("answer"));
    assert_eq!(sources[1].url, "https://example.com/b");
    let serialized = serde_json::to_string(&sources).unwrap();
    assert!(!serialized.contains("secret-index"));
    assert!(!serialized.contains("opaque-credential"));
}

fn url_with_len(len: usize) -> String {
    const PREFIX: &str = "https://example.com/";
    format!("{PREFIX}{}", "a".repeat(len - PREFIX.len()))
}

#[test]
fn replay_sources_drop_overlong_urls_without_opening_a_truncated_one() {
    let valid = url_with_len(2048);
    let invalid = url_with_len(2049);
    assert_eq!(valid.chars().count(), 2048);
    assert_eq!(invalid.chars().count(), 2049);

    let responses = crate::chat::AssistantReplay {
        origin: crate::chat::ReplayOrigin::new(
            ApiFormat::OpenAiResponses,
            "https://api.openai.com/v1",
            "gpt-5",
            "test-key",
        ),
        items: vec![
            crate::chat::AssistantReplayItem::ResponsesWebSearchCall {
                id: Some("ws_1".into()),
                status: Some("completed".into()),
                action: serde_json::json!({"type": "search", "sources": [
                    {"type": "url", "url": valid, "title": "Valid"},
                    {"type": "url", "url": invalid, "title": "Too long"},
                ]}),
                encrypted_content: Some("opaque-response-credential".into()),
            },
            crate::chat::AssistantReplayItem::Text {
                text: "answer".into(),
                citations: vec![crate::chat::TextCitation {
                    url: invalid.clone(),
                    title: "Citation".into(),
                    snippet: None,
                    start_index: None,
                    end_index: None,
                    encrypted_index: Some("secret-citation-index".into()),
                    cited_text: None,
                }],
            },
        ],
    };
    let sources = crate::chat::replay_sources(&responses);
    assert_eq!(sources.len(), 1, "{sources:?}");
    assert_eq!(sources[0].url, url_with_len(2048));
    let serialized = serde_json::to_string(&sources).unwrap();
    assert!(!serialized.contains("opaque-response-credential"));
    assert!(!serialized.contains("secret-citation-index"));
    assert!(
        !serialized.contains(&invalid),
        "the overlong URL is dropped whole, never truncated and opened"
    );

    let anthropic = crate::chat::AssistantReplay {
        origin: crate::chat::ReplayOrigin::new(
            ApiFormat::AnthropicMessages,
            "https://api.anthropic.com",
            "claude",
            "test-key",
        ),
        items: vec![
            crate::chat::AssistantReplayItem::AnthropicWebSearchResult {
                tool_use_id: "srvtoolu_1".into(),
                content: serde_json::json!([{
                    "type": "web_search_result",
                    "url": valid,
                    "title": "Valid",
                    "encrypted_content": "opaque-anthropic-credential",
                }, {
                    "type": "web_search_result",
                    "url": invalid,
                    "title": "Too long",
                    "encrypted_content": "opaque-anthropic-credential-2",
                }]),
            },
            crate::chat::AssistantReplayItem::Text {
                text: "answer".into(),
                citations: vec![crate::chat::TextCitation {
                    url: invalid,
                    title: "Citation".into(),
                    snippet: None,
                    start_index: None,
                    end_index: None,
                    encrypted_index: Some("secret-anthropic-index".into()),
                    cited_text: None,
                }],
            },
        ],
    };
    let sources = crate::chat::replay_sources(&anthropic);
    assert_eq!(sources.len(), 1, "{sources:?}");
    assert_eq!(sources[0].url, url_with_len(2048));
    let serialized = serde_json::to_string(&sources).unwrap();
    assert!(!serialized.contains("opaque-anthropic-credential"));
    assert!(!serialized.contains("secret-anthropic-index"));
}

#[test]
fn replay_sources_are_bounded_and_skip_non_http_urls() {
    let citations = (0..40)
        .map(|index| crate::chat::TextCitation {
            url: if index == 0 {
                "file:///etc/passwd".into()
            } else {
                format!("https://example.com/{index}")
            },
            title: String::new(),
            snippet: None,
            start_index: None,
            end_index: None,
            encrypted_index: None,
            cited_text: None,
        })
        .collect();
    let replay = crate::chat::AssistantReplay {
        origin: crate::chat::ReplayOrigin::new(
            ApiFormat::AnthropicMessages,
            "https://api.anthropic.com",
            "claude",
            "test-key",
        ),
        items: vec![crate::chat::AssistantReplayItem::Text {
            text: "answer".into(),
            citations,
        }],
    };
    let sources = crate::chat::replay_sources(&replay);
    assert_eq!(sources.len(), 32);
    assert!(
        sources
            .iter()
            .all(|source| source.url.starts_with("https://"))
    );
}
