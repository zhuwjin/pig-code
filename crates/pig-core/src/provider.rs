use futures_util::StreamExt as _;
use pig_protocol::ApiFormat;
use serde::{Deserialize, Serialize};

/// 解析后的模型端点配置（provider + model + 推理等级合并产物）
#[derive(Clone, Debug)]
pub struct ResolvedModel {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub api_format: ApiFormat,
    pub reasoning_params: Option<serde_json::Value>,
    /// 展示用
    pub provider_name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChatMsg {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallWire>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMsg {
    pub fn system(content: String) -> Self {
        Self {
            role: "system".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: String) -> Self {
        Self {
            role: "user".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: String, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: (!content.is_empty()).then_some(content),
            tool_calls: (!tool_calls.is_empty())
                .then(|| tool_calls.iter().map(ToolCall::to_wire).collect()),
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: &str, output: String) -> Self {
        Self {
            role: "tool".into(),
            content: Some(output),
            tool_calls: None,
            tool_call_id: Some(call_id.to_string()),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolCallWire {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionWire,
}

#[derive(Clone, Debug, Serialize)]
pub struct FunctionWire {
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    pub fn to_wire(&self) -> ToolCallWire {
        ToolCallWire {
            id: self.id.clone(),
            kind: "function",
            function: FunctionWire {
                name: self.name.clone(),
                arguments: self.arguments.clone(),
            },
        }
    }
}

/// 流式过程中 provider → session 的内部事件
#[derive(Debug)]
pub enum ProviderEvent {
    Reasoning(String),
    Text(String),
    ToolCalls(Vec<ToolCall>),
    Usage { used: u64, total: u64 },
    Finished,
    Failed(String),
}

pub async fn stream_chat(
    config: ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let result = match config.api_format {
        ApiFormat::OpenAiChat => stream_openai(&config, messages, tools, &tx, &cancel).await,
        ApiFormat::AnthropicMessages => stream_anthropic(&config, messages, tools, &tx, &cancel).await,
    };
    if let Err(error) = result {
        let _ = tx.send(ProviderEvent::Failed(error));
    }
}

// ---------------- OpenAI Chat Completions ----------------

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMsg],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [serde_json::Value]>,
    stream: bool,
    stream_options: StreamOptions,
    max_tokens: u64,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

async fn stream_openai(
    config: &ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let mut body = serde_json::to_value(ChatRequest {
        model: &config.model,
        messages: &messages,
        tools: (!tools.is_empty()).then_some(tools.as_slice()),
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        max_tokens: config.max_output_tokens,
    })
    .map_err(|e| e.to_string())?;
    merge_reasoning_params(&mut body, config);

    let response = tokio::select! {
        result = client
            .post(format!("{}/chat/completions", config.base_url))
            .bearer_auth(&config.api_key)
            .json(&body)
            .send() => result.map_err(|e| format!("网络错误: {e}"))?,
        _ = cancel.cancelled() => return Ok(()),
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
                if let Some(total) = usage.total_tokens {
                    let _ = tx.send(ProviderEvent::Usage {
                        used: total,
                        total: config.context_window,
                    });
                }
            }
            let Some(choice) = chunk.choices.and_then(|mut c| c.pop()) else {
                continue;
            };
            if let Some(delta) = choice.delta {
                if let Some(reasoning) = delta.reasoning_content {
                    if !reasoning.is_empty() {
                        let _ = tx.send(ProviderEvent::Reasoning(reasoning));
                    }
                }
                if let Some(text) = delta.content {
                    if !text.is_empty() {
                        let _ = tx.send(ProviderEvent::Text(text));
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
            if matches!(choice.finish_reason.as_deref(), Some("stop") | Some("tool_calls")) {
                finish(tx, &mut tool_calls);
                return Ok(());
            }
        }
    }
    finish(tx, &mut tool_calls);
    Ok(())
}

#[derive(Deserialize)]
struct OpenAiChunk {
    choices: Option<Vec<OpenAiChoice>>,
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    delta: Option<OpenAiDelta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<OpenAiToolCallChunk>>,
}

#[derive(Deserialize)]
struct OpenAiToolCallChunk {
    index: Option<usize>,
    id: Option<String>,
    function: Option<OpenAiFunctionChunk>,
}

#[derive(Deserialize)]
struct OpenAiFunctionChunk {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    total_tokens: Option<u64>,
}

fn finish(
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    tool_calls: &mut Vec<ToolCall>,
) {
    let calls = std::mem::take(tool_calls);
    if !calls.is_empty() {
        let _ = tx.send(ProviderEvent::ToolCalls(calls));
    }
    let _ = tx.send(ProviderEvent::Finished);
}

fn merge_reasoning_params(body: &mut serde_json::Value, config: &ResolvedModel) {
    if let (serde_json::Value::Object(map), Some(params)) = (body, &config.reasoning_params) {
        if let serde_json::Value::Object(extra) = params {
            for (key, value) in extra {
                map.insert(key.clone(), value.clone());
            }
        }
    }
}

// ---------------- Anthropic Messages ----------------

fn anthropic_url(base: &str) -> String {
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// 内部 ChatMsg 列表 → Anthropic messages 数组。
/// system 抽顶层；assistant tool_calls → tool_use block；tool 结果并入 user 消息的 tool_result block。
fn to_anthropic_messages(messages: &[ChatMsg]) -> (String, Vec<serde_json::Value>) {
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
            "user" => out.push(serde_json::json!({
                "role": "user",
                "content": msg.content.clone().unwrap_or_default(),
            })),
            "assistant" => {
                let mut blocks: Vec<serde_json::Value> = vec![];
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
                let block = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": msg.tool_call_id.clone().unwrap_or_default(),
                    "content": msg.content.clone().unwrap_or_default(),
                });
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

fn to_anthropic_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let function = &tool["function"];
            Some(serde_json::json!({
                "name": function["name"],
                "description": function["description"],
                "input_schema": function["parameters"],
            }))
        })
        .collect()
}

async fn stream_anthropic(
    config: &ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    let (system, messages) = to_anthropic_messages(&messages);
    let anthropic_tools = to_anthropic_tools(&tools);
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

    let client = reqwest::Client::new();
    let response = tokio::select! {
        result = client
            .post(anthropic_url(&config.base_url))
            .header("x-api-key", &config.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send() => result.map_err(|e| format!("网络错误: {e}"))?,
        _ = cancel.cancelled() => return Ok(()),
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
                    total_input = json["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0);
                }
                "content_block_start" => {
                    let index = json["index"].as_u64().unwrap_or(0) as usize;
                    let block = &json["content_block"];
                    if block["type"] == "tool_use" {
                        while tool_calls.len() <= index {
                            tool_calls.push(ToolCall::default());
                        }
                        tool_calls[index].id =
                            block["id"].as_str().unwrap_or_default().to_string();
                        tool_calls[index].name =
                            block["name"].as_str().unwrap_or_default().to_string();
                    }
                }
                "content_block_delta" => {
                    let delta = &json["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(text) = delta["text"].as_str() {
                                if !text.is_empty() {
                                    let _ = tx.send(ProviderEvent::Text(text.to_string()));
                                }
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(thinking) = delta["thinking"].as_str() {
                                if !thinking.is_empty() {
                                    let _ = tx.send(ProviderEvent::Reasoning(thinking.to_string()));
                                }
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
                        if total_input + total_output > 0 {
                            let _ = tx.send(ProviderEvent::Usage {
                                used: total_input + total_output,
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

#[derive(Deserialize)]
struct CompleteResponse {
    choices: Option<Vec<CompleteChoice>>,
    content: Option<Vec<CompleteBlock>>,
}

#[derive(Deserialize)]
struct CompleteChoice {
    message: Option<CompleteMessage>,
}

#[derive(Deserialize)]
struct CompleteMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct CompleteBlock {
    text: Option<String>,
}

/// 非流式一次性请求（compaction 摘要）。
pub async fn complete_text(
    config: &ResolvedModel,
    user_content: String,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String, String> {
    let client = reqwest::Client::new();
    match config.api_format {
        ApiFormat::OpenAiChat => {
            let body = serde_json::json!({
                "model": config.model,
                "messages": [{"role": "user", "content": user_content}],
                "stream": false,
                "max_tokens": config.max_output_tokens,
            });
            let response = tokio::select! {
                result = client
                    .post(format!("{}/chat/completions", config.base_url))
                    .bearer_auth(&config.api_key)
                    .json(&body)
                    .send() => result.map_err(|e| format!("网络错误: {e}"))?,
                _ = cancel.cancelled() => return Err("已取消".to_string()),
            };
            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                let detail: String = detail.chars().take(300).collect();
                return Err(format!("HTTP {status}: {detail}"));
            }
            let parsed: CompleteResponse = response
                .json()
                .await
                .map_err(|e| format!("解析响应失败: {e}"))?;
            parsed
                .choices
                .and_then(|mut c| c.pop())
                .and_then(|c| c.message)
                .and_then(|m| m.content)
                .filter(|content| !content.is_empty())
                .ok_or_else(|| "响应无内容".to_string())
        }
        ApiFormat::AnthropicMessages => {
            let body = serde_json::json!({
                "model": config.model,
                "max_tokens": config.max_output_tokens,
                "stream": false,
                "messages": [{"role": "user", "content": user_content}],
            });
            let response = tokio::select! {
                result = client
                    .post(anthropic_url(&config.base_url))
                    .header("x-api-key", &config.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send() => result.map_err(|e| format!("网络错误: {e}"))?,
                _ = cancel.cancelled() => return Err("已取消".to_string()),
            };
            let status = response.status();
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                let detail: String = detail.chars().take(300).collect();
                return Err(format!("HTTP {status}: {detail}"));
            }
            let parsed: CompleteResponse = response
                .json()
                .await
                .map_err(|e| format!("解析响应失败: {e}"))?;
            parsed
                .content
                .and_then(|blocks| blocks.into_iter().find_map(|b| b.text))
                .filter(|text| !text.is_empty())
                .ok_or_else(|| "响应无内容".to_string())
        }
    }
}

/// 连通性测试：最小请求，2xx 即通过。
pub async fn test_provider(
    base_url: &str,
    api_key: &str,
    format: ApiFormat,
    model: &str,
) -> Result<String, String> {
    let client = reqwest::Client::new();
    let send = async {
        match format {
            ApiFormat::OpenAiChat => {
                client
                    .post(format!("{}/chat/completions", base_url.trim_end_matches('/')))
                    .bearer_auth(api_key)
                    .json(&serde_json::json!({
                        "model": model,
                        "messages": [{"role": "user", "content": "ping"}],
                        "max_tokens": 1,
                        "stream": false,
                    }))
                    .send()
                    .await
            }
            ApiFormat::AnthropicMessages => {
                client
                    .post(anthropic_url(base_url.trim_end_matches('/')))
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&serde_json::json!({
                        "model": model,
                        "max_tokens": 1,
                        "messages": [{"role": "user", "content": "ping"}],
                        "stream": false,
                    }))
                    .send()
                    .await
            }
        }
    };
    let response = tokio::time::timeout(std::time::Duration::from_secs(10), send)
        .await
        .map_err(|_| "连接超时（10s）".to_string())?
        .map_err(|e| format!("网络错误: {e}"))?;
    let status = response.status();
    if status.is_success() {
        Ok(format!("连接成功（HTTP {status}）"))
    } else {
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(200).collect();
        Err(format!("HTTP {status}: {detail}"))
    }
}
