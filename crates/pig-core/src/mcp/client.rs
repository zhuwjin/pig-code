//! MCP 客户端：JSON-RPC 2.0 会话（initialize 握手 → notifications/initialized →
//! tools/list 分页；运行期 tools/call/ping）。传输层抽象为 [`Transport`]：
//! stdio（子进程 newline JSON-RPC，本文件）与 streamable HTTP（`http` 模块）两种实现，
//! 握手/分页/超时口径在 McpClient 统一。连接断开时全部 pending 请求以错误返回。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::oneshot;

use crate::mcp::config::{McpServerConfig, McpStdioConfig, McpTransport};
use crate::mcp::tool::McpToolAnnotations;

/// initialize 握手的协议版本（stdio）；server 回复不同版本时记录并按其继续
const PROTOCOL_VERSION: &str = "2024-11-05";
/// 单行 JSON-RPC 消息上限（防失控 server 撑爆内存）；超长行丢弃、连接不断
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// stderr 单行上限
const MAX_STDERR_LINE_BYTES: usize = 4 * 1024;
/// stderr 尾部保留量（连接/调用失败时随报错带出）
const STDERR_TAIL_CHARS: usize = 8 * 1024;
/// tools/list 分页上限（防 cursor 死循环）
const MAX_LIST_PAGES: usize = 8;

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// tools/list 通告的单个工具规格
#[derive(Debug, Clone)]
pub struct McpToolSpec {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub annotations: McpToolAnnotations,
}

/// 传输层抽象：收发 JSON-RPC 消息。握手、tools/list 分页、调用超时口径在
/// McpClient 统一实现；各实现自己保证 `request` 并发安全（stdio 靠 AtomicU64 id +
/// Mutex pending map + stdin 写锁多路复用；http 每个请求独立 POST 天然并发）。
enum Transport {
    Stdio(StdioTransport),
    Http(super::http::HttpTransport),
    #[cfg(test)]
    Noop,
}

impl Transport {
    /// 带 id 请求 → 等响应（单次调用超时；超时后迟到的响应丢弃）
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        match self {
            Self::Stdio(t) => t.request(method, params).await,
            Self::Http(t) => t.request(method, params).await,
            #[cfg(test)]
            Self::Noop => Err(format!("noop transport: {method}")),
        }
    }

    /// notifications/*（无 id，不等响应）
    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        match self {
            Self::Stdio(t) => t.notify(method, params).await,
            Self::Http(t) => t.notify(method, params).await,
            #[cfg(test)]
            Self::Noop => Ok(()),
        }
    }

    /// 关闭连接（stdio 杀子进程；http 尽力 DELETE 终止会话）；幂等
    async fn shutdown(&self) {
        match self {
            Self::Stdio(t) => t.shutdown().await,
            Self::Http(t) => t.shutdown().await,
            #[cfg(test)]
            Self::Noop => {}
        }
    }

    /// 诊断尾部（stdio = server stderr 尾部；http 无 → 空串）
    fn diagnostic_tail(&self) -> String {
        match self {
            Self::Stdio(t) => t.diagnostic_tail(),
            Self::Http(_) => String::new(),
            #[cfg(test)]
            Self::Noop => String::new(),
        }
    }

    /// initialize 握手宣告的协议版本（stdio 2024-11-05；http 2025-03-26 起）
    fn protocol_version(&self) -> &'static str {
        match self {
            Self::Stdio(_) => PROTOCOL_VERSION,
            Self::Http(t) => t.protocol_version(),
            #[cfg(test)]
            Self::Noop => PROTOCOL_VERSION,
        }
    }

    /// initialize 完成后告知协商到的版本（http 后续请求要发 MCP-Protocol-Version 头）
    fn notify_negotiated(&self, server_version: &str) {
        if let Self::Http(t) = self {
            t.notify_negotiated(server_version);
        }
    }
}

/// 一个 MCP server 连接（具体传输 + JSON-RPC 会话）
pub struct McpClient {
    name: String,
    transport: Transport,
}

impl McpClient {
    /// 建立连接 + initialize 握手 + tools/list；失败由调用方记录并跳过该 server。
    /// workspace_root 作为 stdio 子进程的工作目录（args 里的相对路径按工作区根解析，
    /// 与 Claude Code 同语义；http 传输不使用）
    pub async fn connect(
        config: &McpServerConfig,
        workspace_root: &Path,
    ) -> Result<(Arc<Self>, Vec<McpToolSpec>), String> {
        let transport = match &config.transport {
            McpTransport::Stdio(stdio) => Transport::Stdio(
                StdioTransport::spawn(&config.name, stdio, config.timeout, workspace_root).await?,
            ),
            McpTransport::Http(http) => Transport::Http(super::http::HttpTransport::new(
                &config.name,
                http,
                config.timeout,
            )?),
        };
        let client = Arc::new(Self {
            name: config.name.clone(),
            transport,
        });
        let announced = client.transport.protocol_version();
        let init = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": announced,
                    "capabilities": {},
                    "clientInfo": {"name": "pig-code", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await
            .map_err(|e| format!("initialize 失败: {e}"))?;
        let server_version = init["protocolVersion"].as_str().unwrap_or(announced);
        if server_version != announced {
            eprintln!(
                "[mcp] {} 协议版本 {server_version}（客户端 {announced}），按 server 版本继续",
                client.name
            );
        }
        client.transport.notify_negotiated(server_version);
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        let tools = client.list_tools().await?;
        Ok((client, tools))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 诊断尾部快照（stdio = stderr 尾部；http = 空）
    pub fn stderr_tail(&self) -> String {
        self.transport.diagnostic_tail()
    }

    /// JSON-RPC ping（探活）
    pub async fn ping(&self) -> Result<(), String> {
        self.request("ping", json!({})).await.map(|_| ())
    }

    /// tools/call：渲染在 tool 适配层，这里透传原始 result。
    /// 并发安全（&self）：stdio 多路复用单一连接（id 唯一 + pending map），
    /// http 每请求独立 POST。
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    /// 关闭连接；stdio 正常 Drop 路径由 spawn 时的 kill_on_drop 兜底
    pub async fn shutdown(&self) {
        self.transport.shutdown().await;
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.transport.request(method, params).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        self.transport.notify(method, params).await
    }

    /// tools/list（跟随 nextCursor 分页，页数封顶防死循环）
    async fn list_tools(&self) -> Result<Vec<McpToolSpec>, String> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self.request("tools/list", params).await?;
            for entry in result["tools"].as_array().cloned().unwrap_or_default() {
                match parse_tool_spec(&entry) {
                    Some(spec) => tools.push(spec),
                    None => eprintln!("[mcp] {} 跳过无法解析的工具条目: {entry}", self.name),
                }
            }
            let Some(next) = result["nextCursor"].as_str() else {
                break;
            };
            pages += 1;
            if pages >= MAX_LIST_PAGES {
                eprintln!(
                    "[mcp] {} tools/list 分页超过 {MAX_LIST_PAGES} 页，截断",
                    self.name
                );
                break;
            }
            cursor = Some(next.to_string());
        }
        Ok(tools)
    }

    /// 测试用假连接：不握手不收发，只提供 name 的占位传输
    #[cfg(test)]
    pub(crate) fn for_test(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            transport: Transport::Noop,
        })
    }
}

/// tools/list 单条目解析：name 必填；inputSchema 缺省补空 object；annotations 缺省全 None
fn parse_tool_spec(entry: &Value) -> Option<McpToolSpec> {
    let name = entry["name"].as_str()?.to_string();
    Some(McpToolSpec {
        name,
        description: entry["description"].as_str().map(str::to_string),
        input_schema: entry
            .get("inputSchema")
            .cloned()
            .filter(|schema| schema.is_object())
            .unwrap_or_else(|| json!({"type": "object"})),
        annotations: serde_json::from_value(entry["annotations"].clone()).unwrap_or_default(),
    })
}

// ---------------- stdio 传输 ----------------

/// stdin 写端（reader 任务回 -32601 也走它）；Arc 与传输解耦生命周期
#[derive(Clone)]
struct Writer {
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
}

impl Writer {
    async fn write_message(&self, message: &Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(message).map_err(|e| format!("序列化消息失败: {e}"))?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&line)
            .await
            .map_err(|e| format!("写入 stdin 失败: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("flush stdin 失败: {e}"))?;
        Ok(())
    }
}

/// stdio 传输：子进程 + newline JSON-RPC。请求多路复用单一连接：
/// id AtomicU64 唯一分配、pending map 按 id 配对、stdin 写互斥，
/// `request` 全部 &self，并发调用安全。
struct StdioTransport {
    name: String,
    timeout: Duration,
    writer: Writer,
    child: tokio::sync::Mutex<Child>,
    pending: PendingMap,
    next_id: AtomicU64,
    closed: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<String>>,
}

/// Windows 程序解析：在目录列表（通常为 PATH）中按「原名 → .exe → .cmd → .bat」
/// 找到真实文件。npm 系工具是 .cmd 垫片，CreateProcess 只自动补 .exe 故直接
/// spawn 裸名会 NotFound；解析为全路径后 std 会经 cmd.exe 启动并转义参数
///（CVE-2024-24576 修复后的行为）。显式带扩展名但文件不存在的不再猜。
#[cfg(windows)]
fn resolve_program(
    search_dirs: &[std::path::PathBuf],
    command: &str,
) -> Option<std::path::PathBuf> {
    use std::path::{Path, PathBuf};

    fn try_variants(base: &Path) -> Option<PathBuf> {
        if base.extension().is_some_and(|ext| !ext.is_empty()) {
            return base.is_file().then(|| base.to_path_buf());
        }
        // 无扩展名：先按 Windows 可执行候选（exe/cmd/bat）。npm/fnm/scoop 的垫片
        // 目录里同名无扩展文件是 POSIX sh 脚本，直接启动会「不是有效的 Win32 应用程序」
        ["exe", "cmd", "bat"]
            .into_iter()
            .map(|ext| base.with_extension(ext))
            .find(|candidate| candidate.is_file())
            .or_else(|| base.is_file().then(|| base.to_path_buf()))
    }

    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    let explicit =
        path.is_absolute() || trimmed.contains('/') || trimmed.contains('\\');
    if explicit {
        return try_variants(&path);
    }
    search_dirs
        .iter()
        .map(|dir| dir.join(trimmed))
        .find_map(|candidate| try_variants(&candidate))
}

impl StdioTransport {
    /// spawn 子进程 + 读写泵；握手由 McpClient 统一做。
    /// 工作目录 = 会话工作区根（相对路径 args 如 `.dbhub/dbhub.toml` 按工作区解析；
    /// 不设置则继承应用进程的启动目录，相对路径会落错位置）
    async fn spawn(
        name: &str,
        config: &McpStdioConfig,
        timeout: Duration,
        workspace_root: &Path,
    ) -> Result<Self, String> {
        // Windows：CreateProcess 只自动补 .exe，npm 系工具（npx/pnpm/bunx…）实为
        // .cmd/.bat 垫片，须按 PATH + 候选扩展解析出真实路径再启动
        #[cfg(windows)]
        let program = {
            let dirs = std::env::var_os("PATH")
                .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
                .unwrap_or_default();
            resolve_program(&dirs, &config.command)
                .unwrap_or_else(|| std::path::PathBuf::from(&config.command))
        };
        #[cfg(not(windows))]
        let program = std::path::PathBuf::from(&config.command);
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&config.args)
            .envs(&config.env)
            .current_dir(workspace_root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| format!("启动失败（{}）: {e}", config.command))?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let transport = Self {
            name: name.to_string(),
            timeout,
            writer: Writer {
                stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            },
            child: tokio::sync::Mutex::new(child),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            closed: closed.clone(),
            stderr_tail: stderr_tail.clone(),
        };
        tokio::spawn(read_loop(
            name.to_string(),
            stdout,
            pending,
            closed,
            transport.writer.clone(),
        ));
        tokio::spawn(stderr_loop(name.to_string(), stderr, stderr_tail));
        Ok(transport)
    }
}

impl StdioTransport {
    /// 带 id 请求：写一行 → 等响应（单次调用超时；超时后迟到的响应被丢弃）
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err(format!("MCP server {} 连接已断开", self.name));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.writer.write_message(&message).await {
            self.pending.lock().expect("pending lock").remove(&id);
            self.closed.store(true, Ordering::Release);
            fail_all(&self.pending);
            return Err(format!("MCP server {} 写入失败: {e}", self.name));
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("MCP server {} 连接已断开", self.name)),
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(format!(
                    "MCP server {} 调用 {method} 超时（{} ms）",
                    self.name,
                    self.timeout.as_millis()
                ))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.writer
            .write_message(&message)
            .await
            .map_err(|e| format!("MCP server {} 通知 {method} 写入失败: {e}", self.name))
    }

    /// 杀子进程并等待回收；正常 Drop 路径由 spawn 时的 kill_on_drop 兜底
    async fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        let _ = child.wait().await;
        fail_all(&self.pending);
    }

    fn diagnostic_tail(&self) -> String {
        self.stderr_tail.lock().expect("stderr tail lock").clone()
    }
}

/// 连接终结：全部 pending 以错误返回（超时后已 remove 的不在内）
fn fail_all(pending: &PendingMap) {
    let drained: Vec<_> = pending.lock().expect("pending lock").drain().collect();
    for (_, tx) in drained {
        let _ = tx.send(Err("MCP server 连接已断开".to_string()));
    }
}

/// stdout 读者：响应配对 pending；server→client 请求统一回 -32601（不支持
/// roots/elicitation，但不能让对端干等）；通知忽略。EOF/出错时 pending 全部失败。
async fn read_loop(
    name: String,
    stdout: ChildStdout,
    pending: PendingMap,
    closed: Arc<AtomicBool>,
    writer: Writer,
) {
    let mut lines = LineReader::new(stdout, MAX_LINE_BYTES, &name);
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => handle_line(&name, &line, &pending, &writer).await,
            Ok(None) => break,
            Err(e) => {
                eprintln!("[mcp] {name} 读取 stdout 失败: {e}");
                break;
            }
        }
    }
    closed.store(true, Ordering::Release);
    fail_all(&pending);
}

async fn handle_line(name: &str, line: &str, pending: &PendingMap, writer: &Writer) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(_) => {
            let preview: String = line.chars().take(120).collect();
            eprintln!("[mcp] {name} 忽略非 JSON 行: {preview}");
            return;
        }
    };
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        if message.get("result").is_some() || message.get("error").is_some() {
            let tx = pending.lock().expect("pending lock").remove(&id);
            let Some(tx) = tx else {
                return;
            };
            let result = match message.get("error") {
                Some(error) => Err(format!(
                    "MCP 错误 {}: {}",
                    error["code"].as_i64().unwrap_or(-1),
                    error["message"].as_str().unwrap_or("未知错误")
                )),
                None => Ok(message["result"].clone()),
            };
            let _ = tx.send(result);
            return;
        }
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let reply = json!({
                "jsonrpc": "2.0",
                "id": message["id"],
                "error": {"code": -32601, "message": format!("pig-code 暂不支持 {method}")},
            });
            let _ = writer.write_message(&reply).await;
        }
    }
    // 其余（notifications/*）忽略
}

/// stderr 读者：逐行透传 eprintln + 保留尾部快照
async fn stderr_loop(name: String, stderr: ChildStderr, tail: Arc<Mutex<String>>) {
    let mut lines = LineReader::new(stderr, MAX_STDERR_LINE_BYTES, &name);
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        eprintln!("[mcp:{name}] {line}");
        let mut tail = tail.lock().expect("stderr tail lock");
        tail.push_str(&line);
        tail.push('\n');
        if tail.len() > STDERR_TAIL_CHARS {
            let excess = tail.len() - STDERR_TAIL_CHARS;
            // 截到下一行首（'\n' 之后必为字符边界）
            let cut = tail.as_bytes()[excess..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| excess + i + 1)
                .unwrap_or(tail.len());
            tail.drain(..cut);
        }
    }
}

/// 按行读取（\n 分隔，容忍 \r\n）；单行超上限丢弃该行并继续（连接不断）
struct LineReader<R> {
    reader: tokio::io::BufReader<R>,
    buf: Vec<u8>,
    discard: bool,
    max_line: usize,
    label: String,
}

impl<R: tokio::io::AsyncRead + Unpin> LineReader<R> {
    fn new(reader: R, max_line: usize, label: &str) -> Self {
        Self {
            reader: tokio::io::BufReader::new(reader),
            buf: Vec::new(),
            discard: false,
            max_line,
            label: label.to_string(),
        }
    }

    async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        let mut chunk = [0u8; 8192];
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
            }
            let n = self.reader.read(&mut chunk).await?;
            if n == 0 {
                return Ok((!self.buf.is_empty()).then(|| {
                    String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned()
                }));
            }
            if self.discard {
                if let Some(pos) = chunk[..n].iter().position(|&b| b == b'\n') {
                    self.discard = false;
                    self.buf.extend_from_slice(&chunk[pos + 1..n]);
                }
                continue;
            }
            self.buf.extend_from_slice(&chunk[..n]);
            if self.buf.len() > self.max_line {
                eprintln!(
                    "[mcp] {} 单行超过 {} KB，丢弃该行",
                    self.label,
                    self.max_line / 1024
                );
                self.buf.clear();
                self.discard = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::McpStdioConfig;
    use crate::tool::Tool as _;
    use std::io::{BufRead as _, Write as _};

    /// 假 server 入口：以 PIG_MCP_FAKE_SERVER=1 重入当前测试二进制时扮演 MCP server
    ///（harness 的 "running 1 test" 横幅会被对端当非 JSON 行忽略）
    #[test]
    fn fake_server_main() {
        if std::env::var_os("PIG_MCP_FAKE_SERVER").is_none() {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(method) = message["method"].as_str() else {
                continue;
            };
            let id = message["id"].clone();
            if id.is_null() {
                continue; // 通知不回
            }
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "0.1"}
                }),
                "ping" => json!({}),
                "tools/list" => json!({"tools": [{
                    "name": "echo",
                    "description": "回显 text 参数",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
                    "annotations": {"readOnlyHint": true, "idempotentHint": true}
                }]}),
                "tools/call" => json!({"content": [{
                    "type": "text",
                    "text": message["params"]["arguments"]["text"].as_str().unwrap_or("")
                }]}),
                _ => {
                    let reply = json!({"jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "no such method"}});
                    writeln!(stdout, "{reply}").expect("write");
                    stdout.flush().expect("flush");
                    continue;
                }
            };
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
            writeln!(stdout, "{reply}").expect("write");
            stdout.flush().expect("flush");
        }
        std::process::exit(0);
    }

    fn fake_server_config() -> McpServerConfig {
        let exe = std::env::current_exe().expect("current_exe");
        McpServerConfig {
            name: "fake".to_string(),
            transport: McpTransport::Stdio(McpStdioConfig {
                command: exe.to_string_lossy().into_owned(),
                // --nocapture 必需：否则 harness 劫持 println，父进程看不到响应
                args: vec![
                    "mcp::client::tests::fake_server_main".to_string(),
                    "--exact".to_string(),
                    "--nocapture".to_string(),
                ],
                env: HashMap::from([("PIG_MCP_FAKE_SERVER".to_string(), "1".to_string())]),
            }),
            timeout: Duration::from_secs(10),
            disabled: false,
        }
    }

    /// 端到端：真子进程 stdio 握手 → tools/list → tools/call → ping → shutdown
    #[tokio::test]
    async fn stdio_echo_server_roundtrip() {
        let (client, specs) = McpClient::connect(&fake_server_config(), std::path::Path::new("."))
            .await
            .expect("connect");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
        assert!(specs[0].annotations.is_read_only());

        let result = client
            .call_tool("echo", json!({"text": "你好 mcp"}))
            .await
            .expect("call_tool");
        assert_eq!(result["content"][0]["text"], "你好 mcp");

        client.ping().await.expect("ping");

        // Tool 适配层：命名 / read_only / schema
        let tool = crate::mcp::tool::McpTool::new("fake", specs[0].clone(), client.clone());
        assert_eq!(tool.name(), "mcp__fake__echo");
        assert!(tool.read_only());
        assert_eq!(tool.schema()["function"]["name"], "mcp__fake__echo");

        client.shutdown().await;
    }

    /// Windows 程序解析：.cmd 垫片 / .exe / 显式路径补扩展 / 裸名搜目录列表
    #[cfg(windows)]
    #[test]
    fn resolve_program_finds_cmd_shim_and_exe() {
        let dir = std::env::temp_dir().join(format!("pig-mcp-prog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // npm/fnm 式垫片目录：同名无扩展 sh 脚本 + .cmd 垫片并存，必须选 .cmd
        std::fs::write(dir.join("npx"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("npx.cmd"), "@echo off\r\n").unwrap();
        std::fs::write(dir.join("tool.exe"), b"").unwrap();
        let dirs = vec![dir.clone()];

        // 裸名按 exe→cmd→bat 优先于无扩展原名解析
        assert_eq!(resolve_program(&dirs, "npx"), Some(dir.join("npx.cmd")));
        assert_eq!(resolve_program(&dirs, "tool"), Some(dir.join("tool.exe")));
        assert_eq!(resolve_program(&dirs, "missing"), None);
        assert_eq!(resolve_program(&dirs, "  "), None);

        // 显式路径同样补扩展；带扩展名但不存在则不猜
        let bare = dir.join("npx").to_string_lossy().into_owned();
        assert_eq!(resolve_program(&dirs, &bare), Some(dir.join("npx.cmd")));
        let full = dir.join("tool.exe").to_string_lossy().into_owned();
        assert_eq!(
            resolve_program(&dirs, &full),
            Some(dir.join("tool.exe"))
        );
        let absent = dir.join("nope.exe").to_string_lossy().into_owned();
        assert_eq!(resolve_program(&dirs, &absent), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 并发安全核实：同一连接上并发 tools/call 多路复用——
    /// 两个并发调用各自拿到与自己参数匹配的回显（id 配对不错串）
    #[tokio::test]
    async fn stdio_concurrent_calls_multiplexed() {
        let (client, _) = McpClient::connect(&fake_server_config(), std::path::Path::new("."))
            .await
            .expect("connect");
        let (a, b) = tokio::join!(
            client.call_tool("echo", json!({"text": "甲"})),
            client.call_tool("echo", json!({"text": "乙"})),
        );
        assert_eq!(a.expect("call a")["content"][0]["text"], "甲");
        assert_eq!(b.expect("call b")["content"][0]["text"], "乙");
        client.shutdown().await;
    }
}
