//! Request-feature matrix against the real in-process HTTP mock: session
//! cache keys, Anthropic cache breakpoints, official token/reasoning fields,
//! PDF document projections, hosted-search replay, sidecar content arrays, and
//! context-window error classification.

use pig_protocol::{ApiFormat, CoreError};
use pig_provider::{
    AssistantReplay, CallControl, ChatDocument, ChatMsg, ProviderEvent, ResolvedModel, ToolCall,
    ToolCallSettings, mock, stream_chat,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

mod cache;
mod documents;
mod overflow;
mod search;
mod sidecar;

type RequestLog = Arc<Mutex<Vec<String>>>;

fn model(port: u16, format: ApiFormat) -> ResolvedModel {
    ResolvedModel {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "mock-key".into(),
        model: match format {
            ApiFormat::AnthropicMessages => "claude-sonnet-4-5".into(),
            _ => "mock-model".into(),
        },
        context_window: 128_000,
        max_output_tokens: 8_192,
        api_format: format,
        reasoning_params: None,
        cap_structured: false,
        cap_strict_tools: false,
        cap_web_search: false,
        web_search_tool: None,
        input_image: false,
        input_pdf: true,
        anthropic_cache_control: false,
        replay_native_search: false,
        prompt_cache_key: None,
        provider_name: "mock".into(),
        tool_settings: ToolCallSettings::new(true),
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

fn pdf_with(filename: &str, data_base64: &str, label: Option<&str>) -> ChatDocument {
    ChatDocument {
        media_type: "application/pdf".into(),
        data_base64: data_base64.into(),
        filename: filename.into(),
        label: label.map(str::to_string),
        token_estimate: None,
    }
}

fn pdf(filename: &str) -> ChatDocument {
    pdf_with(filename, "JVBERi0xLjQK", None)
}

fn user_with_pdf(text: &str, document: ChatDocument) -> ChatMsg {
    let mut message = ChatMsg::user(text.into());
    message.documents = vec![document];
    message
}

fn tool_result_with_pdf(call_id: &str, output: &str, document: ChatDocument) -> ChatMsg {
    let mut message = ChatMsg::tool_result(call_id, output.into());
    message.documents = vec![document];
    message
}

fn populated_history() -> Vec<ChatMsg> {
    vec![
        ChatMsg::system("frozen system".into()),
        ChatMsg::user("hello".into()),
        ChatMsg::assistant(
            "reading".into(),
            vec![ToolCall {
                id: "call-1".into(),
                name: "Read".into(),
                arguments: "{\"path\":\"a\"}".into(),
            }],
            None,
        ),
        ChatMsg::tool_result("call-1", "file contents".into()),
    ]
}

async fn run(
    model: ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<Value>,
    log: &RequestLog,
) -> (Option<Value>, Vec<ProviderEvent>) {
    let before = log.lock().expect("log").len();
    let format = model.api_format;
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
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

fn failed_error(events: &[ProviderEvent]) -> Option<&CoreError> {
    events.iter().find_map(|event| match event {
        ProviderEvent::Failed(error) => Some(error),
        _ => None,
    })
}

fn replay_of(events: &[ProviderEvent]) -> AssistantReplay {
    events
        .iter()
        .find_map(|event| match event {
            ProviderEvent::AssistantReplay(replay) => Some(replay.clone()),
            _ => None,
        })
        .expect("native replay")
}

fn cache_breakpoints(body: &Value) -> usize {
    body.to_string().matches("\"cache_control\"").count()
}

fn last_sidecar_body(log: &RequestLog, before: usize) -> Value {
    serde_json::from_str(&log.lock().expect("log")[before]).expect("request body")
}

fn assert_failure_free(events: &[ProviderEvent]) {
    if let Some(error) = failed_error(events) {
        panic!("unexpected provider failure: {error:?}");
    }
}
