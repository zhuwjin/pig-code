use super::*;

/// Internal ChatMsg list -> OpenAI messages array.
/// The only difference from direct serde serialization: OpenAI tool-role messages cannot carry images -- an image-carrying tool result
/// (ReadMediaFile) splits into two: the tool message keeps only the text output, images move to the immediately following user
/// message (content parts: text label + image_url data URL).
pub(crate) fn to_openai_messages(messages: &[ChatMsg]) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.images.is_empty() {
            out.push(serde_json::to_value(msg).unwrap_or_default());
            continue;
        }
        match msg.role.as_str() {
            // The tool role cannot carry images: split into tool text + an immediately following user image message
            "tool" => {
                let mut text_only = msg.clone();
                text_only.images = vec![];
                out.push(serde_json::to_value(&text_only).unwrap_or_default());
                let mut parts = Vec::with_capacity(msg.images.len() * 2);
                for img in &msg.images {
                    let label = img.label.as_deref().unwrap_or("image");
                    parts.push(serde_json::json!({
                        "type": "text",
                        "text": format!("[ReadMediaFile output image: {label}]"),
                    }));
                    parts.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", img.media_type, img.data_base64),
                        },
                    }));
                }
                out.push(serde_json::json!({"role": "user", "content": parts}));
            }
            // User message with images (sent by pasting): content becomes a parts array (text first, images after)
            "user" => {
                let mut parts = Vec::with_capacity(msg.images.len() + 1);
                parts.push(serde_json::json!({
                    "type": "text",
                    "text": msg.content.clone().unwrap_or_default(),
                }));
                for img in &msg.images {
                    parts.push(serde_json::json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", img.media_type, img.data_base64),
                        },
                    }));
                }
                out.push(serde_json::json!({"role": "user", "content": parts}));
            }
            _ => out.push(serde_json::to_value(msg).unwrap_or_default()),
        }
    }
    out
}

#[derive(Serialize)]
pub(crate) struct StreamOptions {
    include_usage: bool,
}

/// Request-level tool list assembly (shared by streaming/non-streaming, to prevent drift between the two):
/// OpenAI wire shape verbatim + the server-side search tool when explicitly configured.
pub(crate) fn openai_request_tools(
    config: &ResolvedModel,
    tools: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut out = tools.to_vec();
    if let Some(tool) = openai_web_search_tool(config) {
        out.push(tool);
    }
    out
}

pub(crate) async fn stream_openai(
    config: &ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), CallError> {
    let client = http_client();
    let tools = openai_request_tools(config, &tools);
    let messages_json = to_openai_messages(&messages);
    let mut body = serde_json::to_value(ChatRequest {
        model: &config.model,
        messages: &messages_json,
        tools: (!tools.is_empty()).then_some(tools.as_slice()),
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        max_tokens: config.max_output_tokens,
    })
    .map_err(|e| CoreError::Internal {
        detail: e.to_string(),
    })?;
    merge_reasoning_params(&mut body, config);

    let url = format!("{}/chat/completions", config.base_url);
    // API wire log (PIG_LOG_API): no-op accumulator when disabled
    let mut api = crate::api_log::ApiCall::new("openai.chat", &config.provider_name, &config.model);
    api.request(&url, &body);

    let response = match send_with_retry(
        |retry| {
            apply_sdk_headers(
                client.post(&url).bearer_auth(&config.api_key),
                false,
                false,
                retry,
            )
            .json(&body)
        },
        cancel,
    )
    .await?
    {
        SendOutcome::Response(response) => response,
        SendOutcome::Cancelled => {
            api.finish("cancelled", &[]);
            return Ok(());
        }
    };

    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(500).collect();
        api.fail(&format!("HTTP {status}: {detail}"));
        return Err(CoreError::Internal {
            detail: format!("HTTP {status}: {detail}"),
        }
        .into());
    }

    let mut byte_stream = response.bytes_stream();
    // Spec-level SSE decoding (three line terminators, deferred trailing CR,
    // multi-line data, comments, incremental UTF-8) — same semantics as the
    // eventsource-parser package under the Vercel AI SDK
    let mut sse = crate::sse::SseDecoder::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    loop {
        let chunk = tokio::select! {
            chunk = byte_stream.next() => chunk,
            _ = cancel.cancelled() => {
                api.finish("cancelled", &tool_calls);
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
            // The [DONE] sentinel is matched here (not in the SSE layer), same
            // as the SDK's transform stage
            if data == "[DONE]" {
                let outcome = if tool_calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                };
                api.finish(outcome, &tool_calls);
                finish(tx, &mut tool_calls);
                return Ok(());
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            // Business error inside an HTTP-200 stream (OpenAI-compatible
            // gateways do this instead of a non-2xx status; same class as
            // ZCode's ProviderBusinessError mid-stream detection)
            if let Some(detail) = business_error_detail(&value) {
                api.fail(&detail);
                return Err(CoreError::Internal { detail }.into());
            }
            let Ok(chunk) = serde_json::from_value::<OpenAiChunk>(value) else {
                continue;
            };
            if let Some(usage) = chunk.usage {
                let prompt = usage.prompt_tokens.unwrap_or(0);
                let cache_read = usage
                    .prompt_tokens_details
                    .and_then(|d| d.cached_tokens)
                    .unwrap_or(0);
                let input = prompt.saturating_sub(cache_read);
                let output = usage.completion_tokens.unwrap_or(0);
                let used = usage.total_tokens.unwrap_or(input + cache_read + output);
                if used > 0 {
                    let reasoning_output = usage
                        .completion_tokens_details
                        .and_then(|d| d.reasoning_tokens)
                        .unwrap_or(0);
                    api.usage(
                        input,
                        cache_read,
                        output,
                        used,
                        config.context_window,
                        reasoning_output,
                    );
                    let _ = tx.send(ProviderEvent::Usage {
                        input,
                        cache_read,
                        output,
                        used,
                        total: config.context_window,
                        reasoning_output,
                    });
                }
            }
            let Some(choice) = chunk.choices.and_then(|mut c| c.pop()) else {
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
                        let index = part.index.unwrap_or(0);
                        while tool_calls.len() <= index {
                            tool_calls.push(ToolCall::default());
                        }
                        let call = &mut tool_calls[index];
                        if let Some(id) = part.id {
                            call.id.push_str(&id);
                        }
                        if let Some(function) = part.function {
                            if let Some(name) = function.name {
                                call.name.push_str(&name);
                            }
                            if let Some(arguments) = function.arguments {
                                call.arguments.push_str(&arguments);
                            }
                        }
                    }
                }
            }
            if matches!(
                choice.finish_reason.as_deref(),
                Some("stop") | Some("tool_calls")
            ) {
                let outcome = if tool_calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                };
                api.finish(outcome, &tool_calls);
                finish(tx, &mut tool_calls);
                return Ok(());
            }
        }
    }
    let outcome = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    api.finish(outcome, &tool_calls);
    finish(tx, &mut tool_calls);
    Ok(())
}

#[derive(Deserialize)]
pub(crate) struct OpenAiChunk {
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

/// OpenAI-compatible endpoints: no server-side search standard; inject only when web_search_tool is explicitly configured
/// (e.g. Zhipu {"type":"web_search","web_search":{"enable":true,"search_result":true}}).
pub fn openai_web_search_tool(config: &ResolvedModel) -> Option<serde_json::Value> {
    if !config.cap_web_search {
        return None;
    }
    config.web_search_tool.clone()
}

/// Business error transported inside an HTTP-200 SSE stream (OpenAI-compatible
/// gateways do this instead of failing the status): `{"error": {...}}` /
/// `{"error": "..."}` (OpenAI's own error shape) or `{"success": false,
/// "message"/"msg": ...}` (Zhipu-style business envelope). A legit chat chunk
/// never carries a top-level `error`/`success`, so these checks cannot
/// misfire. Returns the detail to surface.
fn business_error_detail(value: &serde_json::Value) -> Option<String> {
    if let Some(error) = value.get("error") {
        return Some(match error {
            serde_json::Value::String(message) => message.clone(),
            other => {
                let message = other["message"].as_str().unwrap_or("unknown error");
                match other["code"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| other["code"].as_i64().map(|code| code.to_string()))
                {
                    Some(code) => format!("{message} (code {code})"),
                    None => message.to_string(),
                }
            }
        });
    }
    if value.get("success").and_then(|v| v.as_bool()) == Some(false) {
        let message = value["message"]
            .as_str()
            .or_else(|| value["msg"].as_str())
            .unwrap_or("business error");
        return Some(message.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{DeltaPiece, business_error_detail, content_parts};

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
            Some("余额不足".to_string())
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
}
