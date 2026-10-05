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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatImage {
    pub media_type: String,
    pub data_base64: String,
    /// 展示/标注用（OpenAI 拆分消息的 text part）；Anthropic 路径不消费
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// 子代理上下文持久化（{session}.agents/*.jsonl）要反序列化，故带 Deserialize
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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

/// 子代理上下文持久化（{session}.agents/*.jsonl）要反序列化，故带 Deserialize
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallWire {
    pub id: String,
    /// 恒为 "function"：&'static str 无法反序列化，存 String（序列化形态不变）
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

mod anthropic;
mod openai;
mod sidecar;

pub use anthropic::anthropic_web_search_tool;
pub(crate) use anthropic::*;
pub use openai::openai_web_search_tool;
pub(crate) use openai::*;
pub use sidecar::{complete_messages, complete_text, net_test_blocking, test_provider};

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
    fn user_message_with_images_both_formats() {
        let mut msg = ChatMsg::user("看图".into());
        msg.images = vec![ChatImage {
            media_type: "image/png".into(),
            data_base64: "QUJD".into(),
            label: Some("1.png".into()),
        }];
        // OpenAI：user content 改 parts（文本在前、image_url 在后）
        let out = to_openai_messages(&[msg.clone()]);
        assert_eq!(out.len(), 1, "user 带图不拆条");
        let parts = out[0]["content"].as_array().expect("parts");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "看图");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
        // Anthropic：user content 改 blocks（image 在前、text 在后）
        let (_s, out) = to_anthropic_messages(&[msg]);
        let blocks = out[0]["content"].as_array().expect("blocks");
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["data"], "QUJD");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "看图");
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

    /// 请求级工具组装（流式/非流式共用）：Anthropic 形态必须是
    /// name/description/input_schema——残留 OpenAI 线格式会被兼容端点 422
    #[test]
    fn anthropic_request_tools_converts_openai_wire_shape() {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "Read", "description": "读文件", "parameters": {"type": "object"}}
        })];
        let out = anthropic_request_tools(&search_model(false, None), &tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["name"], "Read");
        assert_eq!(out[0]["description"], "读文件");
        assert_eq!(out[0]["input_schema"]["type"], "object");
        assert!(
            out[0].get("function").is_none(),
            "不得残留 OpenAI 线格式: {out:?}"
        );
        assert!(
            out[0].get("type").is_none(),
            "custom 工具不带 type: {out:?}"
        );
        // cap 开 → 追加服务端搜索工具（与流式路径同一组装）
        let out = anthropic_request_tools(&search_model(true, None), &tools);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["type"], "web_search_20250305");
    }

    /// OpenAI 侧：原样透传线格式，cap+自定义时才追加服务端搜索工具
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
