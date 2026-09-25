use futures_util::StreamExt as _;
use pig_protocol::ApiFormat;
use serde::{Deserialize, Serialize};

/// reqwest 的 Display 只到 "error sending request"，真实原因（DNS/证书/代理/连接拒绝）
/// 在 source 链上，取最底层原因拼出来便于定位。
fn net_err(e: reqwest::Error) -> String {
    let mut root: Option<String> = None;
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        root = Some(s.to_string());
        source = s.source();
    }
    match root {
        Some(root) if root != e.to_string() => format!("网络错误: {e}（{root}）"),
        _ => format!("网络错误: {e}"),
    }
}

/// 流式请求不能设整体超时（响应体长期不结束），只限制建连时间。
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

/// 重试策略：指数退避，最多重试 10 次；1s 起步翻倍、封顶 30s。
/// 只在「响应建立之前」重试（建连/TLS/发送失败、429/5xx）——
/// 响应流一旦建立就不再重试，避免已流出的内容重复输出。
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
    // 429/5xx 带 Retry-After 时优先尊重服务端节奏（封顶 120s）
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
) -> Result<SendOutcome, String> {
    let mut retry = 0;
    loop {
        let result = tokio::select! {
            result = build().send() => result,
            _ = cancel.cancelled() => return Ok(SendOutcome::Cancelled),
        };
        // 发送阶段的错误只可能是网络类错误（请求构造错误在 builder 阶段就报掉了）
        let retryable = match &result {
            Ok(response) => retryable_status(response.status()),
            Err(_) => true,
        };
        if !retryable || retry >= MAX_RETRIES {
            return match result {
                // 状态码错误重试耗尽后，响应体留给调用方格式化（HTTP xxx: ...）
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
    pub cap_web_search: bool,
    pub web_search_tool: Option<serde_json::Value>,
    /// 模型支持图片输入（ReadMediaFile 的门控）
    pub input_image: bool,
    /// 展示用
    pub provider_name: String,
}

/// 随消息进上下文的图片（ReadMediaFile 输出）：Anthropic 进 content blocks，
/// OpenAI 拆成紧随的 user image_url 消息（见 to_openai_messages）
#[derive(Clone, Debug, Serialize)]
pub struct ChatImage {
    pub media_type: String,
    pub data_base64: String,
    /// 展示/标注用（OpenAI 拆分消息的 text part）；Anthropic 路径不消费
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
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
    /// 工具输出的图片：OpenAI 路径不走 serde 直序（见 to_openai_messages），
    /// 这个字段只被 Anthropic 的自定义构建读取
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ChatImage>,
    /// assistant 的思考内容：仅供 Anthropic 端点回传（thinking 模式要求带
    /// content[].thinking，否则第二轮 400），不参与序列化——OpenAI 兼容端点
    ///（DeepSeek 原生）要求不回传 reasoning_content。
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

    /// 带图片的工具结果（ReadMediaFile）：图片随 history 进上下文
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
    /// 单次请求的 token 用量：input = 未缓存命中的输入（cache_read 是其中
    /// 命中缓存的另一部分，两者之和为总输入），output 用于记账聚合，
    /// used 为模型上报的本请求总消耗（水位判断用），total 为模型上下文窗口
    Usage {
        input: u64,
        cache_read: u64,
        output: u64,
        used: u64,
        total: u64,
    },
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
        ApiFormat::AnthropicMessages => {
            stream_anthropic(&config, messages, tools, &tx, &cancel).await
        }
    };
    if let Err(error) = result {
        let _ = tx.send(ProviderEvent::Failed(error));
    }
}

// ---------------- OpenAI Chat Completions ----------------

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    /// 自定义构建（见 to_openai_messages）：带图工具结果拆成 tool 文本 + user 图片两条
    messages: &'a [serde_json::Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [serde_json::Value]>,
    stream: bool,
    stream_options: StreamOptions,
    max_tokens: u64,
}

/// 内部 ChatMsg 列表 → OpenAI messages 数组。
/// 与 serde 直序的唯一差异：OpenAI 的 tool 角色消息不能带图——带图的工具结果
///（ReadMediaFile）拆成两条：tool 消息只留文本 output，图片拆到紧随的 user
/// 消息（content parts：text 标注 + image_url data URL）。
fn to_openai_messages(messages: &[ChatMsg]) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.role == "tool" && !msg.images.is_empty() {
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
        } else {
            out.push(serde_json::to_value(msg).unwrap_or_default());
        }
    }
    out
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
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAiPromptDetails>,
}

#[derive(Deserialize)]
struct OpenAiPromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

fn finish(tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>, tool_calls: &mut Vec<ToolCall>) {
    // 模型偶尔发出无名 tool_use（name: null）：过滤掉，避免产生「未知工具」空调用；
    // 不进入历史也就不需要为它补 tool_result
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

/// OpenAI 兼容端点：无服务端搜索标准，只有显式配置 web_search_tool 才注入
///（如智谱 {"type":"web_search","web_search":{"enable":true,"search_result":true}}）。
pub fn openai_web_search_tool(config: &ResolvedModel) -> Option<serde_json::Value> {
    if !config.cap_web_search {
        return None;
    }
    config.web_search_tool.clone()
}

async fn stream_anthropic(
    config: &ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    let (system, messages) = to_anthropic_messages(&messages);
    let mut anthropic_tools = to_anthropic_tools(&tools);
    if let Some(tool) = anthropic_web_search_tool(config) {
        anthropic_tools.push(tool);
    }
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
    let client = http_client();
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
                    .send() => result.map_err(|e| net_err(e))?,
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
                    .send() => result.map_err(|e| net_err(e))?,
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
    let client = http_client();
    let send = async {
        match format {
            ApiFormat::OpenAiChat => {
                client
                    .post(format!(
                        "{}/chat/completions",
                        base_url.trim_end_matches('/')
                    ))
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
        .map_err(|e| net_err(e))?;
    let status = response.status();
    if status.is_success() {
        Ok(format!("连接成功（HTTP {status}）"))
    } else {
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(200).collect();
        Err(format!("HTTP {status}: {detail}"))
    }
}

/// 命令行网络探针（pig-app 的 PIG_NET_TEST=1 触发，不开窗口）：
/// 读取应用真实配置，逐个测试已启用供应商的模型连通性（ping + 真实流式请求），
/// 打印结果，用于网络排障。
pub fn net_test_blocking(config_path: &std::path::Path) {
    let config = match crate::config::load(config_path) {
        Ok(config) => config,
        Err(e) => {
            println!("[net-test] 读取配置失败（{}）: {e}", config_path.display());
            return;
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut tested = 0;
    for provider in config.providers.iter().filter(|p| p.enabled) {
        for model in &provider.models {
            tested += 1;
            let api_key = crate::config::expand_env(&provider.api_key);
            let result = rt.block_on(test_provider(
                &provider.base_url,
                &api_key,
                provider.api_format,
                &model.id,
            ));
            println!(
                "[net-test] ping {}/{} ({}) → {result:?}",
                provider.name, model.id, provider.base_url
            );

            // 与发送消息完全相同的流式路径
            let resolved = ResolvedModel {
                base_url: provider.base_url.clone(),
                api_key,
                model: model.id.clone(),
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
                api_format: provider.api_format,
                reasoning_params: None,
                cap_web_search: model.cap_web_search,
                web_search_tool: model.web_search_tool.clone(),
                input_image: model.input_image,
                provider_name: provider.name.clone(),
            };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let cancel = tokio_util::sync::CancellationToken::new();
            rt.block_on(stream_chat(
                resolved,
                vec![ChatMsg::user("ping".to_string())],
                Vec::new(),
                tx,
                cancel,
            ));
            let mut outcome = "Finished".to_string();
            while let Ok(event) = rx.try_recv() {
                match event {
                    ProviderEvent::Failed(e) => outcome = format!("Failed: {e}"),
                    ProviderEvent::ToolCalls(_) => outcome = "ToolCalls".to_string(),
                    _ => {}
                }
            }
            println!(
                "[net-test] stream {}/{} → {outcome}",
                provider.name, model.id
            );
        }
    }
    if tested == 0 {
        println!("[net-test] 没有已启用的供应商模型");
    }
}

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
                assert_eq!(calls.len(), 1, "无名调用应被过滤");
                assert_eq!(calls[0].name, "Bash");
            }
            other => panic!("应滤出剩余的命名调用: {other:?}"),
        }
        assert!(matches!(rx.try_recv(), Ok(ProviderEvent::Finished)));
    }

    #[test]
    fn anthropic_tool_result_with_images_becomes_blocks() {
        let messages = vec![ChatMsg::tool_result_with_images(
            "c1",
            "已读取图片 x.png".into(),
            vec![ChatImage {
                media_type: "image/png".into(),
                data_base64: "QUJD".into(),
                label: Some("x.png".into()),
            }],
        )];
        let (_system, out) = to_anthropic_messages(&messages);
        let content = out[0]["content"].as_array().expect("user 消息 blocks");
        assert_eq!(content[0]["type"], "tool_result");
        let blocks = content[0]["content"]
            .as_array()
            .expect("带图时 content 是数组");
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["type"], "base64");
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(blocks[0]["source"]["data"], "QUJD");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "已读取图片 x.png");
    }

    #[test]
    fn anthropic_tool_result_without_images_stays_string() {
        // 回归：无图路径与旧版一致（content 是字符串）
        let messages = vec![ChatMsg::tool_result("c1", "ok".into())];
        let (_system, out) = to_anthropic_messages(&messages);
        let content = out[0]["content"].as_array().expect("blocks");
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["content"], "ok", "无图时 content 保持字符串");
    }

    #[test]
    fn openai_tool_result_with_images_splits_into_two_messages() {
        let messages = vec![ChatMsg::tool_result_with_images(
            "c1",
            "已读取图片 x.png".into(),
            vec![ChatImage {
                media_type: "image/jpeg".into(),
                data_base64: "QUJD".into(),
                label: Some("x.png".into()),
            }],
        )];
        let out = to_openai_messages(&messages);
        assert_eq!(out.len(), 2, "拆成 tool 文本 + user 图片两条");
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "c1");
        assert_eq!(out[0]["content"], "已读取图片 x.png");
        assert!(out[0].get("images").is_none(), "tool 消息不带图");
        assert_eq!(out[1]["role"], "user");
        let parts = out[1]["content"].as_array().expect("content parts");
        assert_eq!(parts[0]["type"], "text");
        assert!(parts[0]["text"].as_str().unwrap().contains("x.png"));
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/jpeg;base64,QUJD");
    }

    #[test]
    fn openai_no_image_matches_plain_serde() {
        // 回归：无图时自定义构建与 serde 直序逐字节一致
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
    fn finish_all_nameless_emits_no_tool_calls() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut calls = vec![ToolCall::default()];
        finish(&tx, &mut calls);
        assert!(matches!(rx.try_recv(), Ok(ProviderEvent::Finished)));
        assert!(rx.try_recv().is_err(), "全是无名调用时不应发 ToolCalls");
    }

    #[test]
    fn anthropic_messages_echo_thinking_first() {
        let messages = vec![
            ChatMsg::system("s".into()),
            ChatMsg::user("u".into()),
            ChatMsg::assistant(
                "回答".into(),
                vec![ToolCall {
                    id: "c1".into(),
                    name: "Bash".into(),
                    arguments: "{}".into(),
                }],
                Some("想了想".into()),
            ),
            ChatMsg::tool_result("c1", "ok".into()),
        ];
        let (_system, out) = to_anthropic_messages(&messages);
        let assistant = &out[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "thinking");
        assert_eq!(assistant["content"][0]["thinking"], "想了想");
        assert_eq!(assistant["content"][1]["type"], "text");
        assert_eq!(assistant["content"][2]["type"], "tool_use");
    }

    #[test]
    fn anthropic_messages_skip_empty_thinking() {
        let messages = vec![ChatMsg::assistant("回答".into(), vec![], None)];
        let (_system, out) = to_anthropic_messages(&messages);
        let content = out[0]["content"].as_array().expect("blocks");
        assert_eq!(content.len(), 1, "无思考内容时不应有 thinking 块");
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
        // cap 关 → 不注入
        assert!(anthropic_web_search_tool(&search_model(false, None)).is_none());
        // cap 开、无自定义 → 默认 web_search_20250305
        let tool = anthropic_web_search_tool(&search_model(true, None)).expect("cap 开应注入");
        assert_eq!(
            tool,
            serde_json::json!({"type": "web_search_20250305", "name": "web_search"})
        );
        // cap 开、有自定义 → 用自定义 JSON
        let custom =
            serde_json::json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3});
        assert_eq!(
            anthropic_web_search_tool(&search_model(true, Some(custom.clone()))).unwrap(),
            custom
        );
    }

    #[test]
    fn openai_web_search_tool_injection() {
        // cap 关 → 不注入
        assert!(openai_web_search_tool(&search_model(false, None)).is_none());
        // cap 开但无自定义 → 不注入（OpenAI 兼容端点无服务端搜索标准）
        assert!(openai_web_search_tool(&search_model(true, None)).is_none());
        // cap 开且有自定义（如智谱）→ 注入
        let custom = serde_json::json!({
            "type": "web_search",
            "web_search": {"enable": true, "search_result": true}
        });
        assert_eq!(
            openai_web_search_tool(&search_model(true, Some(custom.clone()))).unwrap(),
            custom
        );
    }
}
