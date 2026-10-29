use super::request::to_responses_input;
use super::stream::usage_fields;
use crate::chat::{ChatImage, ChatMsg, ToolCall};

#[test]
fn input_assembly_hoists_system_and_replays_turns() {
    let messages = vec![
        ChatMsg::system("be brief".into()),
        ChatMsg::user("hi".into()),
        ChatMsg::assistant(
            "thinking about it".into(),
            vec![ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                arguments: "{\"path\":\"a\"}".into(),
            }],
            Some("pondered".into()),
        ),
        ChatMsg::tool_result("c1", "ok".into()),
    ];
    let (instructions, input) = to_responses_input(
        &crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses),
        &messages,
    );
    assert_eq!(instructions, "be brief");
    assert_eq!(
        input.len(),
        5,
        "user + reasoning + assistant message + function_call + output"
    );
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    // Reasoning item precedes the assistant message, plaintext summary only
    assert_eq!(input[1]["type"], "reasoning");
    assert_eq!(input[1]["summary"][0]["text"], "pondered");
    assert!(input[1].get("encrypted_content").is_none());
    assert_eq!(input[2]["type"], "message");
    assert_eq!(input[2]["role"], "assistant");
    assert_eq!(input[2]["content"][0]["type"], "output_text");
    assert_eq!(input[3]["type"], "function_call");
    assert_eq!(input[3]["call_id"], "c1");
    assert_eq!(input[3]["name"], "Read");
    assert_eq!(input[3]["arguments"], "{\"path\":\"a\"}");
    assert_eq!(input[4]["type"], "function_call_output");
    assert_eq!(input[4]["call_id"], "c1");
    assert_eq!(input[4]["output"], "ok");
}

#[test]
fn media_only_user_and_tool_results_have_no_empty_text_parts() {
    let mut user = ChatMsg::user(String::new());
    user.images = vec![ChatImage {
        media_type: "image/png".into(),
        data_base64: "QUJD".into(),
        label: None,
    }];
    user.documents = vec![crate::chat::ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: "JVBERi0=".into(),
        filename: "only.pdf".into(),
        label: None,
        token_estimate: None,
    }];
    let mut tool = ChatMsg::tool_result_with_images(
        "call-1",
        String::new(),
        vec![ChatImage {
            media_type: "image/png".into(),
            data_base64: "REVG".into(),
            label: None,
        }],
    );
    tool.documents = vec![crate::chat::ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: "JVBERi0=".into(),
        filename: "tool.pdf".into(),
        label: None,
        token_estimate: None,
    }];
    let (_, input) = to_responses_input(
        &crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses),
        &[user, tool],
    );
    let user_parts = input[0]["content"].as_array().expect("user parts");
    assert!(
        user_parts
            .iter()
            .all(|part| part["type"] != "input_text"
                || !part["text"].as_str().unwrap_or("").is_empty()),
        "no empty text part: {user_parts:?}"
    );
    let output = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("tool output");
    let output_items = output["output"].as_array().expect("media output array");
    assert!(
        output_items
            .iter()
            .all(|part| part["type"] != "input_text"
                || !part["text"].as_str().unwrap_or("").is_empty()),
        "no empty text part: {output_items:?}"
    );
    assert!(
        output_items
            .iter()
            .any(|part| part["type"] == "input_image"),
        "the image survives: {output_items:?}"
    );
}

#[test]
fn official_origin_replays_web_search_call_without_encrypted_content() {
    let mut config = crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses);
    config.base_url = "https://api.openai.com/v1".into();
    config.cap_web_search = true;
    config.replay_native_search = true;
    let replay = crate::chat::AssistantReplay::new(
        &config,
        vec![
            crate::chat::AssistantReplayItem::ResponsesWebSearchCall {
                id: Some("ws_1".into()),
                status: Some("completed".into()),
                action: serde_json::json!({
                    "type": "search",
                    "query": "weather",
                    "sources": [{"type": "url", "url": "https://example.com/a", "title": "A"}],
                }),
                encrypted_content: None,
            },
            crate::chat::AssistantReplayItem::Text {
                text: "answer".into(),
                citations: Vec::new(),
            },
        ],
    );
    let messages = vec![crate::chat::ChatMsg::assistant_with_replay(
        String::new(),
        vec![],
        None,
        Some(replay),
    )];
    let (_, input) = to_responses_input(&config, &messages);
    let search = input
        .iter()
        .find(|item| item["type"] == "web_search_call")
        .expect("official store:false replay keeps the search call");
    assert_eq!(search["id"], "ws_1");
    assert_eq!(search["status"], "completed");
    assert_eq!(search["action"]["query"], "weather");
    assert!(
        search.get("encrypted_content").is_none(),
        "the real shape does not require an invented credential: {search}"
    );

    let body = super::build_responses_body(&config, &messages, &[], true).expect("body");
    assert!(
        body["include"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "web_search_call.action.sources"),
        "official host requests source details: {body}"
    );

    // The optional real field survives when a gateway supplies it.
    let with_credential = crate::chat::AssistantReplay::new(
        &config,
        vec![crate::chat::AssistantReplayItem::ResponsesWebSearchCall {
            id: Some("ws_2".into()),
            status: None,
            action: serde_json::json!({"type": "search"}),
            encrypted_content: Some("gateway-credential".into()),
        }],
    );
    let (_, input) = to_responses_input(
        &config,
        &[crate::chat::ChatMsg::assistant_with_replay(
            String::new(),
            vec![],
            None,
            Some(with_credential),
        )],
    );
    assert_eq!(input[0]["encrypted_content"], "gateway-credential");

    let no_action = crate::chat::AssistantReplay::new(
        &config,
        vec![crate::chat::AssistantReplayItem::ResponsesWebSearchCall {
            id: Some("ws_3".into()),
            status: Some("completed".into()),
            action: serde_json::json!({}),
            encrypted_content: None,
        }],
    );
    let (_, input) = to_responses_input(
        &config,
        &[crate::chat::ChatMsg::assistant_with_replay(
            String::new(),
            vec![],
            None,
            Some(no_action),
        )],
    );
    assert!(
        input[0].get("action").is_none(),
        "an empty action is omitted, not fabricated: {input:?}"
    );

    // Compatible Responses endpoints do not receive the OpenAI-only include.
    let mut compatible = config.clone();
    compatible.base_url = "https://compatible.example/v1".into();
    compatible.replay_native_search = false;
    let body = super::build_responses_body(&compatible, &messages, &[], true).expect("body");
    assert!(
        body.get("include").is_none_or(|include| {
            !include
                .as_array()
                .is_some_and(|items| items.iter().any(|v| v == "web_search_call.action.sources"))
        }),
        "compatible endpoints keep the include minimal: {body}"
    );

    // Turning the capability off deterministically degrades to text-only.
    let mut without_capability = config;
    without_capability.cap_web_search = false;
    let (_, input) = to_responses_input(&without_capability, &messages);
    assert!(
        input.iter().all(|item| item["type"] != "web_search_call"),
        "cap_web_search=false drops the native item: {input:?}"
    );
    assert!(input.iter().any(|item| item["type"] == "message"));
}

#[test]
fn completed_search_call_merges_missing_fields_into_the_done_item() {
    let mut items = vec![(
        0,
        crate::chat::AssistantReplayItem::ResponsesWebSearchCall {
            id: Some("ws_1".into()),
            status: None,
            action: serde_json::json!({"type": "search", "query": "q"}),
            encrypted_content: None,
        },
    )];
    let output = vec![serde_json::json!({
        "type": "web_search_call",
        "id": "ws_1",
        "status": "completed",
        "action": {
            "type": "search",
            "query": "q",
            "sources": [{"type": "url", "url": "https://example.com/a"}],
        },
        "encrypted_content": "late-credential",
    })];
    super::search::merge_completed_native_items(&mut items, &output);
    assert_eq!(items.len(), 1, "the same id is merged, not duplicated");
    match &items[0].1 {
        crate::chat::AssistantReplayItem::ResponsesWebSearchCall {
            status,
            action,
            encrypted_content,
            ..
        } => {
            assert_eq!(status.as_deref(), Some("completed"));
            assert_eq!(action["sources"][0]["url"], "https://example.com/a");
            assert_eq!(encrypted_content.as_deref(), Some("late-credential"));
        }
        other => panic!("expected search call, got {other:?}"),
    }
}

#[test]
fn responses_request_tools_strict_flag() {
    let mut model = crate::chat::ResolvedModel {
        base_url: "http://localhost".into(),
        api_key: String::new(),
        model: "m".into(),
        context_window: 0,
        max_output_tokens: 0,
        api_format: pig_protocol::ApiFormat::OpenAiResponses,
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
        provider_name: "p".into(),
        tool_settings: Default::default(),
    };
    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {"name": "Read", "description": "d", "strict": true, "parameters": {
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }}
    })];
    let off = super::request::responses_request_tools(&model, &tools);
    assert_eq!(off[0]["strict"], false, "off: strict stays false");
    model.cap_strict_tools = true;
    let on = super::request::responses_request_tools(&model, &tools);
    assert_eq!(on[0]["strict"], true);
    assert_eq!(on[0]["parameters"]["additionalProperties"], false);

    // An undeclared tool never becomes strict, even with the capability on.
    let undeclared = vec![serde_json::json!({
        "type": "function",
        "function": {"name": "Grep", "description": "d", "parameters": {
            "type": "object",
            "properties": {"pattern": {"type": "string"}},
            "required": ["pattern"]
        }}
    })];
    let wire = super::request::responses_request_tools(&model, &undeclared);
    assert_eq!(wire[0]["strict"], false, "undeclared stays false");
    assert!(
        wire[0]["parameters"].get("additionalProperties").is_none(),
        "an undeclared schema is not rewritten: {wire:?}"
    );
}

#[test]
fn responses_web_search_default_injection() {
    use pig_protocol::ApiFormat;
    let model = |cap: bool, tool: Option<serde_json::Value>| crate::chat::ResolvedModel {
        base_url: "http://localhost".into(),
        api_key: String::new(),
        model: "m".into(),
        context_window: 0,
        max_output_tokens: 0,
        api_format: ApiFormat::OpenAiResponses,
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
    };
    // cap off -> no injection
    assert!(super::responses_web_search_tool(&model(false, None)).is_none());
    // cap on, no customization -> the official bare web_search tool
    assert_eq!(
        super::responses_web_search_tool(&model(true, None)).unwrap(),
        serde_json::json!({"type": "web_search"})
    );
    // cap on with customization -> the custom JSON verbatim
    let custom = serde_json::json!({"type": "web_search", "search_context_size": "low"});
    assert_eq!(
        super::responses_web_search_tool(&model(true, Some(custom.clone()))).unwrap(),
        custom
    );
}

#[test]
fn tool_result_images_ride_the_output_array() {
    let messages = vec![ChatMsg::tool_result_with_images(
        "c1",
        "Read image x.png".into(),
        vec![ChatImage {
            media_type: "image/png".into(),
            data_base64: "QUJD".into(),
            label: Some("x.png".into()),
        }],
    )];
    let (_instructions, input) = to_responses_input(
        &crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses),
        &messages,
    );
    assert_eq!(input.len(), 1);
    let output = &input[0]["output"];
    assert!(
        output.is_array(),
        "an image-carrying result sends the array form: {output}"
    );
    assert_eq!(output[0]["type"], "input_text");
    assert_eq!(output[0]["text"], "Read image x.png");
    assert_eq!(output[1]["type"], "input_image");
    assert_eq!(output[1]["image_url"], "data:image/png;base64,QUJD");
    // The imageless path stays a plain string (regression-safe)
    let (_i, input) = to_responses_input(
        &crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses),
        &[ChatMsg::tool_result("c2", "ok".into())],
    );
    assert!(input[0]["output"].is_string());
}

#[test]
fn user_images_become_input_image_parts() {
    let mut msg = ChatMsg::user("look".into());
    msg.images = vec![ChatImage {
        media_type: "image/png".into(),
        data_base64: "QUJD".into(),
        label: Some("x.png".into()),
    }];
    let (_instructions, input) = to_responses_input(
        &crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses),
        &[msg],
    );
    let parts = &input[0]["content"];
    assert_eq!(parts[0]["type"], "input_text");
    // Labeled images carry a "[label]" text marker before their part
    assert_eq!(parts[1]["type"], "input_text");
    assert_eq!(parts[1]["text"], "[x.png]");
    assert_eq!(parts[2]["type"], "input_image");
    assert_eq!(parts[2]["image_url"], "data:image/png;base64,QUJD");

    // Unlabeled: no marker (regression-safe for tool-less callers)
    let mut msg = ChatMsg::user("look".into());
    msg.images = vec![ChatImage {
        media_type: "image/png".into(),
        data_base64: "QUJD".into(),
        label: None,
    }];
    let (_instructions, input) = to_responses_input(
        &crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses),
        &[msg],
    );
    let parts = &input[0]["content"];
    assert_eq!(parts[1]["type"], "input_image");
}

/// Responses counts input_tokens as the whole input (cached included) and
/// reports the reasoning slice separately — the same normalization the
/// Chat Completions path applies
#[test]
fn usage_normalization() {
    let usage = serde_json::json!({
        "input_tokens": 1000,
        "input_tokens_details": {"cached_tokens": 400},
        "output_tokens": 42,
        "output_tokens_details": {"reasoning_tokens": 20},
        "total_tokens": 1042,
    });
    assert_eq!(
        usage_fields(&usage, 128_000),
        Some((600, 400, 42, 1042, 128_000, 20))
    );
    // Zero usage does not emit (same guard as the other formats)
    assert!(usage_fields(&serde_json::json!({"input_tokens": 0}), 0).is_none());
}
