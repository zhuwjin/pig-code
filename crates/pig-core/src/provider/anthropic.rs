use super::*;

pub(crate) fn anthropic_url(base: &str) -> String {
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// 内部 ChatMsg 列表 → Anthropic messages 数组。
/// system 抽顶层；assistant tool_calls → tool_use block；tool 结果并入 user 消息的 tool_result block。
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
                    // 用户消息带图（粘贴发送）：图片块在前、文本在后
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
                // thinking 模式下 Anthropic 兼容端点（DeepSeek/Kimi）要求回传思考块，
                // 且 thinking 必须在 text/tool_use 之前；这类端点不发 signature，
                // 按非 Claude 端点惯例省略 signature 字段（参考 kimi-code）
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
                // 带图工具结果（ReadMediaFile）：content 从字符串改为 blocks 数组
                //（图片块在前、文本摘要在后）；无图保持字符串原样（回归安全）
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
                // 连续 tool 结果并入同一条 user 消息
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

/// 请求级工具清单组装（流式/非流式共用，防两处漂移）：
/// OpenAI 线格式 → Anthropic 形态 + 能力开启时的服务端搜索工具。
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

/// Anthropic 端点：能力开启时注入服务端搜索工具（web_search_tool 可自定义，
/// 缺省 web_search_20250305）。服务端产出的 web_search_tool_result block 由
/// SSE 解析器 fallthrough 忽略。
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
) -> Result<(), String> {
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
        return Err(format!("HTTP {status}: {detail}"));
    }

    let mut byte_stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut event_type = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut total_input = 0u64;
    // Anthropic：input_tokens 不含缓存部分；cache_creation 是本次新写入（未命中）
    let mut total_cache_read = 0u64;
    #[allow(unused_assignments)]
    let mut total_output = 0u64;

    loop {
        let chunk = tokio::select! {
            chunk = byte_stream.next() => chunk,
            _ = cancel.cancelled() => return Ok(()),
        };
        let Some(chunk) = chunk else { break };
        let bytes = chunk.map_err(|e| format!("读取流失败: {e}"))?;
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
                    let message = json["error"]["message"].as_str().unwrap_or("未知错误");
                    return Err(format!("Anthropic error: {message}"));
                }
                _ => {}
            }
        }
    }
    finish(tx, &mut tool_calls);
    Ok(())
}

// ---------------- 一次性请求（compact 摘要 / 连通性测试） ----------------
