//! streamable HTTP transport (MCP 2025-03-26+): single-endpoint POST JSON-RPC.
//! - Request headers: Accept carries both `application/json, text/event-stream`; custom headers are all included;
//! - Both response shapes are supported: `application/json` direct reply / `text/event-stream` SSE streaming reply
//!   (within the stream, responses are matched by id; server-to-client requests get a separate POST reply of -32601; notifications are ignored);
//! - `Mcp-Session-Id`: once returned by the initialize response, carried on all subsequent requests (including DELETE);
//! - `MCP-Protocol-Version`: carried per the initialize negotiation result (required by servers since 2025-06-18);
//! - DELETE session termination is best-effort (missing session id / 405 / network failures are all ignored);
//! - Defenses match stdio: same per-call timeout, 8MB response body cap, a connection drop fails the current request.
//!
//! legacy SSE (2024-11-05 dual-endpoint HTTP+SSE) is not supported -- the config layer already rejects that shape.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt as _;
use serde_json::{Value, json};

use crate::mcp::config::McpHttpConfig;
use pig_protocol::CoreError;

/// Protocol version declared in the streamable HTTP handshake (this transport shape was introduced in 2025-03-26)
const PROTOCOL_VERSION_HTTP: &str = "2025-03-26";
/// Response body cap (aligned with the stdio per-line cap; SSE streams are measured by cumulative bytes)
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Cap on the response body preview included in error messages
const ERROR_BODY_PREVIEW: usize = 2 * 1024;
/// Custom header blocklist: hop-by-hop/framing headers are managed by reqwest itself; user configuration would malform the request
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

/// streamable HTTP transport: each request is an independent POST (naturally concurrency-safe, no shared in-flight state;
/// the only mutable shared state is the two negotiation results session_id/protocol_version, protected by a Mutex)
pub struct HttpTransport {
    name: String,
    client: reqwest::Client,
    url: String,
    /// Pre-built custom request headers (validated in new; an invalid/blocklisted entry rejects the whole server config)
    headers: reqwest::header::HeaderMap,
    timeout: Duration,
    next_id: AtomicU64,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    closed: AtomicBool,
}

impl HttpTransport {
    pub fn new(name: &str, config: &McpHttpConfig, timeout: Duration) -> Result<Self, CoreError> {
        // The config layer only validates the URL scheme; parse it fully here (a failure skips this server)
        reqwest::Url::parse(&config.url).map_err(|e| CoreError::McpInvalidUrl {
            detail: e.to_string(),
        })?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (key, value) in &config.headers {
            let key_lower = key.to_ascii_lowercase();
            if BLOCKED_HEADERS.contains(&key_lower.as_str()) {
                return Err(CoreError::McpHeaderBlocked { key: key.clone() });
            }
            let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
                .map_err(|_| CoreError::McpHeaderNameInvalid { key: key.clone() })?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| CoreError::McpHeaderValueInvalid { key: key.clone() })?;
            headers.insert(name, value);
        }
        Ok(Self {
            name: name.to_string(),
            // Same policy as the provider: only the connect time is limited; the overall timeout is governed by each call's tokio timeout
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

    /// Build request headers: custom -> protocol-forced (content-type/accept, preventing custom overrides) -> negotiated state
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

    /// Record the session id returned by a response (negotiated from the initialize response; later responses carrying one again roll it forward)
    fn capture_session(&self, response: &reqwest::Response) {
        if let Some(value) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().expect("session lock") = Some(value.to_string());
        }
    }

    /// Send the POST and check the status code; returns the response on success (body unread)
    async fn post(&self, message: &Value) -> Result<reqwest::Response, String> {
        let body =
            serde_json::to_vec(message).map_err(|e| format!("Failed to serialize message: {e}"))?;
        let response = self
            .client
            .post(&self.url)
            .headers(self.build_headers())
            .body(body)
            .send()
            .await
            .map_err(|e| format!("MCP server {} request failed: {e}", self.name))?;
        self.capture_session(&response);
        let status = response.status();
        if !status.is_success() {
            let preview = read_body_capped(response, ERROR_BODY_PREVIEW)
                .await
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let hint = if status.as_u16() == 404 {
                " (the session may have expired or the endpoint path is wrong)"
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

    /// Extract the JSON-RPC result with the given id from the response: dispatch to JSON / SSE by Content-Type
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
                let message: Value = serde_json::from_slice(&body).map_err(|e| {
                    format!("MCP server {} response is not valid JSON: {e}", self.name)
                })?;
                match message.get("id").and_then(Value::as_u64) {
                    Some(id) if id != want_id => Err(format!(
                        "MCP server {} response id {id} does not match request id {want_id}",
                        self.name
                    )),
                    _ => rpc_result(&message).ok_or_else(|| {
                        format!(
                            "MCP server {} response has neither result nor error",
                            self.name
                        )
                    })?,
                }
            }
            "text/event-stream" => self.read_sse_response(response, want_id).await,
            _ => Err(format!(
                "MCP server {} response Content-Type is neither JSON nor SSE ({content_type})",
                self.name
            )),
        }
    }

    /// SSE streaming reply: decode events while reading, return as soon as the response for want_id is found;
    /// server-to-client requests within the stream get a separate POST reply of -32601; a stream exhausted without a match means the connection dropped
    async fn read_sse_response(
        &self,
        response: reqwest::Response,
        want_id: u64,
    ) -> Result<Value, String> {
        let mut decoder = SseDecoder::default();
        let mut total = 0usize;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|e| format!("MCP server {} SSE stream read failed: {e}", self.name))?;
            total += chunk.len();
            if total > MAX_BODY_BYTES {
                return Err(format!(
                    "MCP server {} SSE stream exceeded the {} KB limit",
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
            "MCP server {} SSE stream ended without a response to this request (connection closed)",
            self.name
        ))
    }

    /// Handle a single message within the stream: a response to this request -> Ok(Some); a server-to-client request -> reply -32601 then Ok(None);
    /// everything else (notifications / responses with other ids) is ignored. An invalid JSON payload -> Err (malformed stream, the whole call fails)
    fn handle_stream_message(
        &self,
        payload: &str,
        want_id: u64,
    ) -> Result<Option<Result<Value, String>>, String> {
        let message: Value = serde_json::from_str(payload)
            .map_err(|e| format!("MCP server {} SSE event is not valid JSON: {e}", self.name))?;
        let id = message.get("id").and_then(Value::as_u64);
        if id == Some(want_id)
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            return Ok(Some(
                rpc_result(&message).expect("result/error already decided"),
            ));
        }
        if id.is_some() && message.get("method").is_some() {
            // server-to-client request (roots/elicitation/sampling etc.; unsupported but we cannot just wait):
            // per the spec, open a separate POST to reply with a JSON-RPC response (the peer replies 202)
            let reply = json!({
                "jsonrpc": "2.0",
                "id": message["id"],
                "error": {"code": -32601, "message": "Pig Code does not support server-initiated requests"},
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
            return Err(format!("MCP server {} connection is closed", self.name));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        // Same policy as stdio: one timeout per call, covering connect + send + reading the full response
        let call = async {
            let response = self.post(&message).await?;
            // A notification-style 202 (no response body) must not appear on a request carrying an id
            if response.status().as_u16() == 202 {
                Err(format!(
                    "MCP server {} replied 202 (no response body) to a request — protocol violation",
                    self.name
                ))
            } else {
                self.take_response(response, id).await
            }
        };
        match tokio::time::timeout(self.timeout, call).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "MCP server {} call to {method} timed out after {} ms",
                self.name,
                self.timeout.as_millis()
            )),
        }
    }

    pub(super) async fn notify(&self, method: &str, params: Value) -> Result<(), CoreError> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let failure = |detail: String| CoreError::McpNotifyWrite {
            name: self.name.clone(),
            method: method.to_string(),
            detail,
        };
        match tokio::time::timeout(self.timeout, self.post(&message)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(failure(e)),
            Err(_) => Err(failure(format!(
                "timed out after {} ms",
                self.timeout.as_millis()
            ))),
        }
    }

    /// Best-effort DELETE to terminate the session: only sent once a session id has been negotiated; any failure is swallowed
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

/// JSON-RPC response message -> Result (neither result nor error present -> None)
fn rpc_result(message: &Value) -> Option<Result<Value, String>> {
    if let Some(error) = message.get("error") {
        return Some(Err(format!(
            "MCP error {}: {}",
            error["code"].as_i64().unwrap_or(-1),
            error["message"].as_str().unwrap_or("unknown error")
        )));
    }
    message.get("result").map(|result| Ok(result.clone()))
}

/// Read the response body with a cap: Content-Length precheck + abort as soon as the cumulative streaming total exceeds the cap
async fn read_body_capped(response: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    if let Some(len) = response.content_length()
        && len > cap as u64
    {
        return Err(format!(
            "Response body of {} KB exceeds the limit",
            len / 1024
        ));
    }
    let mut buf = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Failed to read the response body: {e}"))?;
        if buf.len() + chunk.len() > cap {
            return Err(format!(
                "Response body exceeded the {} KB limit",
                cap / 1024
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Incremental SSE decoder: only emits event data payloads (multi-line data joined with \n; dispatched on a blank line;
/// `:` comments/keepalives and event/id/retry fields are ignored); CRLF-tolerant
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

    /// Stream end: the leftover incomplete line in the buffer is processed as a line too (tolerates a trailing event without a blank line)
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
        // Other fields (event:/id:/retry:/comments) are irrelevant to this protocol usage; ignored
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

    /// Fake server response shape: direct JSON / SSE stream
    #[derive(Clone, Copy, PartialEq)]
    enum Shape {
        Json,
        Sse,
    }

    /// Request facts observed by the fake server (used to assert session negotiation / custom headers / DELETE)
    #[derive(Default)]
    struct Seen {
        initialize_auth: bool,
        post_without_session: Vec<String>,
        delete_seen: bool,
    }

    /// A hand-rolled minimal HTTP server on a tokio TcpListener: handles one request per connection, then closes.
    /// Requires: custom header Authorization: Bearer t (checked on initialize), and
    /// any non-initialize POST must carry Mcp-Session-Id: sess-1.
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
        // Read the head (request line + headers up to the blank line)
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos;
            }
            let n = socket
                .read(&mut chunk)
                .await
                .map_err(|e| format!("read failed: {e}"))?;
            if n == 0 {
                return Err("connection closed early".to_string());
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > 64 * 1024 {
                return Err("headers too large".to_string());
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
        // Read the body (part of it may have already arrived with the head)
        let mut body = buf[header_end + 4..].to_vec();
        while body.len() < content_length {
            let n = socket
                .read(&mut chunk)
                .await
                .map_err(|e| format!("body read failed: {e}"))?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        if body.len() < content_length {
            return Err("incomplete request body".to_string());
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
            .map_err(|e| format!("request body is not JSON: {e}"))?;
        let rpc_method = message["method"].as_str().unwrap_or("").to_string();
        let id = message["id"].clone();

        // Custom header check: initialize must carry Authorization
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

        // Notification: 202 with an empty body
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
                "description": "Echoes the text parameter",
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

    /// Write a JSON-RPC response per the shape: the initialize response carries the Mcp-Session-Id negotiation header
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
                // Comment keepalive + CRLF + multi-line data splitting also stress-tests the decoder's edges.
                // Multi-line data is joined with \n; the split point must fall in a gap between JSON tokens (at a comma),
                // otherwise the rejoined string is invalid JSON
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
            .map_err(|e| format!("write response failed: {e}"))?;
        socket
            .write_all(body)
            .await
            .map_err(|e| format!("write response failed: {e}"))?;
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

    /// Full round trip (shared by both response shapes): initialize (negotiates the session id + custom headers) ->
    /// tools/list -> tools/call -> ping -> shutdown (best-effort DELETE)
    async fn roundtrip(shape: Shape) {
        let (url, seen) = spawn_fake_server(shape).await;
        let (client, specs) = McpClient::connect(&http_config(url), std::path::Path::new("."))
            .await
            .expect("connect");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
        assert!(specs[0].annotations.is_read_only());

        let result = client
            .call_tool("echo", json!({"text": "hello http"}))
            .await
            .expect("call_tool");
        assert_eq!(result["content"][0]["text"], "hello http");
        client.ping().await.expect("ping");
        client.shutdown().await;

        let seen = seen.lock().expect("seen");
        assert!(
            seen.initialize_auth,
            "custom Authorization header must arrive"
        );
        assert!(
            seen.post_without_session.is_empty(),
            "non-initialize requests must carry the session id: {:?}",
            seen.post_without_session
        );
        assert!(seen.delete_seen, "shutdown should send DELETE");
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
        // An event with no trailing blank line is dispatched at finish
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
        assert!(matches!(
            HttpTransport::new("t", &config, Duration::from_secs(1)).err(),
            Some(CoreError::McpHeaderBlocked { key }) if key == "content-length"
        ));
        config.headers = HashMap::from([("bad name".to_string(), "v".to_string())]);
        assert!(matches!(
            HttpTransport::new("t", &config, Duration::from_secs(1)).err(),
            Some(CoreError::McpHeaderNameInvalid { .. })
        ));
        config.headers = HashMap::from([("X-Ok".to_string(), "v".to_string())]);
        assert!(HttpTransport::new("t", &config, Duration::from_secs(1)).is_ok());
    }
}
