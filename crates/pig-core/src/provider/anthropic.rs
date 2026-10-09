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
) -> Result<(), CallError> {
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

    let url = anthropic_url(&config.base_url);
    // The interleaved-thinking beta header rides along when reasoning params enabled
    // thinking for this request (same condition the SDK's client would send it under;
    // an explicit thinking.type=disabled must not trigger it)
    let thinking =
        body.get("thinking").and_then(|t| t.get("type")) == Some(&serde_json::json!("enabled"));
    // API wire log (PIG_LOG_API): no-op accumulator when disabled
    let mut api =
        crate::api_log::ApiCall::new("anthropic.messages", &config.provider_name, &config.model);
    api.request(&url, &body);

    let client = http_client();
    let response = match send_with_retry(
        |retry| {
            apply_sdk_headers(
                client
                    .post(&url)
                    .header("x-api-key", &config.api_key)
                    .header("anthropic-version", "2023-06-01"),
                true,
                thinking,
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
    let mut total_input = 0u64;
    // Anthropic: input_tokens excludes the cached portion; cache_creation is newly written this time (a cache miss)
    let mut total_cache_read = 0u64;
    let mut total_output = 0u64;

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
            // Raw wire log: the payload verbatim, plus the event: line when
            // present (dispatch below keys off the payload's own type, same
            // as the SDK — the event: field is redundant in Anthropic frames)
            if event.event.is_empty() {
                api.raw_line(&event.data);
            } else {
                api.raw_line(&format!("event: {}\ndata: {}", event.event, event.data));
            }
            let Ok(json) = serde_json::from_str::<serde_json::Value>(event.data.trim()) else {
                continue;
            };
            match json["type"].as_str().unwrap_or("") {
                "message_start" => {
                    merge_usage(
                        &mut total_input,
                        &mut total_cache_read,
                        &mut total_output,
                        &json["message"]["usage"],
                    );
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
                                api.text(text);
                                let _ = tx.send(ProviderEvent::Text(text.to_string()));
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(thinking) = delta["thinking"].as_str()
                                && !thinking.is_empty()
                            {
                                api.reasoning(thinking);
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
                    merge_usage(
                        &mut total_input,
                        &mut total_cache_read,
                        &mut total_output,
                        &json["usage"],
                    );
                    if json["delta"]["stop_reason"].is_string() {
                        if total_input + total_cache_read + total_output > 0 {
                            // Thinking slice of the output total (informational
                            // split, like the SDK's outputTokens.reasoning)
                            let thinking =
                                json["usage"]["output_tokens_details"]["thinking_tokens"]
                                    .as_u64()
                                    .unwrap_or(0);
                            api.usage(
                                total_input,
                                total_cache_read,
                                total_output,
                                total_input + total_cache_read + total_output,
                                config.context_window,
                                thinking,
                            );
                            let _ = tx.send(ProviderEvent::Usage {
                                input: total_input,
                                cache_read: total_cache_read,
                                output: total_output,
                                used: total_input + total_cache_read + total_output,
                                total: config.context_window,
                                reasoning_output: thinking,
                            });
                        }
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
                "error" => {
                    // In-band 200 error. The upstream error body text goes
                    // into detail verbatim (without a message field, fall back
                    // semantically to Unknown). Retryability follows the SDK's
                    // getAnthropicStreamErrorMetadata mapping (overloaded→529,
                    // rate_limit→429, api_error→500, all retryable); the silent
                    // retry itself is gated on "no content forwarded yet" in
                    // stream_chat
                    let error_type = json["error"]["type"].as_str().unwrap_or("");
                    let retryable = matches!(
                        error_type,
                        "overloaded_error" | "rate_limit_error" | "api_error"
                    );
                    let message = json["error"]["message"].as_str().map(str::to_string);
                    api.fail(
                        message
                            .as_deref()
                            .map(|m| format!("Anthropic error: {m}"))
                            .as_deref()
                            .unwrap_or("Anthropic error: unknown"),
                    );
                    return Err(CallError {
                        error: match message {
                            Some(message) => CoreError::Internal {
                                detail: format!("Anthropic error: {message}"),
                            },
                            None => CoreError::Unknown,
                        },
                        retryable,
                    });
                }
                _ => {}
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

/// Merge one usage frame into the running totals (frame input = input_tokens
/// plus cache_creation_input_tokens). Per the Anthropic spec, input/cache
/// arrive in message_start and output in message_delta; compatible providers
/// (GLM's Anthropic endpoint, observed 2026-10-09) instead send zeros in
/// message_start and the real input numbers in message_delta. Every field
/// merges with max — token counts never shrink within one message — so
/// whichever frame carries the real numbers wins.
fn merge_usage(
    total_input: &mut u64,
    total_cache_read: &mut u64,
    total_output: &mut u64,
    usage: &serde_json::Value,
) {
    let frame_input = usage["input_tokens"].as_u64().unwrap_or(0)
        + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    *total_input = (*total_input).max(frame_input);
    *total_cache_read =
        (*total_cache_read).max(usage["cache_read_input_tokens"].as_u64().unwrap_or(0));
    *total_output = (*total_output).max(usage["output_tokens"].as_u64().unwrap_or(0));
}

#[cfg(test)]
mod tests {
    use super::merge_usage;

    /// Spec-compliant Anthropic: input/cache in message_start, output in
    /// message_delta (which carries no input fields)
    #[test]
    fn usage_start_then_delta() {
        let (mut input, mut cache_read, mut output) = (0, 0, 0);
        merge_usage(
            &mut input,
            &mut cache_read,
            &mut output,
            &serde_json::json!({"input_tokens": 100, "cache_creation_input_tokens": 20, "cache_read_input_tokens": 30, "output_tokens": 1}),
        );
        assert_eq!((input, cache_read, output), (120, 30, 1));
        merge_usage(
            &mut input,
            &mut cache_read,
            &mut output,
            &serde_json::json!({"output_tokens": 55}),
        );
        assert_eq!((input, cache_read, output), (120, 30, 55));
    }

    /// GLM's Anthropic endpoint (observed 2026-10-09): message_start carries
    /// zeros, the real input/cache numbers arrive in message_delta
    #[test]
    fn usage_glm_real_numbers_in_delta() {
        let (mut input, mut cache_read, mut output) = (0, 0, 0);
        merge_usage(
            &mut input,
            &mut cache_read,
            &mut output,
            &serde_json::json!({"input_tokens": 0, "output_tokens": 0}),
        );
        assert_eq!((input, cache_read, output), (0, 0, 0));
        merge_usage(
            &mut input,
            &mut cache_read,
            &mut output,
            &serde_json::json!({"input_tokens": 9697, "output_tokens": 112, "cache_read_input_tokens": 0}),
        );
        assert_eq!((input, cache_read, output), (9697, 0, 112));
    }
}

// ---------------- One-shot requests (compact summary / connectivity test) ----------------
