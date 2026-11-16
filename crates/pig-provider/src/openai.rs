use futures_util::StreamExt as _;
use serde::Deserialize;

use pig_protocol::CoreError;

use crate::chat::{ChatDocument, ChatMsg, ResolvedModel};
use crate::events::{CallControl, CallError, ProviderEvent};
use crate::identity::apply_sdk_headers;
use crate::retry::{SendOutcome, http_client, send_with_retry};

/// Internal ChatMsg list -> OpenAI messages array.
/// OpenAI tool-role messages cannot carry images or documents: a media-carrying
/// tool result (ReadMediaFile) splits into the tool text message followed by a
/// user media message. Documents become `file` parts with a
/// `data:application/pdf;base64,...` URL.
/// `is_error`/`stopped` are internal execution markers and are stripped from
/// every Chat message; `documents` never serializes through `ChatMsg` serde.
pub(crate) fn to_openai_messages(messages: &[ChatMsg]) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.images.is_empty() && msg.documents.is_empty() {
            out.push(message_value(msg));
            continue;
        }
        match msg.role.as_str() {
            // The tool role cannot carry media: split into tool text + an
            // immediately following user media message
            "tool" => {
                let mut text_only = msg.clone();
                text_only.images = vec![];
                text_only.documents = vec![];
                out.push(message_value(&text_only));
                let mut parts = Vec::with_capacity(msg.images.len() * 2 + msg.documents.len());
                for img in &msg.images {
                    let label = img.label.as_deref().unwrap_or("image");
                    parts.push(serde_json::json!({
                        "type": "text",
                        "text": format!("[ReadMediaFile output image: {label}]"),
                    }));
                    parts.push(image_part(img));
                }
                parts.extend(msg.documents.iter().map(document_part));
                out.push(serde_json::json!({"role": "user", "content": parts}));
            }
            // User message with media (pasted image / attached PDF): content
            // becomes a parts array (text first, media after)
            "user" => {
                let mut parts = Vec::with_capacity(msg.images.len() + msg.documents.len() + 1);
                // A media-only user turn must not carry an empty text part.
                if let Some(text) = msg.content.as_deref().filter(|text| !text.is_empty()) {
                    parts.push(serde_json::json!({"type": "text", "text": text}));
                }
                for img in &msg.images {
                    // Labeled images (composer mention anchor "Image N") get a
                    // "[Image N]" text marker so the model can correlate text
                    // references with attachments
                    if let Some(label) = &img.label {
                        parts.push(serde_json::json!({
                            "type": "text",
                            "text": format!("[{label}]"),
                        }));
                    }
                    parts.push(image_part(img));
                }
                parts.extend(msg.documents.iter().map(document_part));
                out.push(serde_json::json!({"role": "user", "content": parts}));
            }
            _ => out.push(message_value(msg)),
        }
    }
    out
}

fn image_part(img: &crate::chat::ChatImage) -> serde_json::Value {
    serde_json::json!({
        "type": "image_url",
        "image_url": {
            "url": format!("data:{};base64,{}", img.media_type, img.data_base64),
        },
    })
}

/// Chat wire PDF part: a data URL, matching the file-upload shape the
/// OpenAI-compatible SDKs send.
fn document_part(document: &ChatDocument) -> serde_json::Value {
    serde_json::json!({
        "type": "file",
        "file": {
            "filename": document.filename,
            "file_data": format!("data:{};base64,{}", document.media_type, document.data_base64),
        },
    })
}

/// One message serialized for the Chat Completions wire, without internal
/// tool execution metadata (this protocol has no such fields).
fn message_value(msg: &ChatMsg) -> serde_json::Value {
    let mut value = serde_json::to_value(msg).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.remove("is_error");
        object.remove("stopped");
        object.remove("documents");
    }
    if msg.stopped {
        value["content"] = serde_json::Value::String(crate::chat::tool_result_output(msg));
    }
    value
}

/// Request-level tool list assembly (shared by streaming/non-streaming, to
/// prevent drift between the two): the OpenAI wire shape with the selective
/// strict preference applied. Chat never injects a hosted search tool.
pub(crate) fn openai_request_tools(
    config: &ResolvedModel,
    tools: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| apply_strict_function_tool(config, tool))
        .collect()
}

fn apply_strict_function_tool(
    config: &ResolvedModel,
    tool: &serde_json::Value,
) -> serde_json::Value {
    let name = tool["function"]["name"].as_str().unwrap_or_default();
    let support = if config.cap_strict_tools {
        crate::tool_settings::StrictSupport::Supported
    } else {
        crate::tool_settings::StrictSupport::Disabled
    };
    let strict_params = crate::tool_settings::strict_schema(
        support,
        crate::tool_settings::declared_strict(tool),
        &tool["function"]["parameters"],
        name,
        &config.model,
    );
    let mut out = tool.clone();
    match strict_params {
        Some(strict_params) => {
            out["function"]["parameters"] = strict_params;
            out["function"]["strict"] = serde_json::Value::Bool(true);
        }
        None => {
            // Chat has no per-tool "not strict" declaration: a preference we
            // cannot honor is omitted, never sent as a false promise.
            if let Some(function) = out["function"].as_object_mut() {
                function.remove("strict");
            }
        }
    }
    out
}

/// Pure Chat Completions request body builder shared by streaming and
/// sidecars. Official endpoints use `max_completion_tokens`; compatible
/// endpoints keep `max_tokens`.
pub(crate) fn build_chat_body(
    config: &ResolvedModel,
    messages: &[ChatMsg],
    tools: &[serde_json::Value],
    stream: bool,
) -> Result<serde_json::Value, CoreError> {
    crate::chat::validate_documents(config, messages)?;
    let tools = openai_request_tools(config, tools);
    let mut body = serde_json::json!({
        "model": config.model,
        "messages": to_openai_messages(messages),
        "stream": stream,
    });
    body[crate::endpoint::chat_token_limit_key(&config.base_url)] =
        serde_json::Value::from(config.max_output_tokens);
    if stream {
        body["stream_options"] = serde_json::json!({"include_usage": true});
    }
    let parallel = crate::tool_settings::chat_parallel(&config.tool_settings, &tools);
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools);
    }
    if let Some(parallel) = parallel {
        body["parallel_tool_calls"] = serde_json::Value::Bool(parallel);
    }
    if let Some(key) = &config.prompt_cache_key {
        body["prompt_cache_key"] = serde_json::Value::String(key.clone());
    }
    merge_chat_reasoning_params(&mut body, config);
    Ok(body)
}

/// Official OpenAI Chat only accepts standard reasoning fields; compatible
/// endpoints keep the four-field compatibility bundle.
fn merge_chat_reasoning_params(body: &mut serde_json::Value, config: &ResolvedModel) {
    let official = crate::endpoint::is_official_openai(&config.base_url);
    if let (serde_json::Value::Object(map), Some(serde_json::Value::Object(params))) =
        (body, &config.reasoning_params)
    {
        for (key, value) in params {
            if official && key != "reasoning_effort" {
                continue;
            }
            map.insert(key.clone(), value.clone());
        }
    }
}

pub(crate) async fn stream_openai(
    config: &ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    control: &CallControl,
) -> Result<(), CallError> {
    let client = http_client();
    let mut body = build_chat_body(config, &messages, &tools, true).map_err(CallError::from)?;
    if let Some(output) = control.structured_output() {
        crate::sidecar::apply_structured(&mut body, config.api_format, output);
    }

    let url = crate::endpoint::chat_completions_url(&config.base_url);
    // API wire log (PIG_LOG_API): no-op accumulator when disabled
    let mut api = crate::api_log::ApiCall::new("openai.chat", &config.provider_name, &config.model);
    api.request(&url, &body);

    let response = match send_with_retry(
        |retry| {
            apply_sdk_headers(
                client.post(&url).bearer_auth(&config.api_key),
                false,
                false,
                false,
                retry,
            )
            .json(&body)
        },
        control,
        tx,
    )
    .await?
    {
        SendOutcome::Response(response) => response,
        SendOutcome::Cancelled => {
            api.finish("cancelled", &[]);
            return Ok(());
        }
    };

    let mut response_metadata = crate::ProviderResponseMetadata::from_response(&response);
    let _ = tx.send(ProviderEvent::ResponseMetadata(response_metadata.clone()));
    let status = response.status();
    let idle = control.idle_timeout();
    if !status.is_success() {
        // The error body is a response body like any other: bound it with the
        // same per-chunk idle deadline and let cancellation win. A wedged body
        // must not hide the HTTP status behind an unbounded read.
        let detail =
            match crate::transport::read_body_with_idle(response, control.cancel_token(), idle)
                .await
            {
                Ok(crate::transport::BodyRead::Text(text)) => text,
                Ok(crate::transport::BodyRead::Cancelled) => {
                    api.finish("cancelled", &[]);
                    return Ok(());
                }
                Err(failure) => failure.partial,
            };
        let detail: String = detail.chars().take(500).collect();
        let error = crate::context_window::http_error(status, &detail);
        api.fail(&crate::core_error_en(&error));
        return Err(error.into());
    }

    let mut byte_stream = response.bytes_stream();
    // Spec-level SSE decoding (three line terminators, deferred trailing CR,
    // multi-line data, comments, incremental UTF-8) — same semantics as the
    // eventsource-parser package under the Vercel AI SDK
    let mut sse = crate::sse::SseDecoder::new();
    let mut tool_stream = crate::chat_tool_stream::ChatToolStream::new();
    let mut finish_reason = None;

    loop {
        let chunk = tokio::select! {
            chunk = tokio::time::timeout(idle, byte_stream.next()) => match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    let detail = crate::transport::idle_detail("Stream", idle);
                    api.fail(&detail);
                    return Err(CoreError::StreamRead { detail }.into());
                }
            },
            _ = control.cancelled() => {
                api.finish("cancelled", &[]);
                return Ok(());
            }
        };
        let Some(chunk) = chunk else { break };
        let bytes = chunk.map_err(|e| {
            CallError::from(CoreError::StreamRead {
                detail: e.to_string(),
            })
        })?;

        for event in sse.push(&bytes) {
            api.raw_line(&event.data);
            let data = event.data.trim();
            if data == "[DONE]" {
                if control.is_cancelled() {
                    api.finish("cancelled", &[]);
                    return Ok(());
                }
                return finish_openai_stream(tool_stream, finish_reason, &mut api, tx);
            }
            let value = match serde_json::from_str::<serde_json::Value>(data) {
                Ok(value) => value,
                Err(error) => {
                    let detail = format!("Invalid JSON in Chat Completions stream: {error}");
                    api.fail(&detail);
                    return Err(CoreError::Internal { detail }.into());
                }
            };
            // Business error inside an HTTP-200 stream (OpenAI-compatible
            // gateways do this instead of a non-2xx status; same class as
            // ZCode's ProviderBusinessError mid-stream detection)
            if let Some(detail) = business_error_detail(&value) {
                let error = crate::context_window::stream_error(detail);
                api.fail(&crate::core_error_en(&error));
                return Err(error.into());
            }
            let chunk = match serde_json::from_value::<OpenAiChunk>(value) {
                Ok(chunk) => chunk,
                Err(error) => {
                    let detail = format!("Invalid Chat Completions stream chunk: {error}");
                    api.fail(&detail);
                    return Err(CoreError::Internal { detail }.into());
                }
            };
            if response_metadata.response_id.is_none()
                && let Some(id) = chunk.id.filter(|id| !id.is_empty())
            {
                response_metadata.response_id = Some(id);
                let _ = tx.send(ProviderEvent::ResponseMetadata(response_metadata.clone()));
            }
            if let Some(usage) = chunk.usage.and_then(|usage| {
                normalize_usage(
                    usage.prompt_tokens.unwrap_or(0),
                    usage.completion_tokens.unwrap_or(0),
                    usage.total_tokens,
                    usage
                        .prompt_tokens_details
                        .and_then(|d| d.cached_tokens)
                        .unwrap_or(0),
                    usage
                        .completion_tokens_details
                        .and_then(|d| d.reasoning_tokens)
                        .unwrap_or(0),
                    config.context_window,
                )
            }) {
                api.usage(
                    usage.input,
                    usage.cache_read,
                    usage.output,
                    usage.used,
                    usage.total,
                    usage.reasoning_output,
                );
                let _ = tx.send(ProviderEvent::Usage {
                    input: usage.input,
                    cache_read: usage.cache_read,
                    output: usage.output,
                    used: usage.used,
                    total: usage.total,
                    reasoning_output: usage.reasoning_output,
                });
            }
            let Some(choice) = chunk.choices.and_then(|mut choices| {
                if choices.is_empty() {
                    None
                } else {
                    Some(choices.remove(0))
                }
            }) else {
                continue;
            };
            if let Some(delta) = choice.delta {
                // Reasoning field aliases: reasoning_content (DeepSeek-style) or
                // reasoning (gpt-oss variant; the SDK accepts both)
                if let Some(reasoning) = delta.reasoning_content.or(delta.reasoning)
                    && !reasoning.is_empty()
                {
                    api.reasoning(&reasoning);
                    let _ = tx.send(ProviderEvent::Reasoning(reasoning));
                }
                // delta.content leniency (SDK openai-compatible): a plain string
                // OR a parts array (z.ai/GLM-style streams carry nested thinking
                // parts that flatten to reasoning)
                if let Some(content) = &delta.content {
                    for piece in content_parts(content) {
                        match piece {
                            DeltaPiece::Text(text) if !text.is_empty() => {
                                api.text(&text);
                                let _ = tx.send(ProviderEvent::Text(text));
                            }
                            DeltaPiece::Reasoning(reasoning) if !reasoning.is_empty() => {
                                api.reasoning(&reasoning);
                                let _ = tx.send(ProviderEvent::Reasoning(reasoning));
                            }
                            _ => {}
                        }
                    }
                }
                if let Some(chunks) = delta.tool_calls {
                    for part in chunks {
                        tool_stream.push(
                            part.index,
                            part.id.as_deref(),
                            part.function.as_ref().and_then(|f| f.name.as_deref()),
                            part.function.as_ref().and_then(|f| f.arguments.as_deref()),
                            tx,
                        );
                    }
                }
            }
            if let Some(raw_reason) = choice.finish_reason.as_deref() {
                let reason = match raw_reason {
                    "stop" => pig_protocol::FinishReason::Stop,
                    "tool_calls" | "function_call" => pig_protocol::FinishReason::ToolCalls,
                    "length" => pig_protocol::FinishReason::Length,
                    "content_filter" => pig_protocol::FinishReason::ContentFilter,
                    other => {
                        // Strict rejection is the only sound handling: text
                        // already forwarded stays visible, while no Finished
                        // is invented and no uncertain tool call is emitted.
                        let detail = format!("Unknown Chat Completions finish reason: {other}");
                        api.fail(&detail);
                        return Err(CoreError::Internal { detail }.into());
                    }
                };
                finish_reason = Some(reason);
            }
        }
    }
    // Cancellation that raced the final chunk still must not report success.
    if control.is_cancelled() {
        api.finish("cancelled", &[]);
        return Ok(());
    }
    finish_openai_stream(tool_stream, finish_reason, &mut api, tx)
}

fn finish_openai_stream(
    tool_stream: crate::chat_tool_stream::ChatToolStream,
    finish_reason: Option<pig_protocol::FinishReason>,
    api: &mut crate::api_log::ApiCall,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
) -> Result<(), CallError> {
    let Some(reason) = finish_reason else {
        let detail = "Chat Completions stream ended without a finish reason".to_string();
        api.fail(&detail);
        return Err(CoreError::Internal { detail }.into());
    };
    let calls = match tool_stream.finish(tx) {
        Ok(calls) => calls,
        Err(error) => {
            api.fail(&crate::core_error_en(&error));
            return Err(error.into());
        }
    };
    api.finish(reason.as_str(), &calls);
    crate::chat::finish(tx, calls, reason);
    Ok(())
}

#[derive(Deserialize)]
pub(crate) struct OpenAiChunk {
    id: Option<String>,
    choices: Option<Vec<OpenAiChoice>>,
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
pub(crate) struct OpenAiChoice {
    delta: Option<OpenAiDelta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct OpenAiDelta {
    /// String (the normal case) OR a parts array (z.ai/GLM-style; see
    /// content_parts) — hence the untyped Value
    content: Option<serde_json::Value>,
    /// DeepSeek-style reasoning field
    reasoning_content: Option<String>,
    /// gpt-oss reasoning field variant (the SDK accepts both)
    reasoning: Option<String>,
    tool_calls: Option<Vec<OpenAiToolCallChunk>>,
}

/// One flattened piece of a delta content payload, in wire order
enum DeltaPiece {
    Text(String),
    Reasoning(String),
}

/// delta.content leniency (aligned with the AI SDK's openai-compatible
/// convertOpenAICompatibleContent): a plain string, or a parts array where
/// `{"type":"text","text":...}` is text and `{"type":"thinking","thinking":
/// ...}` (string or a nested [{type:"text",text}] array) flattens to reasoning
fn content_parts(content: &serde_json::Value) -> Vec<DeltaPiece> {
    match content {
        serde_json::Value::String(text) => vec![DeltaPiece::Text(text.clone())],
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part["type"].as_str() {
                Some("text") => part["text"]
                    .as_str()
                    .map(|text| DeltaPiece::Text(text.to_string())),
                Some("thinking") => match &part["thinking"] {
                    serde_json::Value::String(thinking) => {
                        Some(DeltaPiece::Reasoning(thinking.clone()))
                    }
                    serde_json::Value::Array(blocks) => {
                        let thinking: String = blocks
                            .iter()
                            .filter_map(|block| block["text"].as_str())
                            .collect();
                        Some(DeltaPiece::Reasoning(thinking))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

#[derive(Deserialize)]
pub(crate) struct OpenAiToolCallChunk {
    index: Option<usize>,
    id: Option<String>,
    function: Option<OpenAiFunctionChunk>,
}

#[derive(Deserialize)]
pub(crate) struct OpenAiFunctionChunk {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct OpenAiUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptDetails>,
    #[serde(default)]
    completion_tokens_details: Option<OpenAiCompletionDetails>,
}

#[derive(Deserialize)]
pub(crate) struct OpenAiPromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Deserialize)]
pub(crate) struct OpenAiCompletionDetails {
    /// Reasoning slice of the output total (already counted in
    /// completion_tokens; informational split, like the SDK's
    /// outputTokens.reasoning)
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

pub(crate) fn usage_from_json(
    usage: &serde_json::Value,
    context_window: u64,
) -> Option<crate::ProviderUsage> {
    normalize_usage(
        usage["prompt_tokens"].as_u64().unwrap_or(0),
        usage["completion_tokens"].as_u64().unwrap_or(0),
        usage["total_tokens"].as_u64(),
        usage["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0),
        usage["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap_or(0),
        context_window,
    )
}

fn normalize_usage(
    prompt: u64,
    output: u64,
    total: Option<u64>,
    cache_read: u64,
    reasoning_output: u64,
    context_window: u64,
) -> Option<crate::ProviderUsage> {
    let input = prompt.saturating_sub(cache_read);
    let used = total.unwrap_or(input + cache_read + output);
    (used > 0).then_some(crate::ProviderUsage {
        input,
        cache_read,
        output,
        used,
        total: context_window,
        reasoning_output,
    })
}

/// Business error transported inside an HTTP-200 SSE stream (OpenAI-compatible
/// gateways do this instead of failing the status): `{"error": {...}}` /
/// `{"error": "..."}` (OpenAI's own error shape) or `{"success": false,
/// "message"/"msg": ...}` (Zhipu-style business envelope). A legit chat chunk
/// never carries a top-level `error`/`success`, so these checks cannot
/// misfire. Returns the detail to surface.
pub(crate) fn business_error_detail(value: &serde_json::Value) -> Option<String> {
    let with_code = |message: &str, code: Option<String>| match code {
        Some(code) if !message.contains(&code) => format!("{message} (code {code})"),
        _ => message.to_string(),
    };
    let code_of = |value: &serde_json::Value| {
        value["code"]
            .as_str()
            .map(str::to_string)
            .or_else(|| value["code"].as_i64().map(|code| code.to_string()))
    };
    if let Some(error) = value.get("error") {
        return Some(match error {
            serde_json::Value::String(message) => message.clone(),
            other => with_code(
                other["message"].as_str().unwrap_or("unknown error"),
                code_of(other),
            ),
        });
    }
    if value.get("success").and_then(|v| v.as_bool()) == Some(false) {
        let message = value["message"]
            .as_str()
            .or_else(|| value["msg"].as_str())
            .unwrap_or("business error");
        return Some(with_code(message, code_of(value)));
    }
    None
}

#[cfg(test)]
mod tests {

    use super::{DeltaPiece, business_error_detail, content_parts, to_openai_messages};
    use crate::chat::{ChatImage, ChatMsg};

    #[test]
    fn business_error_shapes() {
        // OpenAI's own error shape (object and bare string)
        assert_eq!(
            business_error_detail(
                &serde_json::json!({"error": {"message": "rate limited", "code": "1302"}})
            ),
            Some("rate limited (code 1302)".to_string())
        );
        assert_eq!(
            business_error_detail(&serde_json::json!({"error": {"message": "boom"}})),
            Some("boom".to_string())
        );
        assert_eq!(
            business_error_detail(&serde_json::json!({"error": "plain failure"})),
            Some("plain failure".to_string())
        );
        // Zhipu-style business envelope
        assert_eq!(
            business_error_detail(
                &serde_json::json!({"success": false, "code": 1005, "message": "余额不足"})
            ),
            Some("余额不足 (code 1005)".to_string())
        );
        assert_eq!(
            business_error_detail(
                &serde_json::json!({"success": false, "code": 1261, "message": "Prompt exceeds max length"})
            ),
            Some("Prompt exceeds max length (code 1261)".to_string())
        );
        // Legit chunks pass through (choices/usage shapes carry no error/success)
        assert_eq!(
            business_error_detail(&serde_json::json!({"choices": [{"delta": {"content": "hi"}}]})),
            None
        );
        assert_eq!(
            business_error_detail(&serde_json::json!({"usage": {"total_tokens": 3}})),
            None
        );
    }

    /// delta.content: plain string / z.ai-style parts array with nested
    /// thinking blocks / null
    #[test]
    fn delta_content_string_or_parts() {
        let parts = content_parts(&serde_json::json!("hello"));
        assert!(matches!(&parts[..], [DeltaPiece::Text(t)] if t == "hello"));

        // z.ai/GLM shape: thinking as a nested block array, then text
        let parts = content_parts(&serde_json::json!([
            {"type": "thinking", "thinking": [{"type": "text", "text": "think "}, {"type": "text", "text": "more"}]},
            {"type": "text", "text": "answer"}
        ]));
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], DeltaPiece::Reasoning(r) if r == "think more"));
        assert!(matches!(&parts[1], DeltaPiece::Text(t) if t == "answer"));

        // thinking as a bare string
        let parts = content_parts(&serde_json::json!([{"type": "thinking", "thinking": "plain"}]));
        assert!(matches!(&parts[..], [DeltaPiece::Reasoning(r)] if r == "plain"));

        // null / unknown part types produce nothing
        assert!(content_parts(&serde_json::Value::Null).is_empty());
        assert!(
            content_parts(&serde_json::json!([{"type": "image_url", "image_url": {}}])).is_empty()
        );
    }

    #[test]
    fn official_chat_body_uses_max_completion_tokens_and_effort_only() {
        let mut config = crate::chat::test_model(pig_protocol::ApiFormat::OpenAiChat);
        config.base_url = "https://api.openai.com/v1".into();
        config.reasoning_params = Some(serde_json::json!({
            "reasoning_effort": "high",
            "thinking": {"type": "enabled"},
            "enable_thinking": true,
            "reasoning": {"effort": "high"},
        }));
        config.prompt_cache_key = Some("pig:session-1".into());
        let body = super::build_chat_body(&config, &[ChatMsg::user("hi".into())], &[], true)
            .expect("body");
        assert_eq!(body["max_completion_tokens"], 4096);
        assert!(body.get("max_tokens").is_none(), "{body}");
        assert_eq!(body["reasoning_effort"], "high");
        for key in ["thinking", "enable_thinking", "reasoning"] {
            assert!(
                body.get(key).is_none(),
                "official Chat must drop {key}: {body}"
            );
        }
        assert_eq!(body["prompt_cache_key"], "pig:session-1");
    }

    #[test]
    fn compatible_chat_body_keeps_max_tokens_and_the_full_bundle() {
        let mut config = crate::chat::test_model(pig_protocol::ApiFormat::OpenAiChat);
        config.base_url = "https://api.deepseek.com".into();
        config.reasoning_params = Some(serde_json::json!({
            "reasoning_effort": "high",
            "thinking": {"type": "enabled"},
            "enable_thinking": true,
            "reasoning": {"effort": "high"},
        }));
        let body = super::build_chat_body(&config, &[ChatMsg::user("hi".into())], &[], false)
            .expect("body");
        assert_eq!(body["max_tokens"], 4096);
        assert!(body.get("max_completion_tokens").is_none(), "{body}");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["enable_thinking"], true);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert!(
            body.get("prompt_cache_key").is_none(),
            "no configured key stays absent"
        );
    }

    #[test]
    fn media_only_user_turns_have_no_empty_text_part() {
        let mut msg = ChatMsg::user(String::new());
        msg.images = vec![ChatImage {
            media_type: "image/png".into(),
            data_base64: "QUJD".into(),
            label: None,
        }];
        msg.documents = vec![crate::chat::ChatDocument {
            media_type: "application/pdf".into(),
            data_base64: "JVBERi0=".into(),
            filename: "only.pdf".into(),
            label: None,
            token_estimate: None,
        }];
        let messages = to_openai_messages(&[msg]);
        let parts = messages[0]["content"].as_array().expect("media parts");
        assert_eq!(
            parts
                .iter()
                .map(|part| part["type"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["image_url", "file"],
            "no empty text part: {parts:?}"
        );
    }

    #[test]
    fn user_images_get_label_markers() {
        let mut msg = ChatMsg::user("look".into());
        msg.images = vec![
            ChatImage {
                media_type: "image/png".into(),
                data_base64: "QUJD".into(),
                label: Some("Image 1".into()),
            },
            ChatImage {
                media_type: "image/png".into(),
                data_base64: "REVG".into(),
                label: None,
            },
        ];
        let messages = to_openai_messages(&[msg]);
        let parts = &messages[0]["content"];
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "look");
        // Labeled image: "[Image 1]" marker part before its image part;
        // unlabeled: no marker
        assert_eq!(parts[1]["type"], "text");
        assert_eq!(parts[1]["text"], "[Image 1]");
        assert_eq!(parts[2]["type"], "image_url");
        assert_eq!(parts[3]["type"], "image_url");
    }
}
