//! MCP client: a JSON-RPC 2.0 session (initialize handshake →
//! notifications/initialized → tools/list pagination; tools/call/ping at
//! runtime). The transport layer is abstracted as [`Transport`]: two
//! implementations, stdio (child process newline JSON-RPC, this file) and
//! streamable HTTP (the `http` module); the handshake/pagination/timeout
//! conventions are unified in McpClient. When the connection drops, all
//! pending requests return with an error.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::oneshot;

use crate::NoConsoleExt as _;
use crate::mcp::config::{McpServerConfig, McpStdioConfig, McpTransport};
use crate::mcp::tool::McpToolAnnotations;
use pig_protocol::CoreError;

/// Protocol version for the initialize handshake (stdio); if the server replies with a different version, log it and continue with the server's
const PROTOCOL_VERSION: &str = "2024-11-05";
/// Cap on a single-line JSON-RPC message (guards against a runaway server bloating memory); overlong lines are discarded while the connection stays up
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Per-line cap for stderr
const MAX_STDERR_LINE_BYTES: usize = 4 * 1024;
/// Retained stderr tail (attached to the error when connection/calls fail)
const STDERR_TAIL_CHARS: usize = 8 * 1024;
/// tools/list pagination cap (guards against a cursor infinite loop)
const MAX_LIST_PAGES: usize = 8;

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// A single tool spec advertised by tools/list
#[derive(Debug, Clone)]
pub struct McpToolSpec {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub annotations: McpToolAnnotations,
}

/// Transport abstraction: send and receive JSON-RPC messages. The handshake,
/// tools/list pagination, and call timeout conventions are implemented once in
/// McpClient; each implementation guarantees `request` concurrency safety
/// itself (stdio multiplexes via AtomicU64 id + Mutex pending map + stdin
/// write lock; http is naturally concurrent with one independent POST per
/// request).
enum Transport {
    Stdio(StdioTransport),
    Http(super::http::HttpTransport),
    #[cfg(test)]
    Noop,
}

impl Transport {
    /// Request with an id → await the response (per-call timeout; a late response after timeout is discarded)
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        match self {
            Self::Stdio(t) => t.request(method, params).await,
            Self::Http(t) => t.request(method, params).await,
            #[cfg(test)]
            Self::Noop => Err(format!("noop transport: {method}")),
        }
    }

    /// notifications/* (no id, no response awaited); failures are structured errors (the connection boundary belongs to the MCP class)
    async fn notify(&self, method: &str, params: Value) -> Result<(), CoreError> {
        match self {
            Self::Stdio(t) => t.notify(method, params).await,
            Self::Http(t) => t.notify(method, params).await,
            #[cfg(test)]
            Self::Noop => Ok(()),
        }
    }

    /// Close the connection (stdio kills the child process; http tries a DELETE to terminate the session); idempotent
    async fn shutdown(&self) {
        match self {
            Self::Stdio(t) => t.shutdown().await,
            Self::Http(t) => t.shutdown().await,
            #[cfg(test)]
            Self::Noop => {}
        }
    }

    /// Diagnostic tail (stdio = the server stderr tail; http has none → empty string)
    fn diagnostic_tail(&self) -> String {
        match self {
            Self::Stdio(t) => t.diagnostic_tail(),
            Self::Http(_) => String::new(),
            #[cfg(test)]
            Self::Noop => String::new(),
        }
    }

    /// Protocol version announced in the initialize handshake (stdio 2024-11-05; http 2025-03-26 onward)
    fn protocol_version(&self) -> &'static str {
        match self {
            Self::Stdio(_) => PROTOCOL_VERSION,
            Self::Http(t) => t.protocol_version(),
            #[cfg(test)]
            Self::Noop => PROTOCOL_VERSION,
        }
    }

    /// Tell the transport the negotiated version once initialize completes (http sends the MCP-Protocol-Version header on subsequent requests)
    fn notify_negotiated(&self, server_version: &str) {
        if let Self::Http(t) = self {
            t.notify_negotiated(server_version);
        }
    }
}

/// One MCP server connection (concrete transport + JSON-RPC session)
pub struct McpClient {
    name: String,
    transport: Transport,
}

impl McpClient {
    /// Establish the connection + initialize handshake + tools/list; on
    /// failure the caller logs and skips this server. Errors are classified as
    /// CoreError (connection boundary: carried directly in the state snapshot,
    /// localized by the UI per kind). workspace_root serves as the stdio child
    /// process's working directory (relative paths in args resolve against the
    /// workspace root, same semantics as Claude Code; unused by the http
    /// transport)
    pub async fn connect(
        config: &McpServerConfig,
        workspace_root: &Path,
    ) -> Result<(Arc<Self>, Vec<McpToolSpec>), CoreError> {
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
            .map_err(|e| CoreError::McpInitialize { detail: e })?;
        let server_version = init["protocolVersion"].as_str().unwrap_or(announced);
        if server_version != announced {
            eprintln!(
                "[mcp] {} protocol version {server_version} (client {announced}), continuing with server version",
                client.name
            );
        }
        client.transport.notify_negotiated(server_version);
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        // tools/list is done together with the handshake: failures belong to the same connection boundary, classified as initialize
        let tools = client
            .list_tools()
            .await
            .map_err(|e| CoreError::McpInitialize { detail: e })?;
        Ok((client, tools))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Diagnostic tail snapshot (stdio = stderr tail; http = empty)
    pub fn stderr_tail(&self) -> String {
        self.transport.diagnostic_tail()
    }

    /// JSON-RPC ping (liveness probe)
    pub async fn ping(&self) -> Result<(), String> {
        self.request("ping", json!({})).await.map(|_| ())
    }

    /// tools/call: rendering happens in the tool adapter layer; the raw result
    /// is passed through here. Concurrency-safe (&self): stdio multiplexes a
    /// single connection (unique id + pending map); http does one independent
    /// POST per request.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
            .await
    }

    /// Close the connection; the stdio normal Drop path is covered by kill_on_drop set at spawn
    pub async fn shutdown(&self) {
        self.transport.shutdown().await;
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.transport.request(method, params).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), CoreError> {
        self.transport.notify(method, params).await
    }

    /// tools/list (follows nextCursor pagination, with a page cap to prevent an infinite loop)
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
                    None => eprintln!(
                        "[mcp] {} skipping unparsable tool entry: {entry}",
                        self.name
                    ),
                }
            }
            let Some(next) = result["nextCursor"].as_str() else {
                break;
            };
            pages += 1;
            if pages >= MAX_LIST_PAGES {
                eprintln!(
                    "[mcp] {} tools/list paging exceeded {MAX_LIST_PAGES} pages, truncating",
                    self.name
                );
                break;
            }
            cursor = Some(next.to_string());
        }
        Ok(tools)
    }

    /// Fake connection for tests: no handshake, no traffic; a placeholder transport providing only the name
    #[cfg(test)]
    pub(crate) fn for_test(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            transport: Transport::Noop,
        })
    }
}

/// Parse one tools/list entry: name is required; a missing inputSchema defaults to an empty object; missing annotations default to all None
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

// ---------------- stdio transport ----------------

/// stdin write side (the reader task's -32601 replies go through it too); Arc decouples its lifetime from the transport
#[derive(Clone)]
struct Writer {
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
}

impl Writer {
    async fn write_message(&self, message: &Value) -> Result<(), String> {
        let mut line =
            serde_json::to_vec(message).map_err(|e| format!("Failed to serialize message: {e}"))?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(&line)
            .await
            .map_err(|e| format!("Failed to write to stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("Failed to flush stdin: {e}"))?;
        Ok(())
    }
}

/// stdio transport: child process + newline JSON-RPC. Requests multiplex a
/// single connection: id allocated uniquely via AtomicU64, pending map paired
/// by id, stdin writes mutually exclusive; `request` is all &self, safe for
/// concurrent calls.
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

/// Windows program resolution: find the real file in a directory list (usually
/// PATH) as "bare name → .exe → .cmd → .bat". npm-family tools are .cmd
/// shims; CreateProcess only appends .exe automatically, so spawning the bare
/// name directly yields NotFound; once resolved to a full path, std launches
/// it via cmd.exe and escapes arguments (behavior after the CVE-2024-24576
/// fix). An explicit extension whose file does not exist is not guessed
/// further.
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
        // No extension: try Windows executable candidates first (exe/cmd/bat).
        // In npm/fnm/scoop shim directories the extensionless file of the same
        // name is a POSIX sh script; launching it directly gives "not a valid
        // Win32 application"
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
    let explicit = path.is_absolute() || trimmed.contains('/') || trimmed.contains('\\');
    if explicit {
        return try_variants(&path);
    }
    search_dirs
        .iter()
        .map(|dir| dir.join(trimmed))
        .find_map(|candidate| try_variants(&candidate))
}

impl StdioTransport {
    /// Spawn the child process + read/write pumps; the handshake is done
    /// uniformly by McpClient. Working directory = the session workspace root
    /// (relative-path args such as `.dbhub/dbhub.toml` resolve against the
    /// workspace; without setting it the app process's startup directory is
    /// inherited and relative paths land in the wrong place)
    async fn spawn(
        name: &str,
        config: &McpStdioConfig,
        timeout: Duration,
        workspace_root: &Path,
    ) -> Result<Self, CoreError> {
        // Windows: CreateProcess only appends .exe automatically; npm-family
        // tools (npx/pnpm/bunx...) are actually .cmd/.bat shims, so resolve the
        // real path via PATH + candidate extensions before launching
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
        command.no_console();
        command
            .args(&config.args)
            .envs(&config.env)
            .current_dir(workspace_root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| CoreError::McpSpawn {
            command: config.command.clone(),
            detail: e.to_string(),
        })?;
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
    /// Request with an id: write one line → await the response (per-call timeout; a late response after timeout is discarded)
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err(format!("MCP server {} connection is closed", self.name));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.writer.write_message(&message).await {
            self.pending.lock().expect("pending lock").remove(&id);
            self.closed.store(true, Ordering::Release);
            fail_all(&self.pending);
            return Err(format!("MCP server {} write failed: {e}", self.name));
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("MCP server {} connection is closed", self.name)),
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(format!(
                    "MCP server {} call to {method} timed out after {} ms",
                    self.name,
                    self.timeout.as_millis()
                ))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), CoreError> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.writer
            .write_message(&message)
            .await
            .map_err(|e| CoreError::McpNotifyWrite {
                name: self.name.clone(),
                method: method.to_string(),
                detail: e,
            })
    }

    /// Kill the child process and wait for reaping; the normal Drop path is covered by kill_on_drop set at spawn
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

/// Connection terminated: all pending return with an error (entries removed after timeout are not included)
fn fail_all(pending: &PendingMap) {
    let drained: Vec<_> = pending.lock().expect("pending lock").drain().collect();
    for (_, tx) in drained {
        let _ = tx.send(Err("MCP server connection is closed".to_string()));
    }
}

/// stdout reader: match responses against pending; server→client requests
/// uniformly get -32601 (roots/elicitation unsupported, but the peer must not
/// wait forever); notifications are ignored. On EOF/error, all pending fail.
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
                eprintln!("[mcp] {name} failed to read stdout: {e}");
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
            eprintln!("[mcp] {name} ignoring non-JSON line: {preview}");
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
                    "MCP error {}: {}",
                    error["code"].as_i64().unwrap_or(-1),
                    error["message"].as_str().unwrap_or("unknown error")
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
                "error": {"code": -32601, "message": format!("pig-code does not support {method}")},
            });
            let _ = writer.write_message(&reply).await;
        }
    }
    // The rest (notifications/*) are ignored
}

/// stderr reader: forward each line to eprintln + keep a tail snapshot
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
            // Cut to the start of the next line (right after '\n' is always a char boundary)
            let cut = tail.as_bytes()[excess..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| excess + i + 1)
                .unwrap_or(tail.len());
            tail.drain(..cut);
        }
    }
}

/// Line-oriented reading (\n separated, \r\n tolerated); a line over the cap is discarded and reading continues (the connection stays up)
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
                    "[mcp] {} line exceeds {} KB, discarding the line",
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

    /// Fake server entry: re-enters the current test binary with
    /// PIG_MCP_FAKE_SERVER=1 to act as an MCP server (the harness's "running 1
    /// test" banner is ignored by the peer as a non-JSON line)
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
                continue; // No reply to notifications
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
                    "description": "Echoes the text parameter",
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
                // --nocapture is required: otherwise the harness hijacks
                // println and the parent process never sees responses
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

    /// End to end: real child process stdio handshake → tools/list → tools/call → ping → shutdown
    #[tokio::test]
    async fn stdio_echo_server_roundtrip() {
        let (client, specs) = McpClient::connect(&fake_server_config(), std::path::Path::new("."))
            .await
            .expect("connect");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
        assert!(specs[0].annotations.is_read_only());

        let result = client
            .call_tool("echo", json!({"text": "hello mcp"}))
            .await
            .expect("call_tool");
        assert_eq!(result["content"][0]["text"], "hello mcp");

        client.ping().await.expect("ping");

        // Tool adapter layer: naming / read_only / schema
        let tool = crate::mcp::tool::McpTool::new("fake", specs[0].clone(), client.clone());
        assert_eq!(tool.name(), "mcp__fake__echo");
        assert!(tool.read_only());
        assert_eq!(tool.schema()["function"]["name"], "mcp__fake__echo");

        client.shutdown().await;
    }

    /// Windows program resolution: .cmd shims / .exe / explicit paths with extension appended / bare names searched in the directory list
    #[cfg(windows)]
    #[test]
    fn resolve_program_finds_cmd_shim_and_exe() {
        let dir = std::env::temp_dir().join(format!("pig-mcp-prog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // An npm/fnm-style shim directory: an extensionless sh script and a
        // .cmd shim coexist; the .cmd must be chosen
        std::fs::write(dir.join("npx"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("npx.cmd"), "@echo off\r\n").unwrap();
        std::fs::write(dir.join("tool.exe"), b"").unwrap();
        let dirs = vec![dir.clone()];

        // Bare names resolve as exe→cmd→bat, ahead of the extensionless original name
        assert_eq!(resolve_program(&dirs, "npx"), Some(dir.join("npx.cmd")));
        assert_eq!(resolve_program(&dirs, "tool"), Some(dir.join("tool.exe")));
        assert_eq!(resolve_program(&dirs, "missing"), None);
        assert_eq!(resolve_program(&dirs, "  "), None);

        // Explicit paths also get extensions appended; an explicit extension
        // whose file is absent is not guessed
        let bare = dir.join("npx").to_string_lossy().into_owned();
        assert_eq!(resolve_program(&dirs, &bare), Some(dir.join("npx.cmd")));
        let full = dir.join("tool.exe").to_string_lossy().into_owned();
        assert_eq!(resolve_program(&dirs, &full), Some(dir.join("tool.exe")));
        let absent = dir.join("nope.exe").to_string_lossy().into_owned();
        assert_eq!(resolve_program(&dirs, &absent), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Concurrency safety check: concurrent tools/call multiplexed on the same
    /// connection — two concurrent calls each get the echo matching their own
    /// arguments (id pairing does not cross)
    #[tokio::test]
    async fn stdio_concurrent_calls_multiplexed() {
        let (client, _) = McpClient::connect(&fake_server_config(), std::path::Path::new("."))
            .await
            .expect("connect");
        let (a, b) = tokio::join!(
            client.call_tool("echo", json!({"text": "first"})),
            client.call_tool("echo", json!({"text": "second"})),
        );
        assert_eq!(a.expect("call a")["content"][0]["text"], "first");
        assert_eq!(b.expect("call b")["content"][0]["text"], "second");
        client.shutdown().await;
    }
}
