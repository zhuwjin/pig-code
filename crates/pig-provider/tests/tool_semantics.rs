//! Tool-calling semantics on the real serialized request path: parallel-tool
//! control, selective strict preferences, and the absence of tool-choice
//! forcing, exercised by streaming against the in-process mock provider and
//! inspecting the captured HTTP bodies (not helper return values).

use pig_protocol::ApiFormat;
use pig_provider::mock;
use pig_provider::{
    CallControl, ChatMsg, ProviderEvent, ResolvedModel, ToolCallSettings, complete_messages,
    stream_chat,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

type RequestLog = Arc<Mutex<Vec<String>>>;

fn resolved_model(
    port: u16,
    api_format: ApiFormat,
    parallel_tool_calls: bool,
    cap_strict_tools: bool,
    model_id: &str,
) -> ResolvedModel {
    ResolvedModel {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "mock-key".into(),
        model: model_id.into(),
        context_window: 128_000,
        max_output_tokens: 8_192,
        api_format,
        reasoning_params: None,
        cap_structured: false,
        cap_strict_tools,
        cap_web_search: false,
        web_search_tool: None,
        input_image: false,
        input_pdf: false,
        anthropic_cache_control: false,
        replay_native_search: false,
        prompt_cache_key: None,
        provider_name: "mock".into(),
        tool_settings: ToolCallSettings::new(parallel_tool_calls),
    }
}

fn read_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "Read",
            "description": "read a file",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            },
        },
    })
}

fn grep_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "Grep",
            "description": "search a file",
            "parameters": {
                "type": "object",
                "properties": {"pattern": {"type": "string"}},
                "required": ["pattern"],
            },
        },
    })
}

fn declared_strict(tool: &mut Value, strict: bool) -> Value {
    tool["function"]["strict"] = json!(strict);
    tool.clone()
}

/// Stream one request and return the captured request body plus the terminal
/// outcome.
async fn run(
    model: ResolvedModel,
    tools: Vec<Value>,
    log: &RequestLog,
) -> (Option<Value>, Vec<ProviderEvent>) {
    run_with_messages(model, vec![ChatMsg::user("hello".into())], tools, log).await
}

async fn run_with_messages(
    model: ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<Value>,
    log: &RequestLog,
) -> (Option<Value>, Vec<ProviderEvent>) {
    let before = log.lock().expect("log").len();
    let format = model.api_format;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(stream_chat(
        model,
        messages,
        tools,
        tx,
        CallControl::new(CancellationToken::new()),
    ));
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    task.await.expect("stream task joins");
    let body = log
        .lock()
        .expect("log")
        .get(before)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    if format == ApiFormat::AnthropicMessages
        && let Some(body) = &body
    {
        mock::validate_anthropic_request(body)
            .unwrap_or_else(|problem| panic!("invalid Anthropic request shape: {problem}\n{body}"));
    }
    (body, events)
}

fn failure_detail(events: &[ProviderEvent]) -> Option<String> {
    events.iter().find_map(|event| match event {
        ProviderEvent::Failed(error) => Some(format!("{error:?}")),
        _ => None,
    })
}

fn formats() -> [ApiFormat; 3] {
    [
        ApiFormat::OpenAiChat,
        ApiFormat::OpenAiResponses,
        ApiFormat::AnthropicMessages,
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_tool_calls_includes_the_inverted_anthropic_flag() {
    let (port, log) = mock::start_mock_server_with_log();
    for format in formats() {
        for parallel in [true, false] {
            let model = resolved_model(port, format, parallel, false, "mock-model");
            let (body, events) = run(model, vec![read_tool()], &log).await;
            let body = body.unwrap_or_else(|| panic!("{format:?}: no request body"));
            assert!(failure_detail(&events).is_none(), "{events:?}");
            match format {
                ApiFormat::OpenAiChat => {
                    assert_eq!(
                        body.get("parallel_tool_calls").and_then(Value::as_bool),
                        (!parallel).then_some(parallel),
                        "Chat omits the default and only sends an explicit opt-out: {body}"
                    );
                    assert!(
                        body.get("tool_choice").is_none(),
                        "Chat never sends a tool choice: {body}"
                    );
                }
                ApiFormat::OpenAiResponses => {
                    assert_eq!(
                        body["parallel_tool_calls"],
                        json!(parallel),
                        "Responses retains explicit parallel defaults: {body}"
                    );
                    assert_eq!(body["tool_choice"], json!("auto"), "{body}");
                }
                ApiFormat::AnthropicMessages => {
                    let expected = if parallel {
                        Value::Null
                    } else {
                        json!({"type": "auto", "disable_parallel_tool_use": true})
                    };
                    assert_eq!(
                        body["tool_choice"], expected,
                        "Anthropic only emits a non-default parallel policy: {body}"
                    );
                    assert!(
                        body.get("parallel_tool_calls").is_none(),
                        "Anthropic has no top-level parallel field: {body}"
                    );
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_less_requests_omit_every_policy_field() {
    let (port, log) = mock::start_mock_server_with_log();
    for format in formats() {
        let model = resolved_model(port, format, false, true, "mock-model");
        let (body, events) = run(model, Vec::new(), &log).await;
        let body = body.unwrap_or_else(|| panic!("{format:?}: no body"));
        assert!(failure_detail(&events).is_none(), "{events:?}");
        assert!(body.get("tool_choice").is_none(), "{format:?}: {body}");
        assert!(
            body.get("parallel_tool_calls").is_none(),
            "{format:?}: {body}"
        );
        assert!(body.get("tools").is_none(), "{format:?}: {body}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_strict_is_encoded_per_tool() {
    let (port, log) = mock::start_mock_server_with_log();
    let tools = vec![declared_strict(&mut read_tool(), true), grep_tool()];
    for format in formats() {
        let model_id = if format == ApiFormat::AnthropicMessages {
            "claude-sonnet-4-5"
        } else {
            "mock-model"
        };
        let model = resolved_model(port, format, true, true, model_id);
        let (body, events) = run(model, tools.clone(), &log).await;
        let body = body.unwrap_or_else(|| panic!("{format:?}: no body"));
        assert!(failure_detail(&events).is_none(), "{events:?}");
        let wire = body["tools"].as_array().expect("tools array");
        match format {
            ApiFormat::OpenAiChat => {
                assert_eq!(wire[0]["function"]["strict"], json!(true), "{body}");
                assert_eq!(
                    wire[0]["function"]["parameters"]["additionalProperties"],
                    json!(false)
                );
                assert!(
                    wire[1]["function"].get("strict").is_none(),
                    "an undeclared tool is never strictified: {body}"
                );
                assert_eq!(
                    wire[1]["function"]["parameters"]["additionalProperties"],
                    Value::Null,
                    "the undeclared schema stays untouched: {body}"
                );
            }
            ApiFormat::OpenAiResponses => {
                assert_eq!(wire[0]["strict"], json!(true), "{body}");
                assert_eq!(wire[1]["strict"], json!(false), "{body}");
                assert_eq!(wire[1]["parameters"]["additionalProperties"], Value::Null);
            }
            ApiFormat::AnthropicMessages => {
                assert_eq!(wire[0]["strict"], json!(true), "{body}");
                assert!(
                    wire[1].get("strict").is_none(),
                    "no fabricated strict field on Anthropic: {body}"
                );
                assert_eq!(wire[1]["input_schema"]["additionalProperties"], Value::Null);
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opt_in_strict_degrades_when_capability_is_off() {
    let (port, log) = mock::start_mock_server_with_log();
    for format in formats() {
        let model_id = if format == ApiFormat::AnthropicMessages {
            "claude-sonnet-4-5"
        } else {
            "mock-model"
        };
        let model = resolved_model(port, format, true, false, model_id);
        let (body, events) = run(model, vec![declared_strict(&mut read_tool(), true)], &log).await;
        let body = body.unwrap_or_else(|| {
            panic!("{format:?}: capability-off strict must not fail: {events:?}")
        });
        assert!(failure_detail(&events).is_none(), "{events:?}");
        match format {
            ApiFormat::OpenAiChat => assert!(
                body["tools"][0]["function"].get("strict").is_none(),
                "{body}"
            ),
            ApiFormat::OpenAiResponses => assert_eq!(body["tools"][0]["strict"], json!(false)),
            ApiFormat::AnthropicMessages => {
                assert!(body["tools"][0].get("strict").is_none(), "{body}")
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opt_in_strict_degrades_when_the_schema_is_inexpressible() {
    let (port, log) = mock::start_mock_server_with_log();
    let mut tool = read_tool();
    tool["function"]["parameters"] = json!({"$ref": "#/definitions/path"});
    let tool = declared_strict(&mut tool, true);
    for format in formats() {
        let model_id = if format == ApiFormat::AnthropicMessages {
            "claude-sonnet-4-5"
        } else {
            "mock-model"
        };
        let model = resolved_model(port, format, true, true, model_id);
        let (body, events) = run(model, vec![tool.clone()], &log).await;
        let body = body.unwrap_or_else(|| {
            panic!("{format:?}: inexpressible strict must not fail: {events:?}")
        });
        assert!(failure_detail(&events).is_none(), "{events:?}");
        match format {
            ApiFormat::OpenAiChat => {
                assert!(
                    body["tools"][0]["function"].get("strict").is_none(),
                    "{body}"
                )
            }
            ApiFormat::OpenAiResponses => assert_eq!(body["tools"][0]["strict"], json!(false)),
            ApiFormat::AnthropicMessages => {
                assert!(body["tools"][0].get("strict").is_none(), "{body}")
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undeclared_tools_are_never_strictified() {
    let (port, log) = mock::start_mock_server_with_log();
    for format in formats() {
        let model_id = if format == ApiFormat::AnthropicMessages {
            "claude-sonnet-4-5"
        } else {
            "mock-model"
        };
        let model = resolved_model(port, format, true, true, model_id);
        let (body, events) = run(model, vec![read_tool()], &log).await;
        let body = body.expect("body");
        assert!(failure_detail(&events).is_none(), "{events:?}");
        match format {
            ApiFormat::OpenAiChat => assert!(
                body["tools"][0]["function"].get("strict").is_none(),
                "{body}"
            ),
            ApiFormat::OpenAiResponses => assert_eq!(body["tools"][0]["strict"], json!(false)),
            ApiFormat::AnthropicMessages => {
                assert!(body["tools"][0].get("strict").is_none(), "{body}")
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_known_unsupported_models_degrade_opt_in_strict() {
    let (port, log) = mock::start_mock_server_with_log();
    for model_id in [
        "claude-3-5-sonnet-20241022",
        "claude-sonnet-4-20250514",
        "us.anthropic.claude-opus-4@20250514-v1:0",
    ] {
        let model = resolved_model(port, ApiFormat::AnthropicMessages, true, true, model_id);
        let (body, events) = run(model, vec![declared_strict(&mut read_tool(), true)], &log).await;
        let body = body.unwrap_or_else(|| {
            panic!("{model_id}: unsupported-model strict must degrade: {events:?}")
        });
        assert!(failure_detail(&events).is_none(), "{events:?}");
        assert!(
            body["tools"][0].get("strict").is_none(),
            "{model_id}: known-unsupported model must not send strict: {body}"
        );
    }
}

/// Chat has no hosted-search standard: a configured `web_search_tool` must
/// never leak into the request (an application-provided search tool handles it instead).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_never_sends_a_hosted_search_tool() {
    let (port, log) = mock::start_mock_server_with_log();
    let mut model = resolved_model(port, ApiFormat::OpenAiChat, true, false, "mock-model");
    model.cap_web_search = true;
    model.web_search_tool = Some(json!({"type": "web_search", "web_search": {"enable": true}}));
    let (body, events) = run(model, vec![read_tool()], &log).await;
    let body = body.expect("body");
    assert!(failure_detail(&events).is_none(), "{events:?}");
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(1), "{body}");
    assert_eq!(body["tools"][0]["type"], json!("function"), "{body}");
}

/// Anthropic gets the flag from the real tool message; the other protocols
/// have no equivalent field and must not invent one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_tool_result_is_error_is_encoded_and_other_formats_ignore_it() {
    let (port, log) = mock::start_mock_server_with_log();
    for format in formats() {
        for is_error in [false, true] {
            let model = resolved_model(port, format, true, false, "mock-model");
            let before = log.lock().expect("log").len();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(stream_chat(
                model,
                vec![
                    ChatMsg::user("hello".into()),
                    ChatMsg::assistant(
                        String::new(),
                        vec![pig_provider::ToolCall {
                            id: "call-1".into(),
                            name: "Read".into(),
                            arguments: "{}".into(),
                        }],
                        None,
                    ),
                    ChatMsg::tool_result("call-1", "result".into()).with_error(is_error),
                ],
                vec![read_tool()],
                tx,
                CallControl::new(CancellationToken::new()),
            ));
            while rx.recv().await.is_some() {}
            task.await.expect("joins");
            let body = log
                .lock()
                .expect("log")
                .get(before)
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .expect("request body");
            if format == ApiFormat::AnthropicMessages {
                let blocks = body["messages"]
                    .as_array()
                    .expect("messages")
                    .iter()
                    .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
                    .collect::<Vec<_>>();
                let result = blocks
                    .iter()
                    .find(|block| block["type"] == "tool_result")
                    .expect("tool_result block");
                if is_error {
                    assert_eq!(
                        result["is_error"],
                        json!(true),
                        "is_error must sit on the tool_result block: {body}"
                    );
                } else {
                    assert!(
                        result.get("is_error").is_none(),
                        "success omits the field: {body}"
                    );
                }
            } else {
                assert!(
                    !body.to_string().contains("is_error"),
                    "{format:?} has no equivalent field: {body}"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_sidecar_omits_policy_without_tools_and_stays_auto_with_tools() {
    let (port, log) = mock::start_mock_server_with_log();
    let model = resolved_model(port, ApiFormat::OpenAiResponses, false, false, "mock-model");
    let before = log.lock().expect("log").len();
    complete_messages(
        &model,
        &[ChatMsg::user("hello".into())],
        &[],
        &CancellationToken::new(),
    )
    .await
    .expect("tool-less sidecar succeeds");
    let body: Value = serde_json::from_str(&log.lock().expect("log")[before]).expect("body");
    assert!(body.get("tool_choice").is_none(), "{body}");
    assert!(body.get("parallel_tool_calls").is_none(), "{body}");
    assert!(body.get("tools").is_none(), "{body}");

    let before = log.lock().expect("log").len();
    complete_messages(
        &model,
        &[ChatMsg::user("hello".into())],
        &[read_tool()],
        &CancellationToken::new(),
    )
    .await
    .expect("sidecar with tools succeeds");
    let body: Value = serde_json::from_str(&log.lock().expect("log")[before]).expect("body");
    assert_eq!(body["tool_choice"], json!("auto"), "{body}");
    assert_eq!(
        body["parallel_tool_calls"],
        json!(false),
        "the sidecar uses the model's parallel setting, not a default: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_receipt_persists_but_never_leaks_as_a_provider_field() {
    let (port, log) = mock::start_mock_server_with_log();
    let mut stored =
        serde_json::to_value(ChatMsg::tool_result("call-1", "Stopped".into()).with_error(true))
            .expect("serialize receipt");
    stored["stopped"] = json!(true);
    let stopped: ChatMsg = serde_json::from_value(stored).expect("stopped receipt parses");
    let roundtrip = serde_json::to_value(&stopped).expect("stopped receipt roundtrip");
    assert_eq!(roundtrip["stopped"], json!(true), "{roundtrip}");

    for format in formats() {
        let model = resolved_model(port, format, true, false, "m");
        let messages = vec![
            ChatMsg::user("hello".into()),
            ChatMsg::assistant(
                String::new(),
                vec![pig_provider::ToolCall {
                    id: "call-1".into(),
                    name: "Read".into(),
                    arguments: "{}".into(),
                }],
                None,
            ),
            stopped.clone(),
        ];
        let (body, events) = run_with_messages(model, messages, vec![read_tool()], &log).await;
        let body = body.unwrap_or_else(|| panic!("{format:?}: stopped receipt failed: {events:?}"));
        assert!(
            !body.to_string().contains("\"stopped\""),
            "{format:?} must not leak internal stopped metadata: {body}"
        );
        assert!(
            body.to_string().contains(pig_provider::STOPPED_TOOL_OUTPUT),
            "{format:?} must project the cancellation guidance: {body}"
        );
        if format == ApiFormat::AnthropicMessages {
            let result = body["messages"]
                .as_array()
                .expect("messages")
                .iter()
                .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
                .find(|block| block["type"] == "tool_result")
                .expect("tool_result");
            assert_eq!(result["is_error"], json!(true), "{body}");
        }
    }
}

#[test]
fn stopped_receipt_builder_sets_model_error_and_legacy_defaults_to_false() {
    let stopped = ChatMsg::stopped_tool_result("call-1", "Stopped".into());
    assert!(stopped.stopped);
    assert!(stopped.is_error);
    assert_eq!(stopped.content.as_deref(), Some("Stopped"));
    let stored = serde_json::to_value(&stopped).expect("serialize stopped receipt");
    assert_eq!(stored["stopped"], json!(true));
    assert_eq!(stored["is_error"], json!(true));

    let legacy: ChatMsg = serde_json::from_value(json!({
        "role": "tool",
        "content": "ok",
        "tool_call_id": "call-1",
    }))
    .expect("legacy receipt parses");
    assert!(!legacy.stopped);
    assert!(!legacy.is_error);
    assert!(!ChatMsg::tool_result("call-1", "ok".into()).stopped);
}
