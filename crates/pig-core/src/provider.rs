use futures_util::StreamExt as _;
use pig_protocol::{ApiFormat, ConnTestResult, CoreError};
use serde::{Deserialize, Serialize};

/// reqwest's Display only says "error sending request"; the real cause (DNS/certificate/proxy/connection refused)
/// is on the source chain. Fold the deepest cause into detail for easier diagnosis.
fn net_err(e: reqwest::Error) -> CoreError {
    let mut root: Option<String> = None;
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        root = Some(s.to_string());
        source = s.source();
    }
    let detail = match root {
        Some(root) if root != e.to_string() => format!("{e} ({root})"),
        _ => e.to_string(),
    };
    CoreError::Network { detail }
}

/// CoreError -> single-line English text (for the model channel: subagent failure receipts, call trace persistence).
/// Multilingual UI goes through pig-app's kind mapping; core only produces English fallback primary-language text;
/// detail is always the upstream English original, passed through verbatim.
pub(crate) fn core_error_en(error: &CoreError) -> String {
    match error {
        CoreError::Network { detail } => format!("Network error: {detail}"),
        CoreError::StreamRead { detail } => format!("Failed to read stream: {detail}"),
        CoreError::Internal { detail } => detail.clone(),
        CoreError::Unknown => "Unknown error".to_string(),
        // Other variants never go through the model channel (UI-only); fall back to the Debug shape for diagnosability
        other => format!("{other:?}"),
    }
}

/// Streaming requests cannot set an overall timeout (the response body may not end for a long time); only the connect time is limited.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

/// Retry policy: exponential backoff, at most 10 retries; starting at 1s, doubling, capped at 30s.
/// Retries happen only "before the response is established" (connect/TLS/send failures, 429/5xx) --
/// once the response stream is established there is no retry, avoiding duplicate output of already-streamed content.
const MAX_RETRIES: u32 = 10;
const RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(1);
const RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(30);

enum SendOutcome {
    Response(reqwest::Response),
    Cancelled,
}

fn retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn backoff_delay(retry: u32, response: Option<&reqwest::Response>) -> std::time::Duration {
    // When a 429/5xx carries Retry-After, respect the server's pacing first (capped at 120s)
    if let Some(secs) = response
        .and_then(|r| r.headers().get(reqwest::header::RETRY_AFTER))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        return std::time::Duration::from_secs(secs.min(120));
    }
    RETRY_BASE
        .saturating_mul(2u32.saturating_pow(retry))
        .min(RETRY_CAP)
}

async fn send_with_retry(
    build: impl Fn() -> reqwest::RequestBuilder,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<SendOutcome, CoreError> {
    let mut retry = 0;
    loop {
        let result = tokio::select! {
            result = build().send() => result,
            _ = cancel.cancelled() => return Ok(SendOutcome::Cancelled),
        };
        // Errors in the send phase can only be network-class errors (request construction errors already fail at the builder stage)
        let retryable = match &result {
            Ok(response) => retryable_status(response.status()),
            Err(_) => true,
        };
        if !retryable || retry >= MAX_RETRIES {
            return match result {
                // After status-code error retries are exhausted, the response body is left for the caller to format (HTTP xxx: ...)
                Ok(response) => Ok(SendOutcome::Response(response)),
                Err(e) => Err(net_err(e)),
            };
        }
        let delay = backoff_delay(retry, result.as_ref().ok());
        retry += 1;
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = cancel.cancelled() => return Ok(SendOutcome::Cancelled),
        }
    }
}

/// Resolved model endpoint config (product of merging provider + model + reasoning level)
#[derive(Clone, Debug)]
pub struct ResolvedModel {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub api_format: ApiFormat,
    pub reasoning_params: Option<serde_json::Value>,
    pub cap_web_search: bool,
    pub web_search_tool: Option<serde_json::Value>,
    /// Model supports image input (gate for ReadMediaFile)
    pub input_image: bool,
    /// For display
    pub provider_name: String,
}

/// Images entering the context with a message (ReadMediaFile output): Anthropic puts them into content blocks,
/// OpenAI splits them into a following user image_url message (see to_openai_messages)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatImage {
    pub media_type: String,
    pub data_base64: String,
    /// For display/labeling (the text part of the OpenAI split message); not consumed on the Anthropic path
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Subagent context persistence ({session}.agents/*.jsonl) needs deserialization, hence Deserialize
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMsg {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallWire>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Images from tool output: the OpenAI path bypasses direct serde serialization (see to_openai_messages),
    /// this field is only read by Anthropic's custom construction
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ChatImage>,
    /// Assistant thinking content: only echoed back to Anthropic endpoints (thinking mode requires
    /// content[].thinking, otherwise the second turn 400s); excluded from serialization -- OpenAI-compatible endpoints
    /// (DeepSeek native) require not echoing reasoning_content back.
    #[serde(skip_serializing)]
    pub reasoning: Option<String>,
}

impl ChatMsg {
    pub fn system(content: String) -> Self {
        Self {
            role: "system".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
            reasoning: None,
        }
    }

    pub fn user(content: String) -> Self {
        Self {
            role: "user".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
            reasoning: None,
        }
    }

    pub fn assistant(
        content: String,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<String>,
    ) -> Self {
        Self {
            role: "assistant".into(),
            content: (!content.is_empty()).then_some(content),
            tool_calls: (!tool_calls.is_empty())
                .then(|| tool_calls.iter().map(ToolCall::to_wire).collect()),
            tool_call_id: None,
            images: vec![],
            reasoning: reasoning.filter(|r| !r.is_empty()),
        }
    }

    pub fn tool_result(call_id: &str, output: String) -> Self {
        Self {
            role: "tool".into(),
            content: Some(output),
            tool_calls: None,
            tool_call_id: Some(call_id.to_string()),
            images: vec![],
            reasoning: None,
        }
    }

    /// Tool result carrying images (ReadMediaFile): the images enter the context along with history
    pub fn tool_result_with_images(call_id: &str, output: String, images: Vec<ChatImage>) -> Self {
        let mut msg = Self::tool_result(call_id, output);
        msg.images = images;
        msg
    }
}

#[derive(Clone, Debug, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Subagent context persistence ({session}.agents/*.jsonl) needs deserialization, hence Deserialize
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallWire {
    pub id: String,
    /// Always "function": &'static str cannot be deserialized, so store a String (serialized shape unchanged)
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionWire,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionWire {
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    pub fn to_wire(&self) -> ToolCallWire {
        ToolCallWire {
            id: self.id.clone(),
            kind: "function".to_string(),
            function: FunctionWire {
                name: self.name.clone(),
                arguments: self.arguments.clone(),
            },
        }
    }
}

/// Internal provider -> session events during streaming
#[derive(Debug)]
pub enum ProviderEvent {
    Reasoning(String),
    Text(String),
    ToolCalls(Vec<ToolCall>),
    /// Token usage for a single request: input = uncached input (cache_read is the other
    /// portion of it served from cache; the two sum to total input), output is for accounting aggregation,
    /// used is the model-reported total consumption of this request (for the usage watermark check), total is the model context window,
    /// reasoning_output is the reasoning/thinking slice of output (informational; already counted in output)
    Usage {
        input: u64,
        cache_read: u64,
        output: u64,
        used: u64,
        total: u64,
        reasoning_output: u64,
    },
    Finished,
    /// Request/streaming failure: the structured error goes straight to the UI (passed through as Event::Error)
    Failed(CoreError),
}

/// Internal stream-call error: the CoreError to surface plus whether a silent
/// retry is allowed. Retryable = in-band 200 errors whose type maps to a
/// retryable status (Anthropic overloaded_error/rate_limit_error/api_error);
/// the retry is additionally gated on "no content forwarded yet" in
/// stream_chat (the same boundary as the empty-completion retry). Errors after
/// send_with_retry's pre-response budget are never retryable here.
pub(crate) struct CallError {
    pub error: CoreError,
    pub retryable: bool,
}

impl From<CoreError> for CallError {
    fn from(error: CoreError) -> Self {
        Self {
            error,
            retryable: false,
        }
    }
}

pub async fn stream_chat(
    config: ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    cancel: tokio_util::sync::CancellationToken,
) {
    // Empty-completion guard (same anomaly class as ZCode's
    // empty-completion-retry): a 200 stream that ends with no text, no
    // reasoning and no tool calls gets ONE silent retry. Events stream through
    // live EXCEPT Finished, which the forwarder holds back until the outcome
    // is known — the session treats Finished as step end, so an empty attempt
    // must never emit Finished followed by the retry's content.
    let mut retried = false;
    let mut tx = tx;
    loop {
        let (itx, mut irx) = tokio::sync::mpsc::unbounded_channel::<ProviderEvent>();
        let saw_content = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = saw_content.clone();
        let forward = tokio::spawn(async move {
            let mut held_finish = false;
            while let Some(event) = irx.recv().await {
                if matches!(event, ProviderEvent::Finished) {
                    held_finish = true;
                    continue;
                }
                if matches!(
                    event,
                    ProviderEvent::Text(_)
                        | ProviderEvent::Reasoning(_)
                        | ProviderEvent::ToolCalls(_)
                ) {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                if tx.send(event).is_err() {
                    break;
                }
            }
            (tx, held_finish)
        });
        let result = match config.api_format {
            ApiFormat::OpenAiChat => {
                stream_openai(&config, messages.clone(), tools.clone(), &itx, &cancel).await
            }
            ApiFormat::AnthropicMessages => {
                stream_anthropic(&config, messages.clone(), tools.clone(), &itx, &cancel).await
            }
        };
        drop(itx);
        let Ok((back, held_finish)) = forward.await else {
            return;
        };
        tx = back;
        match result {
            Err(call_error) => {
                // Retryable in-band error (Anthropic overload/rate-limit
                // arriving as a 200 SSE error event): one silent retry, only
                // while nothing visible was forwarded yet
                if call_error.retryable
                    && !saw_content.load(std::sync::atomic::Ordering::Relaxed)
                    && !retried
                    && !cancel.is_cancelled()
                {
                    retried = true;
                    tracing::warn!(
                        "provider reported a retryable in-band error, retrying once: {}",
                        core_error_en(&call_error.error)
                    );
                    continue;
                }
                let _ = tx.send(ProviderEvent::Failed(call_error.error));
                return;
            }
            Ok(()) => {
                let empty = !saw_content.load(std::sync::atomic::Ordering::Relaxed);
                if empty && !retried && !cancel.is_cancelled() {
                    retried = true;
                    tracing::warn!(
                        "provider stream completed empty (no text/reasoning/tool calls), retrying once"
                    );
                    continue;
                }
                if held_finish {
                    let _ = tx.send(ProviderEvent::Finished);
                }
                return;
            }
        }
    }
}

// ---------------- OpenAI Chat Completions ----------------

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    /// Custom-built (see to_openai_messages): an image-carrying tool result splits into a tool text message + a user image message
    messages: &'a [serde_json::Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [serde_json::Value]>,
    stream: bool,
    stream_options: StreamOptions,
    max_tokens: u64,
}

mod anthropic;
mod openai;
mod sidecar;

pub use anthropic::anthropic_web_search_tool;
pub(crate) use anthropic::*;
pub use openai::openai_web_search_tool;
pub(crate) use openai::*;
pub use sidecar::{complete_messages, complete_text, net_test_blocking, test_provider};

fn finish(tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>, tool_calls: &mut Vec<ToolCall>) {
    // Models occasionally emit nameless tool_use (name: null): filter them out to avoid empty "unknown tool" calls;
    // since they never enter history, no tool_result needs to be backfilled for them
    let calls: Vec<ToolCall> = std::mem::take(tool_calls)
        .into_iter()
        .filter(|call| !call.name.trim().is_empty())
        .collect();
    if !calls.is_empty() {
        let _ = tx.send(ProviderEvent::ToolCalls(calls));
    }
    let _ = tx.send(ProviderEvent::Finished);
}

fn merge_reasoning_params(body: &mut serde_json::Value, config: &ResolvedModel) {
    if let (serde_json::Value::Object(map), Some(params)) = (body, &config.reasoning_params)
        && let serde_json::Value::Object(extra) = params
    {
        for (key, value) in extra {
            map.insert(key.clone(), value.clone());
        }
    }
}

// ---------------- Anthropic Messages ----------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_filters_nameless_tool_calls() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut calls = vec![
            ToolCall {
                id: "a".into(),
                name: String::new(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "b".into(),
                name: "Bash".into(),
                arguments: "{}".into(),
            },
        ];
        finish(&tx, &mut calls);
        match rx.try_recv() {
            Ok(ProviderEvent::ToolCalls(calls)) => {
                assert_eq!(calls.len(), 1, "nameless calls should be filtered");
                assert_eq!(calls[0].name, "Bash");
            }
            other => panic!("remaining named call should come through: {other:?}"),
        }
        assert!(matches!(rx.try_recv(), Ok(ProviderEvent::Finished)));
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
        let (_system, out) = to_anthropic_messages(&messages);
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
        let (_system, out) = to_anthropic_messages(&messages);
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
        // OpenAI: user content becomes parts (text first, image_url after)
        let out = to_openai_messages(&[msg.clone()]);
        assert_eq!(out.len(), 1, "user with images is not split");
        let parts = out[0]["content"].as_array().expect("parts");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "view image");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
        // Anthropic: user content becomes blocks (image first, text after)
        let (_s, out) = to_anthropic_messages(&[msg]);
        let blocks = out[0]["content"].as_array().expect("blocks");
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["data"], "QUJD");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "view image");
    }

    #[test]
    fn finish_all_nameless_emits_no_tool_calls() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut calls = vec![ToolCall::default()];
        finish(&tx, &mut calls);
        assert!(matches!(rx.try_recv(), Ok(ProviderEvent::Finished)));
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
                    name: "Bash".into(),
                    arguments: "{}".into(),
                }],
                Some("pondered".into()),
            ),
            ChatMsg::tool_result("c1", "ok".into()),
        ];
        let (_system, out) = to_anthropic_messages(&messages);
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
        let (_system, out) = to_anthropic_messages(&messages);
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
            cap_web_search: cap,
            web_search_tool: tool,
            input_image: false,
            provider_name: "p".into(),
        }
    }

    #[test]
    fn anthropic_web_search_tool_injection() {
        // cap off -> no injection
        assert!(anthropic_web_search_tool(&search_model(false, None)).is_none());
        // cap on, no customization -> default web_search_20250305
        let tool = anthropic_web_search_tool(&search_model(true, None))
            .expect("cap enabled should inject");
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

    #[test]
    fn openai_web_search_tool_injection() {
        // cap off -> no injection
        assert!(openai_web_search_tool(&search_model(false, None)).is_none());
        // cap on but no customization -> no injection (OpenAI-compatible endpoints have no server-side search standard)
        assert!(openai_web_search_tool(&search_model(true, None)).is_none());
        // cap on with customization (e.g. Zhipu) -> inject
        let custom = serde_json::json!({
            "type": "web_search",
            "web_search": {"enable": true, "search_result": true}
        });
        assert_eq!(
            openai_web_search_tool(&search_model(true, Some(custom.clone()))).unwrap(),
            custom
        );
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

    /// OpenAI side: pass the wire shape through verbatim; append the server-side search tool only with cap+customization
    #[test]
    fn openai_request_tools_keeps_wire_shape() {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "Read", "description": "d", "parameters": {}}
        })];
        let out = openai_request_tools(&search_model(false, None), &tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "function");
        let custom = serde_json::json!({"type": "web_search", "web_search": {"enable": true}});
        let out = openai_request_tools(&search_model(true, Some(custom)), &tools);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["type"], "web_search");
    }
}
