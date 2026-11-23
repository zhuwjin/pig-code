//! MCP client: a JSON-RPC 2.0 session (initialize handshake →
//! notifications/initialized → tools/list pagination; tools/call/ping at
//! runtime). The transport layer is abstracted as [`Transport`]: two
//! implementations, stdio (child process newline JSON-RPC, this file) and
//! streamable HTTP (the `http` module); the handshake/pagination/timeout
//! conventions are unified in McpClient. When the connection drops, all
//! pending requests return with an error.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::oneshot;

use crate::NoConsoleExt as _;
use crate::mcp::capabilities::{
    ElicitationHandler, McpConnectionState, McpPromptResult, McpPromptSpec, McpResourceContent,
    McpResourceSpec, McpResourceTemplate, McpServerCapabilities, handle_server_request,
};
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

/// One MCP server connection (concrete transport + JSON-RPC session + reconnection state machine)
pub struct McpClient {
    name: String,
    config: Option<McpServerConfig>,
    workspace_root: PathBuf,
    elicitation: Option<ElicitationHandler>,
    transport: tokio::sync::RwLock<Transport>,
    generation: AtomicU64,
    state: Mutex<McpConnectionState>,
    capabilities: Mutex<McpServerCapabilities>,
    reconnect_lock: tokio::sync::Mutex<()>,
    shut_down: AtomicBool,
}

fn is_reconnectable_error(error: &str) -> bool {
    error.contains("connection is closed")
        || error.contains("write failed")
        || error.contains("SSE stream ended")
        || error.contains("HTTP 404")
        || error.contains("request failed")
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
        Self::connect_with_elicitation(config, workspace_root, None).await
    }

    /// Connect with an optional handler for server-initiated `elicitation/create` requests.
    pub async fn connect_with_elicitation(
        config: &McpServerConfig,
        workspace_root: &Path,
        elicitation: Option<ElicitationHandler>,
    ) -> Result<(Arc<Self>, Vec<McpToolSpec>), CoreError> {
        let (transport, capabilities) =
            Self::open_and_initialize(config, workspace_root, elicitation.clone()).await?;
        let client = Arc::new(Self {
            name: config.name.clone(),
            config: Some(config.clone()),
            workspace_root: workspace_root.to_path_buf(),
            elicitation,
            transport: tokio::sync::RwLock::new(transport),
            generation: AtomicU64::new(1),
            state: Mutex::new(McpConnectionState::Connected),
            capabilities: Mutex::new(capabilities),
            reconnect_lock: tokio::sync::Mutex::new(()),
            shut_down: AtomicBool::new(false),
        });
        // tools/list is done together with the handshake: failures belong to the same connection boundary, classified as initialize
        let tools = client
            .list_tools()
            .await
            .map_err(|e| CoreError::McpInitialize { detail: e })?;
        Ok((client, tools))
    }

    async fn open_and_initialize(
        config: &McpServerConfig,
        workspace_root: &Path,
        elicitation: Option<ElicitationHandler>,
    ) -> Result<(Transport, McpServerCapabilities), CoreError> {
        let transport = match &config.transport {
            McpTransport::Stdio(stdio) => Transport::Stdio(
                StdioTransport::spawn(
                    &config.name,
                    stdio,
                    config.timeout,
                    workspace_root,
                    elicitation.clone(),
                )
                .await?,
            ),
            McpTransport::Http(http) => Transport::Http(super::http::HttpTransport::new(
                &config.name,
                http,
                config.timeout,
                elicitation.clone(),
            )?),
        };
        let announced = transport.protocol_version();
        let client_capabilities = if elicitation.is_some() {
            json!({ "elicitation": {} })
        } else {
            json!({})
        };
        let init = transport
            .request(
                "initialize",
                json!({
                    "protocolVersion": announced,
                    "capabilities": client_capabilities,
                    "clientInfo": {"name": "pig-code", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await
            .map_err(|e| CoreError::McpInitialize { detail: e })?;
        let server_version = init["protocolVersion"].as_str().unwrap_or(announced);
        if server_version != announced {
            tracing::warn!(
                "{} protocol version {server_version} (client {announced}), continuing with server version",
                config.name
            );
        }
        let capabilities = McpServerCapabilities::from_initialize_result(&init);
        transport.notify_negotiated(server_version);
        transport.notify("notifications/initialized", json!({})).await?;
        Ok((transport, capabilities))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Negotiated server capabilities from the latest `initialize` handshake.
    pub fn capabilities(&self) -> McpServerCapabilities {
        *self
            .capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Current connection lifecycle state.
    pub fn connection_state(&self) -> McpConnectionState {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Explicitly tear down and re-establish the underlying transport + MCP handshake.
    pub async fn reconnect(&self) -> Result<(), CoreError> {
        let gen_before = self.generation.load(Ordering::Acquire);
        self.reconnect_if_needed(gen_before)
            .await
            .map_err(|detail| CoreError::McpInitialize { detail })
    }

    async fn reconnect_if_needed(&self, failed_generation: u64) -> Result<(), String> {
        if self.shut_down.load(Ordering::Acquire) {
            return Err(format!("MCP server {} was shut down", self.name));
        }
        let Some(config) = &self.config else {
            return Err(format!("MCP server {} cannot reconnect (no config)", self.name));
        };
        let _guard = self.reconnect_lock.lock().await;
        if self.shut_down.load(Ordering::Acquire) {
            return Err(format!("MCP server {} was shut down", self.name));
        }
        // Single-flight deduplication: if a concurrent call already reconnected
        // while we waited for the lock, reuse the new transport immediately.
        if self.generation.load(Ordering::Acquire) != failed_generation {
            return Ok(());
        }
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            McpConnectionState::Reconnecting { attempt: 1 };
        tracing::warn!(server = %self.name, "MCP transport dropped; attempting single-flight reconnect");

        // Close old transport before spawning/connecting the replacement.
        {
            let old = self.transport.read().await;
            old.shutdown().await;
        }

        match Self::open_and_initialize(config, &self.workspace_root, self.elicitation.clone())
            .await
        {
            Ok((fresh, caps)) => {
                *self.transport.write().await = fresh;
                *self
                    .capabilities
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = caps;
                self.generation.fetch_add(1, Ordering::Release);
                *self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    McpConnectionState::Connected;
                Ok(())
            }
            Err(error) => {
                let reason = format!("{error:?}");
                *self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    McpConnectionState::Disconnected {
                        reason: reason.clone(),
                    };
                Err(format!("MCP server {} reconnect failed: {reason}", self.name))
            }
        }
    }

    /// Diagnostic tail snapshot (stdio = stderr tail; http = empty)
    pub fn stderr_tail(&self) -> String {
        self.transport
            .try_read()
            .map(|t| t.diagnostic_tail())
            .unwrap_or_default()
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

    /// `resources/list` (paginated via `nextCursor` up to `MAX_LIST_PAGES`).
    pub async fn list_resources(&self) -> Result<Vec<McpResourceSpec>, String> {
        let mut resources = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self.request("resources/list", params).await?;
            for entry in result["resources"].as_array().cloned().unwrap_or_default() {
                if let Ok(spec) = serde_json::from_value::<McpResourceSpec>(entry) {
                    resources.push(spec);
                }
            }
            let Some(next) = result["nextCursor"].as_str() else {
                break;
            };
            pages += 1;
            if pages >= MAX_LIST_PAGES {
                break;
            }
            cursor = Some(next.to_string());
        }
        Ok(resources)
    }

    /// `resources/templates/list` (paginated via `nextCursor` up to `MAX_LIST_PAGES`).
    pub async fn list_resource_templates(&self) -> Result<Vec<McpResourceTemplate>, String> {
        let mut templates = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self.request("resources/templates/list", params).await?;
            for entry in result["resourceTemplates"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                if let Ok(tpl) = serde_json::from_value::<McpResourceTemplate>(entry) {
                    templates.push(tpl);
                }
            }
            let Some(next) = result["nextCursor"].as_str() else {
                break;
            };
            pages += 1;
            if pages >= MAX_LIST_PAGES {
                break;
            }
            cursor = Some(next.to_string());
        }
        Ok(templates)
    }

    /// `resources/read` for one URI.
    pub async fn read_resource(&self, uri: &str) -> Result<Vec<McpResourceContent>, String> {
        let result = self.request("resources/read", json!({"uri": uri})).await?;
        let mut contents = Vec::new();
        for entry in result["contents"].as_array().cloned().unwrap_or_default() {
            if let Ok(content) = serde_json::from_value::<McpResourceContent>(entry) {
                contents.push(content);
            }
        }
        Ok(contents)
    }

    /// `prompts/list` (paginated via `nextCursor` up to `MAX_LIST_PAGES`).
    pub async fn list_prompts(&self) -> Result<Vec<McpPromptSpec>, String> {
        let mut prompts = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let result = self.request("prompts/list", params).await?;
            for entry in result["prompts"].as_array().cloned().unwrap_or_default() {
                if let Ok(spec) = serde_json::from_value::<McpPromptSpec>(entry) {
                    prompts.push(spec);
                }
            }
            let Some(next) = result["nextCursor"].as_str() else {
                break;
            };
            pages += 1;
            if pages >= MAX_LIST_PAGES {
                break;
            }
            cursor = Some(next.to_string());
        }
        Ok(prompts)
    }

    /// `prompts/get` for one named prompt and optional arguments map.
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, Value>>,
    ) -> Result<McpPromptResult, String> {
        let params = match arguments {
            Some(args) => json!({"name": name, "arguments": Value::Object(args)}),
            None => json!({"name": name}),
        };
        let result = self.request("prompts/get", params).await?;
        serde_json::from_value(result)
            .map_err(|e| format!("MCP server {} returned invalid prompts/get result: {e}", self.name))
    }

    /// Close the connection; the stdio normal Drop path is covered by kill_on_drop set at spawn
    pub async fn shutdown(&self) {
        self.shut_down.store(true, Ordering::Release);
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            McpConnectionState::Disconnected {
                reason: "shut down".to_string(),
            };
        self.transport.read().await.shutdown().await;
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let gen_before = self.generation.load(Ordering::Acquire);
        let first = {
            let transport = self.transport.read().await;
            transport.request(method, params.clone()).await
        };
        match first {
            Ok(value) => Ok(value),
            Err(error)
                if is_reconnectable_error(&error)
                    && !self.shut_down.load(Ordering::Acquire)
                    && self.config.is_some() =>
            {
                if let Err(reconnect_err) = self.reconnect_if_needed(gen_before).await {
                    return Err(format!("{error}; {reconnect_err}"));
                }
                let transport = self.transport.read().await;
                transport.request(method, params).await
            }
            Err(error) => Err(error),
        }
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
                    None => tracing::warn!("{} skipping unparsable tool entry: {entry}", self.name),
                }
            }
            let Some(next) = result["nextCursor"].as_str() else {
                break;
            };
            pages += 1;
            if pages >= MAX_LIST_PAGES {
                tracing::warn!(
                    "{} tools/list paging exceeded {MAX_LIST_PAGES} pages, truncating",
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
            config: None,
            workspace_root: PathBuf::from("."),
            elicitation: None,
            transport: tokio::sync::RwLock::new(Transport::Noop),
            generation: AtomicU64::new(1),
            state: Mutex::new(McpConnectionState::Connected),
            capabilities: Mutex::new(McpServerCapabilities::default()),
            reconnect_lock: tokio::sync::Mutex::new(()),
            shut_down: AtomicBool::new(false),
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
    child: tokio::sync::Mutex<crate::task::ManagedChild>,
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
        elicitation: Option<ElicitationHandler>,
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
        let mut child =
            crate::task::spawn_managed(&mut command).map_err(|e| CoreError::McpSpawn {
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
            elicitation,
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
        let process = child.process();
        let termination = process.terminate();
        let waited = tokio::time::timeout(Duration::from_secs(5), async {
            child.wait().await.map_err(|error| error.to_string())?;
            process.wait_empty().await
        })
        .await;
        match (termination, waited) {
            (Ok(()), Ok(Ok(()))) => {}
            (Err(error), _) => {
                tracing::warn!(server = self.name, %error, "MCP process-tree termination failed")
            }
            (_, Ok(Err(error))) => {
                tracing::warn!(server = self.name, %error, "MCP process-tree wait failed")
            }
            (_, Err(_)) => {
                tracing::warn!(server = self.name, "MCP process-tree shutdown timed out")
            }
        }
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
    elicitation: Option<ElicitationHandler>,
) {
    let mut lines = LineReader::new(stdout, MAX_LINE_BYTES, &name);
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                handle_line(&name, &line, &pending, &writer, elicitation.as_ref()).await
            }
            Ok(None) => break,
            Err(e) => {
                tracing::warn!("{name} failed to read stdout: {e}");
                break;
            }
        }
    }
    closed.store(true, Ordering::Release);
    fail_all(&pending);
}

async fn handle_line(
    name: &str,
    line: &str,
    pending: &PendingMap,
    writer: &Writer,
    elicitation: Option<&ElicitationHandler>,
) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(_) => {
            let preview: String = line.chars().take(120).collect();
            tracing::warn!("{name} ignoring non-JSON line: {preview}");
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
            let server_name = name.to_string();
            let req_id = message["id"].clone();
            let method = method.to_string();
            let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
            let writer = writer.clone();
            let handler = elicitation.cloned();
            // Spawn so slow interactive elicitation never stalls reading
            // subsequent JSON-RPC responses on stdout.
            tokio::spawn(async move {
                let reply = handle_server_request(
                    &server_name,
                    req_id,
                    &method,
                    &params,
                    handler.as_ref(),
                )
                .await;
                let _ = writer.write_message(&reply).await;
            });
        }
    }
    // The rest (notifications/*) are ignored
}

/// stderr reader: forward each line to the debug log + keep a tail snapshot
async fn stderr_loop(name: String, stderr: ChildStderr, tail: Arc<Mutex<String>>) {
    let mut lines = LineReader::new(stderr, MAX_STDERR_LINE_BYTES, &name);
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        tracing::debug!("[mcp:{name}] {line}");
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
                tracing::warn!(
                    "{} line exceeds {} KB, discarding the line",
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
        let mut stdin_lines = stdin.lock().lines();
        while let Some(Ok(line)) = stdin_lines.next() {
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
                    "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                    "serverInfo": {"name": "fake", "version": "0.1"}
                }),
                "ping" => json!({}),
                "tools/list" => json!({"tools": [{
                    "name": "echo",
                    "description": "Echoes the text parameter",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
                    "annotations": {"readOnlyHint": true, "idempotentHint": true}
                }, {
                    "name": "ask_user",
                    "description": "Triggers elicitation/create before replying",
                    "inputSchema": {"type": "object"},
                    "annotations": {"readOnlyHint": true}
                }, {
                    "name": "crash_once",
                    "description": "Exits immediately to test reconnection",
                    "inputSchema": {"type": "object"},
                    "annotations": {"readOnlyHint": true}
                }]}),
                "tools/call" => {
                    let tool = message["params"]["name"].as_str().unwrap_or("");
                    if tool == "crash_once" {
                        std::process::exit(0);
                    } else if tool == "ask_user" {
                        let req = json!({
                            "jsonrpc": "2.0",
                            "id": 999,
                            "method": "elicitation/create",
                            "params": {
                                "message": "Provide token label",
                                "requestedSchema": {
                                    "type": "object",
                                    "properties": {"label": {"type": "string"}}
                                }
                            }
                        });
                        writeln!(stdout, "{req}").expect("write elicitation");
                        stdout.flush().expect("flush");
                        let reply_line = stdin_lines
                            .next()
                            .and_then(Result::ok)
                            .unwrap_or_default();
                        let reply_json: Value =
                            serde_json::from_str(&reply_line).unwrap_or_default();
                        let label = reply_json["result"]["content"]["label"]
                            .as_str()
                            .unwrap_or("none");
                        let action = reply_json["result"]["action"]
                            .as_str()
                            .unwrap_or("none");
                        json!({"content": [{"type": "text", "text": format!("{action}:{label}")}]})
                    } else {
                        json!({"content": [{
                            "type": "text",
                            "text": message["params"]["arguments"]["text"].as_str().unwrap_or("")
                        }]})
                    }
                }
                "resources/list" => json!({"resources": [{
                    "uri": "file:///README.md",
                    "name": "README",
                    "description": "Project readme",
                    "mimeType": "text/markdown"
                }]}),
                "resources/read" => json!({"contents": [{
                    "uri": message["params"]["uri"].as_str().unwrap_or(""),
                    "mimeType": "text/markdown",
                    "text": "# Hello from MCP resource"
                }]}),
                "prompts/list" => json!({"prompts": [{
                    "name": "summarize",
                    "description": "Summarize input",
                    "arguments": [{"name": "topic", "required": true}]
                }]}),
                "prompts/get" => json!({
                    "description": "Summarize prompt",
                    "messages": [{
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": format!(
                                "Summarize {}",
                                message["params"]["arguments"]["topic"].as_str().unwrap_or("all")
                            )
                        }
                    }]
                }),
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
        assert!(specs.len() >= 1);
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
        client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stdio_shutdown_is_idempotent_and_stops_descendants() {
        let root = std::env::temp_dir().join(format!(
            "pig-mcp-shutdown-tree-{}-{}",
            std::process::id(),
            crate::model_io::now_ms()
        ));
        std::fs::create_dir_all(&root).expect("test directory");
        let started = root.join("started");
        let late = root.join("late");
        #[cfg(windows)]
        let config = {
            let script = root.join("grandchild.cmd");
            std::fs::write(
                &script,
                format!(
                    "@echo off\r\necho started > \"{}\"\r\nping -n 2 127.0.0.1 >nul\r\necho late > \"{}\"\r\n",
                    started.display(),
                    late.display()
                ),
            )
            .expect("grandchild script");
            McpStdioConfig {
                command: "cmd.exe".into(),
                args: vec!["/C".into(), script.display().to_string()],
                env: HashMap::new(),
            }
        };
        #[cfg(unix)]
        let config = McpStdioConfig {
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                format!(
                    "(touch \"{}\"; sleep 1; printf late > \"{}\") & wait",
                    started.display(),
                    late.display()
                ),
            ],
            env: HashMap::new(),
        };
        let transport = StdioTransport::spawn(
            "shutdown-tree",
            &config,
            Duration::from_secs(1),
            &root,
            None,
        )
        .await
        .expect("spawn MCP transport");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descendant started");
        transport.shutdown().await;
        transport.shutdown().await;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!late.exists(), "MCP descendant survived repeated shutdown");
        drop(transport);
        let _ = std::fs::remove_dir_all(root);
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

    #[tokio::test]
    async fn stdio_resources_prompts_elicitation_and_auto_reconnect() {
        let handler: ElicitationHandler = Arc::new(|req| {
            Box::pin(async move {
                assert_eq!(req.server, "fake");
                assert_eq!(req.message, "Provide token label");
                crate::mcp::capabilities::McpElicitationResponse::accept(
                    json!({"label": "prod-key"}),
                )
            })
        });
        let (client, _) = McpClient::connect_with_elicitation(
            &fake_server_config(),
            std::path::Path::new("."),
            Some(handler),
        )
        .await
        .expect("connect");

        let caps = client.capabilities();
        assert!(caps.tools && caps.resources && caps.prompts);
        assert_eq!(client.connection_state(), McpConnectionState::Connected);

        // Resources list + read
        let resources = client.list_resources().await.expect("resources/list");
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, "file:///README.md");
        let contents = client
            .read_resource("file:///README.md")
            .await
            .expect("resources/read");
        assert_eq!(
            contents[0].text.as_deref(),
            Some("# Hello from MCP resource")
        );

        // Prompts list + get
        let prompts = client.list_prompts().await.expect("prompts/list");
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "summarize");
        let mut args = serde_json::Map::new();
        args.insert("topic".into(), json!("architecture"));
        let prompt = client
            .get_prompt("summarize", Some(args))
            .await
            .expect("prompts/get");
        assert_eq!(
            prompt.messages[0].content["text"],
            json!("Summarize architecture")
        );

        // Server-initiated elicitation/create roundtrip
        let elicited = client
            .call_tool("ask_user", json!({}))
            .await
            .expect("ask_user");
        assert_eq!(elicited["content"][0]["text"], "accept:prod-key");

        // Crash server process -> next request automatically reconnects single-flight and succeeds!
        let _ = client.call_tool("crash_once", json!({})).await;
        let recovered = client
            .call_tool("echo", json!({"text": "after-reconnect"}))
            .await
            .expect("auto-reconnect should recover");
        assert_eq!(recovered["content"][0]["text"], "after-reconnect");
        assert_eq!(client.connection_state(), McpConnectionState::Connected);

        client.shutdown().await;
    }
}
