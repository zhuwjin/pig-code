//! streamable HTTP 传输（MCP 2025-03-26+）：单端点 POST JSON-RPC。
//! - 请求头：Accept 同时给 `application/json, text/event-stream`，自定义 headers 全量携带；
//! - 响应两种形态都支持：`application/json` 直接回包 / `text/event-stream` SSE 流回包
//!   （流内按 id 配对，server→client 请求另行 POST 回 -32601，通知忽略）；
//! - `Mcp-Session-Id`：initialize 响应带回后后续请求（含 DELETE）都携带；
//! - `MCP-Protocol-Version`：按 initialize 协商结果携带（2025-06-18 起服务端要求）；
//! - DELETE 终止会话尽力而为（无会话 id / 405 / 网络失败都忽略）；
//! - 防御与 stdio 同口径：单次调用超时一致、响应体 8MB 上限、连接断开即本请求报错。
//!
//! legacy SSE（2024-11-05 双端点 HTTP+SSE）不支持——config 层已拦下该形态。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt as _;
use serde_json::{Value, json};

use crate::mcp::config::McpHttpConfig;

/// streamable HTTP 握手宣告的协议版本（该传输形态自 2025-03-26 引入）
const PROTOCOL_VERSION_HTTP: &str = "2025-03-26";
/// 响应体上限（对齐 stdio 单行上限；SSE 流按累计字节计）
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 报错文案里带的响应体预览上限
const ERROR_BODY_PREVIEW: usize = 2 * 1024;
/// 自定义 header 黑名单：逐跳/ framing 头由 reqwest 自己管理，用户配置会让请求畸形
const BLOCKED_HEADERS: &[&str] = &[
    "content-length",
    "host",
    "connection",
    "transfer-encoding",
    "expect",
    "upgrade",
    "te",
    "trailer",
    "keep-alive",
];

/// streamable HTTP 传输：每个请求独立 POST（天然并发安全，无共享 in-flight 状态；
/// 可变共享态只有 session_id/protocol_version 两个协商结果，Mutex 保护）
pub struct HttpTransport {
    name: String,
    client: reqwest::Client,
    url: String,
    /// 预编译的自定义请求头（new 时校验，非法/黑名单条目拒绝整个 server 配置）
    headers: reqwest::header::HeaderMap,
    timeout: Duration,
    next_id: AtomicU64,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    closed: AtomicBool,
}

impl HttpTransport {
    pub fn new(name: &str, config: &McpHttpConfig, timeout: Duration) -> Result<Self, String> {
        // URL 在 config 层只验了 scheme，这里完整解析（失败跳过该 server）
        reqwest::Url::parse(&config.url).map_err(|e| format!("url 无效: {e}"))?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (key, value) in &config.headers {
            let key_lower = key.to_ascii_lowercase();
            if BLOCKED_HEADERS.contains(&key_lower.as_str()) {
                return Err(format!("header {key} 由传输层管理，不允许自定义"));
            }
            let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
                .map_err(|_| format!("header 名非法: {key}"))?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| format!("header {key} 的值含非法字符"))?;
            headers.insert(name, value);
        }
        Ok(Self {
            name: name.to_string(),
            // 与 provider 同口径：只限建连时间，整体超时由每次调用的 tokio timeout 管
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            url: config.url.clone(),
            headers,
            timeout,
            next_id: AtomicU64::new(1),
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            closed: AtomicBool::new(false),
        })
    }

    /// 组请求头：自定义 → 协议强制（content-type/accept，防自定义覆盖）→ 协商态
    fn build_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = self.headers.clone();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json, text/event-stream"),
        );
        if let Some(session) = self.session_id.lock().expect("session lock").clone()
            && let Ok(value) = reqwest::header::HeaderValue::from_str(&session)
        {
            headers.insert("mcp-session-id", value);
        }
        if let Some(version) = self.protocol_version.lock().expect("version lock").clone()
            && let Ok(value) = reqwest::header::HeaderValue::from_str(&version)
        {
            headers.insert("mcp-protocol-version", value);
        }
        headers
    }

    /// 记录响应带回的会话 id（initialize 响应协商；后续响应若再带则跟随滚动）
    fn capture_session(&self, response: &reqwest::Response) {
        if let Some(value) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().expect("session lock") = Some(value.to_string());
        }
    }

    /// 发 POST 并做状态码检查；成功返回响应（体未读）
    async fn post(&self, message: &Value) -> Result<reqwest::Response, String> {
        let body = serde_json::to_vec(message).map_err(|e| format!("序列化消息失败: {e}"))?;
        let response = self
            .client
            .post(&self.url)
            .headers(self.build_headers())
            .body(body)
            .send()
            .await
            .map_err(|e| format!("MCP server {} 请求失败: {e}", self.name))?;
        self.capture_session(&response);
        let status = response.status();
        if !status.is_success() {
            let preview = read_body_capped(response, ERROR_BODY_PREVIEW)
                .await
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let hint = if status.as_u16() == 404 {
                "（会话可能已过期或服务端路径不对）"
            } else {
                ""
            };
            return Err(format!(
                "MCP server {} HTTP {status}{hint}: {}",
                self.name,
                preview.trim()
            ));
        }
        Ok(response)
    }

    /// 从响应里取回指定 id 的 JSON-RPC 结果：按 Content-Type 分流 JSON / SSE
    async fn take_response(
        &self,
        response: reqwest::Response,
        want_id: u64,
    ) -> Result<Value, String> {
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let media = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match media.as_str() {
            "application/json" => {
                let body = read_body_capped(response, MAX_BODY_BYTES).await?;
                let message: Value = serde_json::from_slice(&body)
                    .map_err(|e| format!("MCP server {} 响应不是合法 JSON: {e}", self.name))?;
                match message.get("id").and_then(Value::as_u64) {
                    Some(id) if id != want_id => Err(format!(
                        "MCP server {} 响应 id {id} 与请求 {want_id} 不符",
                        self.name
                    )),
                    _ => rpc_result(&message)
                        .ok_or_else(|| format!("MCP server {} 响应缺少 result/error", self.name))?,
                }
            }
            "text/event-stream" => self.read_sse_response(response, want_id).await,
            _ => Err(format!(
                "MCP server {} 响应 Content-Type 既不是 JSON 也不是 SSE（{content_type}）",
                self.name
            )),
        }
    }

    /// SSE 流回包：边读边解码事件，找到 want_id 的响应即返回；
    /// 流内 server→client 请求另开 POST 回 -32601；流耗尽未配对 = 连接断开语义
    async fn read_sse_response(
        &self,
        response: reqwest::Response,
        want_id: u64,
    ) -> Result<Value, String> {
        let mut decoder = SseDecoder::default();
        let mut total = 0usize;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| format!("MCP server {} SSE 流读取失败: {e}", self.name))?;
            total += chunk.len();
            if total > MAX_BODY_BYTES {
                return Err(format!(
                    "MCP server {} SSE 流超过 {} KB 上限",
                    self.name,
                    MAX_BODY_BYTES / 1024
                ));
            }
            for payload in decoder.push(&chunk) {
                if let Some(result) = self.handle_stream_message(&payload, want_id)? {
                    return result;
                }
            }
        }
        for payload in decoder.finish() {
            if let Some(result) = self.handle_stream_message(&payload, want_id)? {
                return result;
            }
        }
        Err(format!(
            "MCP server {} SSE 流结束但未收到本请求响应（连接已断开）",
            self.name
        ))
    }

    /// 处理流内单条消息：是本请求响应 → Ok(Some)；是 server→client 请求 → 回 -32601 后 Ok(None)；
    /// 其余（通知/别 id 响应）忽略。payload 非法 JSON → Err（流畸形，整调用失败）
    fn handle_stream_message(
        &self,
        payload: &str,
        want_id: u64,
    ) -> Result<Option<Result<Value, String>>, String> {
        let message: Value = serde_json::from_str(payload)
            .map_err(|e| format!("MCP server {} SSE 事件非合法 JSON: {e}", self.name))?;
        let id = message.get("id").and_then(Value::as_u64);
        if id == Some(want_id)
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            return Ok(Some(rpc_result(&message).expect("result/error 已判定")));
        }
        if id.is_some() && message.get("method").is_some() {
            // server→client 请求（roots/elicitation/sampling 等，不支持但不能干等）：
            // 按规范另开 POST 回 JSON-RPC 响应（对端回 202）
            let reply = json!({
                "jsonrpc": "2.0",
                "id": message["id"],
                "error": {"code": -32601, "message": "pig-code 暂不支持服务端请求"},
            });
            let client = self.client.clone();
            let url = self.url.clone();
            let headers = self.build_headers();
            tokio::spawn(async move {
                let _ = client
                    .post(&url)
                    .headers(headers)
                    .body(reply.to_string())
                    .send()
                    .await;
            });
        }
        Ok(None)
    }
}

impl HttpTransport {
    pub(super) async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err(format!("MCP server {} 连接已断开", self.name));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        // 与 stdio 同口径：单次调用一个超时，覆盖建连+发送+读完整响应
        let call = async {
            let response = self.post(&message).await?;
            // 通知口径的 202（无响应体）不该出现在带 id 请求上
            if response.status().as_u16() == 202 {
                Err(format!(
                    "MCP server {} 对请求回了 202（无响应体），协议违规",
                    self.name
                ))
            } else {
                self.take_response(response, id).await
            }
        };
        match tokio::time::timeout(self.timeout, call).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "MCP server {} 调用 {method} 超时（{} ms）",
                self.name,
                self.timeout.as_millis()
            )),
        }
    }

    pub(super) async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        match tokio::time::timeout(self.timeout, self.post(&message)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(format!(
                "MCP server {} 通知 {method} 超时（{} ms）",
                self.name,
                self.timeout.as_millis()
            )),
        }
    }

    /// DELETE 终止会话尽力而为：只在协商出会话 id 后发；任何失败都吞掉
    pub(super) async fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        if self.session_id.lock().expect("session lock").is_none() {
            return;
        }
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            self.client
                .delete(&self.url)
                .headers(self.build_headers())
                .send(),
        )
        .await;
    }

    pub(super) fn protocol_version(&self) -> &'static str {
        PROTOCOL_VERSION_HTTP
    }

    pub(super) fn notify_negotiated(&self, server_version: &str) {
        *self.protocol_version.lock().expect("version lock") = Some(server_version.to_string());
    }
}

/// JSON-RPC 响应消息 → Result（result/error 都不存在 → None）
fn rpc_result(message: &Value) -> Option<Result<Value, String>> {
    if let Some(error) = message.get("error") {
        return Some(Err(format!(
            "MCP 错误 {}: {}",
            error["code"].as_i64().unwrap_or(-1),
            error["message"].as_str().unwrap_or("未知错误")
        )));
    }
    message.get("result").map(|result| Ok(result.clone()))
}

/// 有上限地读响应体：Content-Length 预检 + 流式累计超限即断
async fn read_body_capped(response: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    if let Some(len) = response.content_length()
        && len > cap as u64
    {
        return Err(format!("响应体 {} KB 超过上限", len / 1024));
    }
    let mut buf = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("读取响应体失败: {e}"))?;
        if buf.len() + chunk.len() > cap {
            return Err(format!("响应体超过 {} KB 上限", cap / 1024));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// 增量 SSE 解码器：只产出事件 data 载荷（多行 data 按 \n 拼接；空行派发；
/// `:` 注释/keepalive 与 event/id/retry 字段忽略）；容忍 CRLF
#[derive(Default)]
struct SseDecoder {
    buf: Vec<u8>,
    data: String,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            self.process_line(&line, &mut events);
        }
        events
    }

    /// 流收尾：缓冲里剩余的不完整行也按行处理（容忍尾事件缺空行）
    fn finish(mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).into_owned();
            self.process_line(&line, &mut events);
        }
        if !self.data.is_empty() {
            events.push(std::mem::take(&mut self.data));
        }
        events
    }

    fn process_line(&mut self, line: &str, events: &mut Vec<String>) {
        if line.is_empty() {
            if !self.data.is_empty() {
                events.push(std::mem::take(&mut self.data));
            }
            return;
        }
        if let Some(payload) = line
            .strip_prefix("data:")
            .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
        {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(payload);
        }
        // 其余字段（event:/id:/retry:/注释）与本协议用法无关，忽略
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::client::McpClient;
    use crate::mcp::config::{McpServerConfig, McpTransport};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 假 server 响应形态：直接 JSON / SSE 流
    #[derive(Clone, Copy, PartialEq)]
    enum Shape {
        Json,
        Sse,
    }

    /// 假 server 观察到的请求事实（断言会话协商/自定义头/DELETE 用）
    #[derive(Default)]
    struct Seen {
        initialize_auth: bool,
        post_without_session: Vec<String>,
        delete_seen: bool,
    }

    /// 用 tokio TcpListener 手搓的极简 HTTP server：每连接处理一个请求即关闭。
    /// 要求：自定义头 Authorization: Bearer t（initialize 时校验）、
    /// 非 initialize 的 POST 必须带 Mcp-Session-Id: sess-1。
    async fn spawn_fake_server(shape: Shape) -> (String, Arc<Mutex<Seen>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let seen_task = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen_task.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(&mut socket, shape, &seen).await;
                });
            }
        });
        (format!("http://{addr}/mcp"), seen)
    }

    async fn handle_conn(
        socket: &mut tokio::net::TcpStream,
        shape: Shape,
        seen: &Mutex<Seen>,
    ) -> Result<(), String> {
        // 读头部（请求行 + headers 到空行）
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos;
            }
            let n = socket
                .read(&mut chunk)
                .await
                .map_err(|e| format!("读失败: {e}"))?;
            if n == 0 {
                return Err("连接提前关闭".to_string());
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > 64 * 1024 {
                return Err("头部过大".to_string());
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or("");
        let method = request_line.split_whitespace().next().unwrap_or("");
        let headers: HashMap<String, String> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let content_length: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // 读体（可能已随头部到达一部分）
        let mut body = buf[header_end + 4..].to_vec();
        while body.len() < content_length {
            let n = socket
                .read(&mut chunk)
                .await
                .map_err(|e| format!("读体失败: {e}"))?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        if body.len() < content_length {
            return Err("请求体不完整".to_string());
        }

        if method == "DELETE" {
            seen.lock().expect("seen").delete_seen = true;
            return write_response(socket, 200, "OK", "application/json", b"{}".as_slice(), &[])
                .await;
        }
        if method != "POST" {
            return write_response(
                socket,
                405,
                "Method Not Allowed",
                "text/plain",
                b"".as_slice(),
                &[],
            )
            .await;
        }

        let message: Value = serde_json::from_slice(&body[..content_length])
            .map_err(|e| format!("请求体非 JSON: {e}"))?;
        let rpc_method = message["method"].as_str().unwrap_or("").to_string();
        let id = message["id"].clone();

        // 自定义头校验：initialize 必须带 Authorization
        if rpc_method == "initialize" {
            seen.lock().expect("seen").initialize_auth =
                headers.get("authorization").map(String::as_str) == Some("Bearer t");
        } else if headers.get("mcp-session-id").map(String::as_str) != Some("sess-1") {
            seen.lock()
                .expect("seen")
                .post_without_session
                .push(rpc_method.clone());
            return write_response(
                socket,
                400,
                "Bad Request",
                "application/json",
                br#"{"error":"missing session"}"#.as_slice(),
                &[],
            )
            .await;
        }

        // 通知：202 空体
        if id.is_null() {
            return write_response(socket, 202, "Accepted", "text/plain", b"".as_slice(), &[])
                .await;
        }
        let result = match rpc_method.as_str() {
            "initialize" => json!({
                "protocolVersion": PROTOCOL_VERSION_HTTP,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-http", "version": "0.1"}
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": [{
                "name": "echo",
                "description": "回显 text 参数",
                "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
                "annotations": {"readOnlyHint": true}
            }]}),
            "tools/call" => json!({"content": [{
                "type": "text",
                "text": message["params"]["arguments"]["text"].as_str().unwrap_or("")
            }]}),
            _ => {
                let reply = json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "no such method"}});
                return write_rpc(socket, shape, &reply, rpc_method == "initialize").await;
            }
        };
        let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
        write_rpc(socket, shape, &reply, rpc_method == "initialize").await
    }

    /// 按形态写 JSON-RPC 响应：initialize 响应带 Mcp-Session-Id 协商头
    async fn write_rpc(
        socket: &mut tokio::net::TcpStream,
        shape: Shape,
        reply: &Value,
        with_session: bool,
    ) -> Result<(), String> {
        let extra: &[(&str, &str)] = if with_session {
            &[("Mcp-Session-Id", "sess-1")]
        } else {
            &[]
        };
        match shape {
            Shape::Json => {
                write_response(
                    socket,
                    200,
                    "OK",
                    "application/json",
                    reply.to_string().as_bytes(),
                    extra,
                )
                .await
            }
            Shape::Sse => {
                // 注释 keepalive + CRLF + 多行 data 切片，顺便压测解码器边界。
                // 多行 data 以 \n 拼接，切点必须在 JSON token 间隙（逗号处），
                // 否则拼出的串是非法 JSON
                let payload = reply.to_string();
                let mid = payload.find(',').unwrap_or(payload.len());
                let body = format!(
                    ": keepalive\r\nevent: message\r\ndata: {},\r\ndata:{}\r\n\r\n",
                    &payload[..mid],
                    &payload[mid + 1..]
                );
                write_response(
                    socket,
                    200,
                    "OK",
                    "text/event-stream",
                    body.as_bytes(),
                    extra,
                )
                .await
            }
        }
    }

    async fn write_response(
        socket: &mut tokio::net::TcpStream,
        status: u16,
        reason: &str,
        content_type: &str,
        body: &[u8],
        extra_headers: &[(&str, &str)],
    ) -> Result<(), String> {
        let mut head = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n",
            body.len()
        );
        if !body.is_empty() || status == 200 {
            head.push_str(&format!("content-type: {content_type}\r\n"));
        }
        for (k, v) in extra_headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        socket
            .write_all(head.as_bytes())
            .await
            .map_err(|e| format!("写响应失败: {e}"))?;
        socket
            .write_all(body)
            .await
            .map_err(|e| format!("写响应失败: {e}"))?;
        Ok(())
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn http_config(url: String) -> McpServerConfig {
        McpServerConfig {
            name: "fake-http".to_string(),
            transport: McpTransport::Http(crate::mcp::config::McpHttpConfig {
                url,
                headers: HashMap::from([("Authorization".to_string(), "Bearer t".to_string())]),
            }),
            timeout: Duration::from_secs(10),
            disabled: false,
        }
    }

    /// 全链路（两种响应形态共用）：initialize（协商会话 id + 自定义头）→
    /// tools/list → tools/call → ping → shutdown（DELETE 尽力）
    async fn roundtrip(shape: Shape) {
        let (url, seen) = spawn_fake_server(shape).await;
        let (client, specs) = McpClient::connect(&http_config(url), std::path::Path::new("."))
            .await
            .expect("connect");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
        assert!(specs[0].annotations.is_read_only());

        let result = client
            .call_tool("echo", json!({"text": "你好 http"}))
            .await
            .expect("call_tool");
        assert_eq!(result["content"][0]["text"], "你好 http");
        client.ping().await.expect("ping");
        client.shutdown().await;

        let seen = seen.lock().expect("seen");
        assert!(seen.initialize_auth, "自定义 Authorization 头必须到达");
        assert!(
            seen.post_without_session.is_empty(),
            "非 initialize 请求必须带会话 id: {:?}",
            seen.post_without_session
        );
        assert!(seen.delete_seen, "shutdown 应发 DELETE");
    }

    #[tokio::test]
    async fn http_json_roundtrip() {
        roundtrip(Shape::Json).await;
    }

    #[tokio::test]
    async fn http_sse_roundtrip() {
        roundtrip(Shape::Sse).await;
    }

    #[tokio::test]
    async fn sse_decoder_multiline_and_crlf() {
        let mut dec = SseDecoder::default();
        assert!(dec.push(b": comment\r\n").is_empty());
        assert!(dec.push(b"event: message\r\ndata: {\"a").is_empty());
        let events = dec.push(b":1}\r\ndata: second\r\n\r\n");
        assert_eq!(events, vec!["{\"a:1}\nsecond".to_string()]);
        // 尾部无空行的事件在 finish 时派发
        let mut dec = SseDecoder::default();
        assert!(dec.push(b"data: tail").is_empty());
        assert_eq!(dec.finish(), vec!["tail".to_string()]);
    }

    #[test]
    fn custom_headers_validated() {
        let mut config = crate::mcp::config::McpHttpConfig {
            url: "http://127.0.0.1/".to_string(),
            headers: HashMap::from([("content-length".to_string(), "5".to_string())]),
        };
        assert!(
            HttpTransport::new("t", &config, Duration::from_secs(1))
                .err()
                .expect("黑名单 header 应拒绝")
                .contains("content-length")
        );
        config.headers = HashMap::from([("bad name".to_string(), "v".to_string())]);
        assert!(HttpTransport::new("t", &config, Duration::from_secs(1)).is_err());
        config.headers = HashMap::from([("X-Ok".to_string(), "v".to_string())]);
        assert!(HttpTransport::new("t", &config, Duration::from_secs(1)).is_ok());
    }
}
