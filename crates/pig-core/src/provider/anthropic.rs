use super::*;

pub(crate) fn anthropic_url(base: &str) -> String {
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// Internal ChatMsg list -> Anthropic messages array.
/// system is hoisted to the top level; assistant tool_calls -> tool_use block; tool results merge into a user message's tool_result block.
pub(crate) fn to_anthropic_messages(messages: &[ChatMsg]) -> (String, Vec<serde_json::Value>) {
    let mut system = String::new();
    let mut out: Vec<serde_json::Value> = vec![];

    for msg in messages {
        match msg.role.as_str() {
            "system" => {
                if let Some(content) = &msg.content {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(content);
                }
            }
            "user" => {
                if msg.images.is_empty() {
                    out.push(serde_json::json!({
                        "role": "user",
                        "content": msg.content.clone().unwrap_or_default(),
                    }));
                } else {
                    // User message with images (sent by pasting): image blocks first, text after
                    let mut blocks: Vec<serde_json::Value> = msg
                        .images
                        .iter()
                        .map(|img| {
                            serde_json::json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": img.media_type,
                                    "data": img.data_base64,
                                },
                            })
                        })
                        .collect();
                    blocks.push(serde_json::json!({
                        "type": "text",
                        "text": msg.content.clone().unwrap_or_default(),
                    }));
                    out.push(serde_json::json!({"role": "user", "content": blocks}));
                }
            }
            "assistant" => {
                let mut blocks: Vec<serde_json::Value> = vec![];
                // In thinking mode, Anthropic-compatible endpoints (DeepSeek/Kimi) require echoing the thinking block back,
                // and thinking must come before text/tool_use; such endpoints send no signature,
                // so the signature field is omitted per non-Claude-endpoint convention (see kimi-code)
                if let Some(reasoning) = &msg.reasoning
                    && !reasoning.is_empty()
                {
                    blocks.push(serde_json::json!({"type": "thinking", "thinking": reasoning}));
                }
                if let Some(content) = &msg.content {
                    blocks.push(serde_json::json!({"type": "text", "text": content}));
                }
                if let Some(calls) = &msg.tool_calls {
                    for call in calls {
                        let input = serde_json::from_str(&call.function.arguments)
                            .unwrap_or(serde_json::Value::Object(Default::default()));
                        blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.function.name,
                            "input": input,
                        }));
                    }
                }
                out.push(serde_json::json!({"role": "assistant", "content": blocks}));
            }
            "tool" => {
                // Tool result with images (ReadMediaFile): content changes from a string to a blocks array
                // (image blocks first, text summary after); imageless stays a plain string (regression-safe)
                let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
                let block = if msg.images.is_empty() {
                    serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": msg.content.clone().unwrap_or_default(),
                    })
                } else {
                    let mut blocks: Vec<serde_json::Value> = msg
                        .images
                        .iter()
                        .map(|img| {
                            serde_json::json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": img.media_type,
                                    "data": img.data_base64,
                                },
                            })
                        })
                        .collect();
                    blocks.push(serde_json::json!({
                        "type": "text",
                        "text": msg.content.clone().unwrap_or_default(),
                    }));
                    serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": blocks,
                    })
                };
                // Consecutive tool results merge into the same user message
                let merged = if let Some(last) = out.last_mut() {
                    if last["role"] == "user" && last["content"].is_array() {
                        last["content"]
                            .as_array_mut()
                            .expect("array")
                            .push(block.clone());
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !merged {
                    out.push(serde_json::json!({"role": "user", "content": [block]}));
                }
            }
            _ => {}
        }
    }
    (system, out)
}

pub(crate) fn to_anthropic_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            let function = &tool["function"];
            serde_json::json!({
                "name": function["name"],
                "description": function["description"],
                "input_schema": function["parameters"],
            })
        })
        .collect()
}

/// Request-level tool list assembly (shared by streaming/non-streaming, to prevent drift between the two):
/// OpenAI wire shape -> Anthropic shape + the server-side search tool when the capability is on.
pub(crate) fn anthropic_request_tools(
    config: &ResolvedModel,
    tools: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut out = to_anthropic_tools(tools);
    if let Some(tool) = anthropic_web_search_tool(config) {
        out.push(tool);
    }
    out
}

/// Anthropic endpoint: inject the server-side search tool when the capability is on (web_search_tool can be customized,
/// defaulting to web_search_20250305). The server-produced web_search_tool_result block is
/// ignored by the SSE parser's fallthrough.
pub fn anthropic_web_search_tool(config: &ResolvedModel) -> Option<serde_json::Value> {
    if !config.cap_web_search {
        return None;
    }
    Some(config.web_search_tool.clone().unwrap_or_else(
        || serde_json::json!({"type": "web_search_20250305", "name": "web_search"}),
    ))
}

pub(crate) async fn stream_anthropic(
    config: &ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), CoreError> {
    let (system, messages) = to_anthropic_messages(&messages);
    let anthropic_tools = anthropic_request_tools(config, &tools);
    let mut body = serde_json::json!({
        "model": config.model,
        "max_tokens": config.max_output_tokens,
        "stream": true,
        "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = serde_json::Value::String(system);
    }
    if !anthropic_tools.is_empty() {
        body["tools"] = serde_json::Value::Array(anthropic_tools);
    }
    merge_reasoning_params(&mut body, config);

    let client = http_client();
    let response = match send_with_retry(
        || {
            client
                .post(anthropic_url(&config.base_url))
                .header("x-api-key", &config.api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&body)
        },
        cancel,
    )
    .await?
    {
        SendOutcome::Response(response) => response,
        SendOutcome::Cancelled => return Ok(()),
    };

    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(500).collect();
        return Err(CoreError::Internal {
            detail: format!("HTTP {status}: {detail}"),
        });
    }

    let mut byte_stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut event_type = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut total_input = 0u64;
    // Anthropic: input_tokens excludes the cached portion; cache_creation is newly written this time (a cache miss)
    let mut total_cache_read = 0u64;
    #[allow(unused_assignments)]
    let mut total_output = 0u64;

    loop {
        let chunk = tokio::select! {
            chunk = byte_stream.next() => chunk,
            _ = cancel.cancelled() => return Ok(()),
        };
        let Some(chunk) = chunk else { break };
        let bytes = chunk.map_err(|e| CoreError::StreamRead {
            detail: e.to_string(),
        })?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].trim_end_matches('\r').trim().to_string();
            buffer.drain(..=pos);
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            if let Some(event) = line.strip_prefix("event:") {
                event_type = event.trim().to_string();
                continue;
            }
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
                continue;
            };
            match event_type.as_str() {
                "message_start" => {
                    let usage = &json["message"]["usage"];
                    total_input = usage["input_tokens"].as_u64().unwrap_or(0)
                        + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                    total_cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                }
                "content_block_start" => {
                    let index = json["index"].as_u64().unwrap_or(0) as usize;
                    let block = &json["content_block"];
                    if block["type"] == "tool_use" {
                        while tool_calls.len() <= index {
                            tool_calls.push(ToolCall::default());
                        }
                        tool_calls[index].id = block["id"].as_str().unwrap_or_default().to_string();
                        tool_calls[index].name =
                            block["name"].as_str().unwrap_or_default().to_string();
                    }
                }
                "content_block_delta" => {
                    let delta = &json["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(text) = delta["text"].as_str()
                                && !text.is_empty()
                            {
                                let _ = tx.send(ProviderEvent::Text(text.to_string()));
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(thinking) = delta["thinking"].as_str()
                                && !thinking.is_empty()
                            {
                                let _ = tx.send(ProviderEvent::Reasoning(thinking.to_string()));
                            }
                        }
                        Some("input_json_delta") => {
                            let index = json["index"].as_u64().unwrap_or(0) as usize;
                            if let Some(partial) = delta["partial_json"].as_str() {
                                while tool_calls.len() <= index {
                                    tool_calls.push(ToolCall::default());
                                }
                                tool_calls[index].arguments.push_str(partial);
                            }
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    total_output = json["usage"]["output_tokens"].as_u64().unwrap_or(0);
                    if json["delta"]["stop_reason"].is_string() {
                        if total_input + total_cache_read + total_output > 0 {
                            let _ = tx.send(ProviderEvent::Usage {
                                input: total_input,
                                cache_read: total_cache_read,
                                output: total_output,
                                used: total_input + total_cache_read + total_output,
                                total: config.context_window,
                            });
                        }
                        finish(tx, &mut tool_calls);
                        return Ok(());
                    }
                }
                "error" => {
                    // The upstream error body text goes into detail verbatim; without a message field, fall back semantically to Unknown
                    return Err(match json["error"]["message"].as_str() {
                        Some(message) => CoreError::Internal {
                            detail: format!("Anthropic error: {message}"),
                        },
                        None => CoreError::Unknown,
                    });
                }
                _ => {}
            }
        }
    }
    finish(tx, &mut tool_calls);
    Ok(())
}

// ---------------- One-shot requests (compact summary / connectivity test) ----------------
