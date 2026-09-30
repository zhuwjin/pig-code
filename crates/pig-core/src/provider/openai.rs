use super::*;

/// 内部 ChatMsg 列表 → OpenAI messages 数组。
/// 与 serde 直序的唯一差异：OpenAI 的 tool 角色消息不能带图——带图的工具结果
///（ReadMediaFile）拆成两条：tool 消息只留文本 output，图片拆到紧随的 user
/// 消息（content parts：text 标注 + image_url data URL）。
pub(crate) fn to_openai_messages(messages: &[ChatMsg]) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.images.is_empty() {
            out.push(serde_json::to_value(msg).unwrap_or_default());
            continue;
        }
        match msg.role.as_str() {
            // tool 角色不能带图：拆成 tool 文本 + 紧随的 user 图片消息
            "tool" => {
                let mut text_only = msg.clone();
                text_only.images = vec![];
                out.push(serde_json::to_value(&text_only).unwrap_or_default());
                let mut parts = Vec::with_capacity(msg.images.len() * 2);
                for img in &msg.images {
                    let label = img.label.as_deref().unwrap_or("图片");
                    parts.push(serde_json::json!({
                        "type": "text",
                        "text": format!("[ReadMediaFile 输出图片: {label}]"),
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
            // 用户消息带图（粘贴发送）：content 改 parts 数组（文本在前、图在后）
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

pub(crate) async fn stream_openai(
    config: &ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    let client = http_client();
    let mut tools = tools;
    if let Some(tool) = openai_web_search_tool(config) {
        tools.push(tool);
    }
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
    .map_err(|e| e.to_string())?;
    merge_reasoning_params(&mut body, config);

    let response = match send_with_retry(
        || {
            client
                .post(format!("{}/chat/completions", config.base_url))
                .bearer_auth(&config.api_key)
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
    let mut tool_calls: Vec<ToolCall> = Vec::new();

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
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                finish(tx, &mut tool_calls);
                return Ok(());
            }
            let Ok(chunk) = serde_json::from_str::<OpenAiChunk>(data) else {
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
                    let _ = tx.send(ProviderEvent::Usage {
                        input,
                        cache_read,
                        output,
                        used,
                        total: config.context_window,
                    });
                }
            }
            let Some(choice) = chunk.choices.and_then(|mut c| c.pop()) else {
                continue;
            };
            if let Some(delta) = choice.delta {
                if let Some(reasoning) = delta.reasoning_content
                    && !reasoning.is_empty()
                {
                    let _ = tx.send(ProviderEvent::Reasoning(reasoning));
                }
                if let Some(text) = delta.content
                    && !text.is_empty()
                {
                    let _ = tx.send(ProviderEvent::Text(text));
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
                finish(tx, &mut tool_calls);
                return Ok(());
            }
        }
    }
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
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<OpenAiToolCallChunk>>,
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
}

#[derive(Deserialize)]
pub(crate) struct OpenAiPromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

/// OpenAI 兼容端点：无服务端搜索标准，只有显式配置 web_search_tool 才注入
///（如智谱 {"type":"web_search","web_search":{"enable":true,"search_result":true}}）。
pub fn openai_web_search_tool(config: &ResolvedModel) -> Option<serde_json::Value> {
    if !config.cap_web_search {
        return None;
    }
    config.web_search_tool.clone()
}
