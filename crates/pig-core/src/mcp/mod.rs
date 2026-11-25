//! MCP (Model Context Protocol) support: stdio (newline JSON-RPC 2.0) and streamable HTTP
//! transports, with Claude Code-compatible config (project-level `.pigcode/mcp.json` overrides user-level
//! `<data_dir>/mcp.json`). Each server's tools are wrapped as Tool trait impls
//! (`mcp__<server>__<tool>`); connections are established lazily with the session's first step and torn down via `shutdown` on exit.
//! The remote shape is streamable HTTP (2025-03-26+ single-endpoint POST, JSON/SSE dual response shapes);
//! legacy SSE (2024-11-05 dual-endpoint) is not supported; the config layer logs and skips `"type": "sse"` entries.
//! Future items: roots.

mod capabilities;
mod client;
mod config;
mod elicitation;
mod http;
mod meta_tools;
mod naming;
mod tool;

pub use capabilities::{
    ElicitationHandler, McpConnectionState, McpElicitationAction, McpElicitationRequest,
    McpElicitationResponse, McpPromptArgument, McpPromptMessage, McpPromptResult, McpPromptSpec,
    McpResourceContent, McpResourceSpec, McpResourceTemplate, McpServerCapabilities,
};
pub use client::{McpClient, McpToolSpec};
pub use config::{
    McpEntryError, McpEntryShape, McpHttpConfig, McpOAuthConfig, McpServerConfig, McpStdioConfig,
    McpTransport, validate_server,
};
pub use elicitation::build_session_elicitation_handler;
pub use meta_tools::{
    MCP_GET_PROMPT_TOOL, MCP_LIST_PROMPTS_TOOL, MCP_LIST_RESOURCES_TOOL, MCP_READ_RESOURCE_TOOL,
};
pub use tool::{McpTool, McpToolAnnotations};

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use crate::config::WorkspaceTrust;
use crate::tool::Tool;

/// One sanitized MCP tool name claimed by more than one `(server, tool)` pair.
/// Sanitization is irreversible ("a b" and "a_b" collapse to the same name),
/// so the name cannot identify a target: every colliding tool is removed from
/// the tool set instead of routing to whichever server happened to load first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpToolCollision {
    pub name: String,
    /// Names of the servers whose tools sanitize to `name` (sorted).
    pub servers: Vec<String>,
}

/// A connected server: client handle + tool list
struct McpServer {
    client: Arc<McpClient>,
    tools: Vec<Arc<McpTool>>,
}

/// MCP connection manager: loads config, connects to all servers concurrently; a single server's failure does not affect the others
pub struct McpManager {
    servers: Vec<McpServer>,
    /// Servers that failed to connect (with reasons): for display on the settings page
    failures: Vec<pig_protocol::McpServerStatus>,
    /// Ambiguous sanitized tool names that were removed from the tool set
    collisions: Vec<McpToolCollision>,
}

impl McpManager {
    /// Load the user-level config only (fail-closed) and connect: project
    /// servers are repository content and need a trust decision.
    pub async fn connect_all(
        workspace_root: &Path,
        data_dir: &Path,
        plugins: &crate::plugins::SharedPluginRegistry,
    ) -> Self {
        Self::connect_all_with_trust(
            workspace_root,
            data_dir,
            &WorkspaceTrust::untrusted(workspace_root),
            plugins,
        )
        .await
    }

    /// Load the config under one workspace-trust decision and connect to all
    /// servers concurrently; failures are recorded one by one and skipped (not
    /// fatal), with reasons kept for the settings page.
    /// The stdio subprocess working directory = workspace_root (relative args
    /// resolve against the workspace root).
    pub async fn connect_all_with_trust(
        workspace_root: &Path,
        data_dir: &Path,
        trust: &WorkspaceTrust,
        plugins: &crate::plugins::SharedPluginRegistry,
    ) -> Self {
        Self::connect_all_with_trust_and_elicitation(workspace_root, data_dir, trust, plugins, None)
            .await
    }

    /// Connect all configured MCP servers under `trust` with an optional
    /// session-bound `elicitation/create` handler and persistent OAuth token
    /// directory (`data_dir/oauth`).
    pub async fn connect_all_with_trust_and_elicitation(
        workspace_root: &Path,
        data_dir: &Path,
        trust: &WorkspaceTrust,
        plugins: &crate::plugins::SharedPluginRegistry,
        elicitation: Option<ElicitationHandler>,
    ) -> Self {
        let configs = if trust.admits(workspace_root) {
            McpServerConfig::load_with_trust(workspace_root, data_dir, trust, plugins)
        } else {
            // Fail-closed path: the user level only (project servers are
            // repository content and never parsed without a matching trust
            // decision).
            config::load(workspace_root, data_dir, plugins)
        };
        let results = futures_util::future::join_all(configs.into_iter().map(|config| {
            let workspace_root = workspace_root.to_path_buf();
            let data_dir = data_dir.to_path_buf();
            let elicitation = elicitation.clone();
            async move {
                match client::McpClient::connect_with_options(
                    &config,
                    &workspace_root,
                    elicitation,
                    Some(&data_dir),
                )
                .await
                {
                    Ok((client, specs)) => {
                        let tools = specs
                            .into_iter()
                            .map(|spec| Arc::new(McpTool::new(&config.name, spec, client.clone())))
                            .collect();
                        Ok(McpServer { client, tools })
                    }
                    Err(e) => {
                        tracing::warn!("failed to connect {}, skipped: {e:?}", config.name);
                        Err(pig_protocol::McpServerStatus {
                            name: config.name,
                            connected: false,
                            tool_count: 0,
                            error: Some(e),
                        })
                    }
                }
            }
        }))
        .await;
        let mut servers = Vec::new();
        let mut failures = Vec::new();
        for result in results {
            match result {
                Ok(server) => servers.push(server),
                Err(failure) => failures.push(failure),
            }
        }
        let collisions = detect_collisions(&mut servers);
        Self {
            servers,
            failures,
            collisions,
        }
    }

    /// Sanitized names that were ambiguous across servers (removed from the
    /// tool set; the model never sees a name that cannot be routed).
    pub fn collisions(&self) -> &[McpToolCollision] {
        &self.collisions
    }

    /// Snapshot of all server statuses (connected ones with tool counts, failed ones with reasons; sorted by name, for the settings page)
    pub fn statuses(&self) -> Vec<pig_protocol::McpServerStatus> {
        let mut out: Vec<_> = self
            .servers
            .iter()
            .map(|server| pig_protocol::McpServerStatus {
                name: server.client.name().to_string(),
                connected: true,
                tool_count: server.tools.len(),
                error: None,
            })
            .collect();
        out.extend(self.failures.iter().cloned());
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn capability_meta_tools(&self) -> Vec<Box<dyn Tool>> {
        let mut out: Vec<Box<dyn Tool>> = Vec::new();
        let resource_clients: Vec<Arc<McpClient>> = self
            .servers
            .iter()
            .filter(|s| s.client.capabilities().resources)
            .map(|s| s.client.clone())
            .collect();
        if !resource_clients.is_empty() {
            out.push(Box::new(meta_tools::McpListResourcesTool::new(
                resource_clients.clone(),
            )));
            out.push(Box::new(meta_tools::McpReadResourceTool::new(
                resource_clients,
            )));
        }
        let prompt_clients: Vec<Arc<McpClient>> = self
            .servers
            .iter()
            .filter(|s| s.client.capabilities().prompts)
            .map(|s| s.client.clone())
            .collect();
        if !prompt_clients.is_empty() {
            out.push(Box::new(meta_tools::McpListPromptsTool::new(
                prompt_clients.clone(),
            )));
            out.push(Box::new(meta_tools::McpGetPromptTool::new(prompt_clients)));
        }
        out
    }

    /// All MCP tools (Box<dyn Tool>, same shape as tool::all(), extendable directly into the toolset),
    /// plus read-only `mcp_list_resources` / `mcp_read_resource` / `mcp_list_prompts` / `mcp_get_prompt`
    /// when connected servers advertise `resources` or `prompts`.
    pub fn tools(&self) -> Vec<Box<dyn Tool>> {
        let mut tools: Vec<Box<dyn Tool>> = self
            .servers
            .iter()
            .flat_map(|server| {
                server
                    .tools
                    .iter()
                    .map(|tool| Box::new((**tool).clone()) as Box<dyn Tool>)
            })
            .collect();
        tools.extend(self.capability_meta_tools());
        tools
    }

    /// Get a single tool handle by combined name (`mcp__<server>__<tool>` or
    /// MCP resource/prompt meta-tool name).
    pub fn tool_named(&self, name: &str) -> Option<Box<dyn Tool>> {
        self.servers
            .iter()
            .flat_map(|server| server.tools.iter())
            .find(|tool| tool.name() == name)
            .map(|tool| Box::new((**tool).clone()) as Box<dyn Tool>)
            .or_else(|| {
                self.capability_meta_tools()
                    .into_iter()
                    .find(|tool| tool.name() == name)
            })
    }

    /// MCP toolset inherited by subagents: inherits_all=false (read-only profile) only yields readOnlyHint tools
    pub fn child_tools(&self, inherits_all: bool) -> Vec<Box<dyn Tool>> {
        self.tools()
            .into_iter()
            .filter(|tool| inherits_all || tool.read_only())
            .collect()
    }

    /// List of connected server names (for diagnostics / settings page display)
    pub fn server_names(&self) -> Vec<&str> {
        self.servers
            .iter()
            .map(|server| server.client.name())
            .collect()
    }

    /// Ping all servers (liveness probe)
    pub async fn ping_all(&self) -> Vec<(String, Result<(), String>)> {
        futures_util::future::join_all(
            self.servers.iter().map(|server| async {
                (server.client.name().to_string(), server.client.ping().await)
            }),
        )
        .await
    }

    /// Snapshot of every connected server's current reconnection state.
    pub fn connection_states(&self) -> Vec<(String, McpConnectionState)> {
        self.servers
            .iter()
            .map(|server| {
                (
                    server.client.name().to_string(),
                    server.client.connection_state(),
                )
            })
            .collect()
    }

    /// List resources advertised across all connected servers supporting `resources`.
    pub async fn list_resources(&self) -> Vec<(String, Result<Vec<McpResourceSpec>, String>)> {
        futures_util::future::join_all(
            self.servers
                .iter()
                .filter(|server| server.client.capabilities().resources)
                .map(|server| async {
                    (
                        server.client.name().to_string(),
                        server.client.list_resources().await,
                    )
                }),
        )
        .await
    }

    /// Read one resource URI from a specific named MCP server.
    pub async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
    ) -> Result<Vec<McpResourceContent>, String> {
        let server = self
            .servers
            .iter()
            .find(|s| s.client.name() == server_name)
            .ok_or_else(|| format!("MCP server not found: {server_name}"))?;
        server.client.read_resource(uri).await
    }

    /// List prompts advertised across all connected servers supporting `prompts`.
    pub async fn list_prompts(&self) -> Vec<(String, Result<Vec<McpPromptSpec>, String>)> {
        futures_util::future::join_all(
            self.servers
                .iter()
                .filter(|server| server.client.capabilities().prompts)
                .map(|server| async {
                    (
                        server.client.name().to_string(),
                        server.client.list_prompts().await,
                    )
                }),
        )
        .await
    }

    /// Render/fetch one prompt from a specific named MCP server.
    pub async fn get_prompt(
        &self,
        server_name: &str,
        prompt_name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<McpPromptResult, String> {
        let server = self
            .servers
            .iter()
            .find(|s| s.client.name() == server_name)
            .ok_or_else(|| format!("MCP server not found: {server_name}"))?;
        server.client.get_prompt(prompt_name, arguments).await
    }

    /// Shut down all connections (stdio kills and reaps the subprocess; http sends a best-effort DELETE to terminate the session)
    pub async fn shutdown(&self) {
        futures_util::future::join_all(self.servers.iter().map(|server| server.client.shutdown()))
            .await;
    }

    /// Run an interactive OAuth 2.0 + PKCE loopback authorization flow for a
    /// configured remote MCP HTTP server and persist its token bundle.
    /// If `authorization_url` is omitted in config, attempts RFC 9728 / RFC 8414
    /// discovery against the server's endpoint URL.
    pub async fn start_oauth<F>(
        workspace_root: &Path,
        data_dir: &Path,
        trust: &WorkspaceTrust,
        plugins: &crate::plugins::SharedPluginRegistry,
        server_name: &str,
        timeout: std::time::Duration,
        cancel: tokio_util::sync::CancellationToken,
        on_authorize_url: F,
    ) -> Result<(), String>
    where
        F: FnOnce(String),
    {
        let configs = if trust.admits(workspace_root) {
            McpServerConfig::load_with_trust(workspace_root, data_dir, trust, plugins)
        } else {
            config::load(workspace_root, data_dir, plugins)
        };
        let server = configs
            .into_iter()
            .find(|cfg| cfg.name == server_name)
            .ok_or_else(|| format!("MCP server `{server_name}` is not configured"))?;
        let McpTransport::Http(http_cfg) = server.transport else {
            return Err(format!(
                "MCP server `{server_name}` uses stdio transport and does not support OAuth"
            ));
        };
        let mut oauth_cfg = http_cfg
            .oauth
            .ok_or_else(|| format!("MCP server `{server_name}` has no OAuth configuration"))?;

        if oauth_cfg
            .authorization_url
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        {
            let discovered = pig_provider::oauth::discover_oauth_metadata(&http_cfg.url).await?;
            oauth_cfg.authorization_url = Some(discovered.authorization_endpoint.ok_or_else(|| {
                format!(
                    "OAuth discovery for `{server_name}` did not advertise an authorization_endpoint"
                )
            })?);
            if oauth_cfg.resource.is_none() {
                oauth_cfg.resource = discovered.resource;
            }
        }

        let manager = http::build_oauth_manager_for_server(server_name, &oauth_cfg, Some(data_dir))
            .map_err(|e| format!("{e:?}"))?;
        manager
            .authorize_with_loopback(timeout, cancel, on_authorize_url)
            .await
            .map(|_| ())
    }

    /// Clear persisted OAuth tokens for a configured remote MCP HTTP server.
    pub fn clear_oauth(
        workspace_root: &Path,
        data_dir: &Path,
        trust: &WorkspaceTrust,
        plugins: &crate::plugins::SharedPluginRegistry,
        server_name: &str,
    ) -> Result<(), String> {
        let configs = if trust.admits(workspace_root) {
            McpServerConfig::load_with_trust(workspace_root, data_dir, trust, plugins)
        } else {
            config::load(workspace_root, data_dir, plugins)
        };
        if let Some(server) = configs.into_iter().find(|cfg| cfg.name == server_name)
            && let McpTransport::Http(http_cfg) = server.transport
            && let Some(oauth_cfg) = http_cfg.oauth
        {
            let manager =
                http::build_oauth_manager_for_server(server_name, &oauth_cfg, Some(data_dir))
                    .map_err(|e| format!("{e:?}"))?;
            return manager.clear_tokens();
        }
        // Fallback: remove the default token file if present.
        let default_path = http::default_oauth_token_path(data_dir, server_name);
        match std::fs::remove_file(&default_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!(
                "Failed to remove MCP OAuth token file {}: {e}",
                default_path.display()
            )),
        }
    }

    /// For tests: a manager with a single fake server (Noop transport, only carries the tool list/read-only flags)
    #[cfg(test)]
    pub(crate) fn for_test(server: &str, specs: Vec<McpToolSpec>) -> Self {
        let client = McpClient::for_test(server);
        let tools = specs
            .into_iter()
            .map(|spec| Arc::new(McpTool::new(server, spec, client.clone())))
            .collect();
        Self {
            servers: vec![McpServer { client, tools }],
            failures: Vec::new(),
            collisions: Vec::new(),
        }
    }

    /// For tests: inject connection failure records (the failure branch of statuses())
    #[cfg(test)]
    pub(crate) fn with_failures(mut self, failures: Vec<pig_protocol::McpServerStatus>) -> Self {
        self.failures = failures;
        self
    }
}

/// Remove every tool whose sanitized name is claimed by more than one server
/// and report the ambiguity. Stable order (name, then server names) so status
/// and logs are deterministic.
fn detect_collisions(servers: &mut [McpServer]) -> Vec<McpToolCollision> {
    // One entry per (server, tool) origin — not per server name: two tools on
    // the same server can also sanitize to the same combined name.
    let mut origins: HashMap<&str, Vec<String>> = HashMap::new();
    for server in servers.iter() {
        for tool in &server.tools {
            origins
                .entry(tool.name())
                .or_default()
                .push(server.client.name().to_string());
        }
    }
    let mut collisions: Vec<McpToolCollision> = origins
        .into_iter()
        .filter(|(_, origins)| origins.len() > 1)
        .map(|(name, mut origins)| {
            origins.sort();
            origins.dedup();
            McpToolCollision {
                name: name.to_string(),
                servers: origins,
            }
        })
        .collect();
    collisions.sort_by(|a, b| a.name.cmp(&b.name));
    let ambiguous: HashSet<&str> = collisions.iter().map(|c| c.name.as_str()).collect();
    if !ambiguous.is_empty() {
        for collision in &collisions {
            tracing::warn!(
                tool = %collision.name,
                servers = %collision.servers.join(", "),
                "MCP tool name is ambiguous after sanitization; removing it from the tool set"
            );
        }
        for server in servers.iter_mut() {
            server.tools.retain(|tool| !ambiguous.contains(tool.name()));
        }
    }
    collisions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> McpToolSpec {
        McpToolSpec {
            name: name.to_string(),
            description: None,
            input_schema: serde_json::json!({}),
            annotations: McpToolAnnotations::default(),
        }
    }

    #[test]
    fn statuses_merges_connected_and_failures_sorted_by_name() {
        let manager = McpManager::for_test("zeta", vec![spec("a"), spec("b"), spec("c")])
            .with_failures(vec![pig_protocol::McpServerStatus {
                name: "alpha-failed".to_string(),
                connected: false,
                tool_count: 0,
                error: Some(pig_protocol::CoreError::McpSpawn {
                    command: "bad-server".to_string(),
                    detail: "spawn failed".to_string(),
                }),
            }]);
        let statuses = manager.statuses();
        let names: Vec<&str> = statuses.iter().map(|s| s.name.as_str()).collect();
        // Failures and connected servers are merged, then sorted by name
        assert_eq!(names, vec!["alpha-failed", "zeta"]);
        let zeta = &statuses[1];
        assert!(zeta.connected);
        assert_eq!(zeta.tool_count, 3);
        assert!(zeta.error.is_none());
        let failed = &statuses[0];
        assert!(!failed.connected);
        assert!(matches!(
            failed.error,
            Some(pig_protocol::CoreError::McpSpawn { .. })
        ));
    }

    /// Two servers whose sanitized names coincide (spaces vs underscores) must
    /// not leave a tool that silently routes to whichever server connected
    /// first: the ambiguous name is removed and reported.
    #[test]
    fn sanitized_tool_name_collisions_are_detected_and_removed() {
        let client_a = McpClient::for_test("a b");
        let client_b = McpClient::for_test("a_b");
        let tool = |client: &Arc<McpClient>| {
            Arc::new(McpTool::new(client.name(), spec("read"), client.clone()))
        };
        let mut servers = vec![
            McpServer {
                client: client_a.clone(),
                tools: vec![tool(&client_a)],
            },
            McpServer {
                client: client_b.clone(),
                tools: vec![tool(&client_b)],
            },
        ];
        let collisions = detect_collisions(&mut servers);
        assert_eq!(
            collisions,
            vec![McpToolCollision {
                name: "mcp__a_b__read".to_string(),
                servers: vec!["a b".to_string(), "a_b".to_string()],
            }]
        );
        assert!(
            servers.iter().all(|server| server.tools.is_empty()),
            "the ambiguous name is removed from every server"
        );
        let manager = McpManager {
            servers,
            failures: Vec::new(),
            collisions,
        };
        assert!(
            manager.tool_named("mcp__a_b__read").is_none(),
            "no first-match routing for an ambiguous name"
        );

        // Two tools on the SAME server can collide too (sanitization is
        // lossy); they must be detected just like cross-server collisions.
        let client = McpClient::for_test("srv");
        let mut same_server = vec![McpServer {
            client: client.clone(),
            tools: vec![
                Arc::new(McpTool::new("srv", spec("a b"), client.clone())),
                Arc::new(McpTool::new("srv", spec("a_b"), client.clone())),
            ],
        }];
        let collisions = detect_collisions(&mut same_server);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].name, "mcp__srv__a_b");
        assert!(same_server[0].tools.is_empty());
    }

    #[tokio::test]
    async fn oauth_pkce_elicitation_resources_and_prompts_work_end_to_end() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        // 1. Mock OAuth token server
        let token_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind token");
        let token_addr = token_listener.local_addr().expect("token addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = token_listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    let body = serde_json::json!({
                        "access_token": "pkce-mcp-token",
                        "refresh_token": "pkce-mcp-refresh",
                        "token_type": "Bearer",
                        "expires_in": 3600
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        // 2. Mock Streamable HTTP MCP server requiring Bearer auth and advertising
        //    tools + resources + prompts, plus triggering elicitation/create during tools/call.
        let mcp_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mcp");
        let mcp_addr = mcp_listener.local_addr().expect("mcp addr");
        let elicitation_reply: Arc<tokio::sync::Mutex<Option<serde_json::Value>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let elicitation_notify = Arc::new(tokio::sync::Notify::new());
        let elicit_reply_srv = Arc::clone(&elicitation_reply);
        let elicit_notify_srv = Arc::clone(&elicitation_notify);

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = mcp_listener.accept().await else {
                    return;
                };
                let elicit_reply = Arc::clone(&elicit_reply_srv);
                let elicit_notify = Arc::clone(&elicit_notify_srv);
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 4096];
                    let mut header_end = None;
                    let mut content_len = 0usize;
                    while let Ok(n) = socket.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        raw.extend_from_slice(&buf[..n]);
                        if header_end.is_none()
                            && let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n")
                        {
                            header_end = Some(pos + 4);
                            let head = String::from_utf8_lossy(&raw[..pos]);
                            for line in head.lines() {
                                if let Some((k, v)) = line.split_once(':')
                                    && k.trim().eq_ignore_ascii_case("content-length")
                                {
                                    content_len = v.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                        if let Some(end) = header_end
                            && raw.len() >= end + content_len
                        {
                            break;
                        }
                    }
                    let Some(end) = header_end else {
                        return;
                    };
                    let head = String::from_utf8_lossy(&raw[..end]);
                    if !head.contains("Authorization: Bearer pkce-mcp-token")
                        && !head.contains("authorization: Bearer pkce-mcp-token")
                    {
                        let resp = "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                        let _ = socket.write_all(resp.as_bytes()).await;
                        let _ = socket.shutdown().await;
                        return;
                    }
                    let body_bytes = &raw[end..end + content_len];
                    let Ok(msg) = serde_json::from_slice::<serde_json::Value>(body_bytes) else {
                        let resp = "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                        let _ = socket.write_all(resp.as_bytes()).await;
                        let _ = socket.shutdown().await;
                        return;
                    };

                    // Client reply to our server-initiated elicitation/create request
                    if msg.get("method").is_none() && msg.get("id").and_then(|v| v.as_u64()) == Some(77)
                    {
                        *elicit_reply.lock().await = Some(msg["result"].clone());
                        elicit_notify.notify_waiters();
                        let resp = "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                        let _ = socket.write_all(resp.as_bytes()).await;
                        let _ = socket.shutdown().await;
                        return;
                    }

                    let id = msg.get("id").cloned();
                    let method = msg["method"].as_str().unwrap_or("");
                    if id.is_none() {
                        let resp = "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                        let _ = socket.write_all(resp.as_bytes()).await;
                        let _ = socket.shutdown().await;
                        return;
                    }
                    let id = id.unwrap();

                    if method == "tools/call" && msg["params"]["name"] == "deploy" {
                        // Stream SSE: send server->client elicitation/create, wait for client's POST reply, then finish.
                        let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                        let _ = socket.write_all(headers.as_bytes()).await;
                        let elicit_req = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": 77,
                            "method": "elicitation/create",
                            "params": {
                                "message": "Confirm deployment target",
                                "requestedSchema": {
                                    "type": "object",
                                    "properties": {
                                        "confirm": { "type": "boolean" },
                                        "env": { "type": "string", "enum": ["staging", "prod"] }
                                    },
                                    "required": ["confirm", "env"]
                                }
                            }
                        });
                        let chunk = format!("data: {elicit_req}\n\n");
                        let _ = socket.write_all(chunk.as_bytes()).await;
                        let _ = socket.flush().await;

                        let answer = loop {
                            if let Some(val) = elicit_reply.lock().await.take() {
                                break val;
                            }
                            elicit_notify.notified().await;
                        };
                        let done = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": format!("deployed: {}", answer)
                                }]
                            }
                        });
                        let done_chunk = format!("data: {done}\n\n");
                        let _ = socket.write_all(done_chunk.as_bytes()).await;
                        let _ = socket.shutdown().await;
                        return;
                    }

                    let result = match method {
                        "initialize" => serde_json::json!({
                            "protocolVersion": "2025-03-26",
                            "capabilities": {
                                "tools": {},
                                "resources": {},
                                "prompts": {}
                            },
                            "serverInfo": { "name": "remote_demo", "version": "1.0" }
                        }),
                        "tools/list" => serde_json::json!({
                            "tools": [{
                                "name": "deploy",
                                "description": "Deploy service",
                                "inputSchema": { "type": "object" }
                            }]
                        }),
                        "resources/list" => serde_json::json!({
                            "resources": [{
                                "uri": "docs://guide",
                                "name": "Guide",
                                "mimeType": "text/markdown",
                                "description": "Deployment guide"
                            }]
                        }),
                        "resources/templates/list" => serde_json::json!({
                            "resourceTemplates": []
                        }),
                        "resources/read" => serde_json::json!({
                            "contents": [{
                                "uri": "docs://guide",
                                "mimeType": "text/markdown",
                                "text": "# Deployment Guide\nRun tests first."
                            }]
                        }),
                        "prompts/list" => serde_json::json!({
                            "prompts": [{
                                "name": "release_notes",
                                "description": "Draft release notes",
                                "arguments": [{ "name": "version", "required": true }]
                            }]
                        }),
                        "prompts/get" => {
                            let ver = msg["params"]["arguments"]["version"]
                                .as_str()
                                .unwrap_or("?");
                            serde_json::json!({
                                "description": "Release notes prompt",
                                "messages": [{
                                    "role": "user",
                                    "content": { "type": "text", "text": format!("Write notes for {ver}") }
                                }]
                            })
                        }
                        _ => serde_json::json!({}),
                    };
                    let body = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        // 3. Configure user mcp.json with OAuth (no static token, no manual tokenFile).
        let unique = format!(
            "pig-mcp-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let workspace = root.join("ws");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        let mcp_json = serde_json::json!({
            "mcpServers": {
                "remote_demo": {
                    "type": "http",
                    "url": format!("http://{mcp_addr}/mcp"),
                    "oauth": {
                        "clientId": "pig-client",
                        "authorizationUrl": "https://auth.example.com/authorize",
                        "tokenUrl": format!("http://{token_addr}/token"),
                        "scopes": ["mcp:all"]
                    }
                }
            }
        });
        std::fs::write(
            data_dir.join("mcp.json"),
            serde_json::to_string_pretty(&mcp_json).unwrap(),
        )
        .unwrap();

        let plugins = crate::plugins::PluginRegistry::shared(data_dir.clone());
        let trust = WorkspaceTrust::untrusted(&workspace);

        // 4. Run full PKCE OAuth flow via McpManager::start_oauth
        McpManager::start_oauth(
            &workspace,
            &data_dir,
            &trust,
            &plugins,
            "remote_demo",
            std::time::Duration::from_secs(5),
            tokio_util::sync::CancellationToken::new(),
            |auth_url| {
                let parsed = reqwest::Url::parse(&auth_url).expect("auth url");
                let redirect = parsed
                    .query_pairs()
                    .find(|(k, _)| k == "redirect_uri")
                    .unwrap()
                    .1
                    .into_owned();
                let state = parsed
                    .query_pairs()
                    .find(|(k, _)| k == "state")
                    .unwrap()
                    .1
                    .into_owned();
                tokio::spawn(async move {
                    let _ = reqwest::Client::new()
                        .get(format!("{redirect}?code=code-99&state={state}"))
                        .send()
                        .await;
                });
            },
        )
        .await
        .expect("OAuth PKCE flow must succeed");

        assert!(
            http::default_oauth_token_path(&data_dir, "remote_demo").is_file(),
            "OAuth token bundle must persist under data_dir/oauth/"
        );

        // 5. Connect MCP manager with session elicitation handler; token is loaded automatically.
        let pending_questions: crate::session::PendingQuestions =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = async_channel::bounded::<pig_protocol::Event>(32);
        let handler = build_session_elicitation_handler(
            "sess-1".to_string(),
            pending_questions.clone(),
            event_tx.clone(),
        );
        let manager = McpManager::connect_all_with_trust_and_elicitation(
            &workspace,
            &data_dir,
            &trust,
            &plugins,
            Some(handler),
        )
        .await;
        let statuses = manager.statuses();
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].connected, "{statuses:?}");

        // 6. Verify resource & prompt meta-tools are registered and read-only.
        let all_tools = manager.tools();
        for expected in [
            "mcp__remote_demo__deploy",
            MCP_LIST_RESOURCES_TOOL,
            MCP_READ_RESOURCE_TOOL,
            MCP_LIST_PROMPTS_TOOL,
            MCP_GET_PROMPT_TOOL,
        ] {
            assert!(
                all_tools.iter().any(|t| t.name() == expected),
                "missing tool {expected}"
            );
        }
        assert!(manager.tool_named(MCP_LIST_RESOURCES_TOOL).unwrap().read_only());
        assert!(manager.tool_named(MCP_READ_RESOURCE_TOOL).unwrap().read_only());
        assert!(manager.tool_named(MCP_LIST_PROMPTS_TOOL).unwrap().read_only());
        assert!(manager.tool_named(MCP_GET_PROMPT_TOOL).unwrap().read_only());

        let (task_tx, _task_rx) = async_channel::bounded(16);
        let tool_state =
            crate::task::SessionToolState::new("sess-1".into(), task_tx, vec![], vec![]);
        let mut tracker = crate::tool::ChangeTracker::default();
        fn make_ctx<'a>(
            cwd: &'a Path,
            tracker: &'a mut crate::tool::ChangeTracker,
            state: &'a crate::task::SessionToolState,
        ) -> crate::tool::ToolContext<'a> {
            crate::tool::ToolContext {
                cwd,
                tracker,
                state,
                fs_grant: None,
                sandbox_override: crate::task::SandboxOverride::default(),
                sandbox_profile: crate::task::SandboxProfile::WorkspaceWrite,
                additional_permissions: crate::task::AdditionalPermissions::default(),
            }
        }

        let listed = manager
            .tool_named(MCP_LIST_RESOURCES_TOOL)
            .unwrap()
            .execute(serde_json::json!({}), make_ctx(&workspace, &mut tracker, &tool_state))
            .await
            .expect("list resources");
        assert!(listed.output.contains("docs://guide"), "{}", listed.output);

        let read_res = manager
            .tool_named(MCP_READ_RESOURCE_TOOL)
            .unwrap()
            .execute(
                serde_json::json!({ "server": "remote_demo", "uri": "docs://guide" }),
                make_ctx(&workspace, &mut tracker, &tool_state),
            )
            .await
            .expect("read resource");
        assert!(
            read_res.output.contains("Run tests first."),
            "{}",
            read_res.output
        );

        let prompts = manager
            .tool_named(MCP_LIST_PROMPTS_TOOL)
            .unwrap()
            .execute(serde_json::json!({}), make_ctx(&workspace, &mut tracker, &tool_state))
            .await
            .expect("list prompts");
        assert!(prompts.output.contains("release_notes"), "{}", prompts.output);

        let prompt = manager
            .tool_named(MCP_GET_PROMPT_TOOL)
            .unwrap()
            .execute(
                serde_json::json!({
                    "server": "remote_demo",
                    "name": "release_notes",
                    "arguments": { "version": "2.0" }
                }),
                make_ctx(&workspace, &mut tracker, &tool_state),
            )
            .await
            .expect("get prompt");
        assert!(
            prompt.output.contains("Write notes for 2.0"),
            "{}",
            prompt.output
        );

        // 7. Call `deploy`, answer the server-initiated elicitation via QuestionRequested / PendingQuestions.
        let pq_answer = pending_questions.clone();
        let answer_task = tokio::spawn(async move {
            let ev = event_rx.recv().await.expect("QuestionRequested");
            match ev {
                pig_protocol::Event::QuestionRequested {
                    session_id,
                    request_id,
                    questions,
                } => {
                    assert_eq!(session_id, "sess-1");
                    assert_eq!(questions.len(), 2);
                    let reply = pq_answer
                        .lock()
                        .unwrap()
                        .remove(&request_id)
                        .expect("pending elicitation question");
                    let _ = reply.send(Some(vec![vec!["Yes".into()], vec!["prod".into()]]));
                }
                other => panic!("expected QuestionRequested, got {other:?}"),
            }
        });

        let deploy_res = manager
            .tool_named("mcp__remote_demo__deploy")
            .unwrap()
            .execute(serde_json::json!({}), make_ctx(&workspace, &mut tracker, &tool_state))
            .await
            .expect("deploy with elicitation");
        answer_task.await.expect("answer task");
        assert!(
            deploy_res.output.contains("\"action\":\"accept\"")
                && deploy_res.output.contains("\"confirm\":true")
                && deploy_res.output.contains("\"env\":\"prod\""),
            "{}",
            deploy_res.output
        );

        manager.shutdown().await;
        McpManager::clear_oauth(&workspace, &data_dir, &trust, &plugins, "remote_demo")
            .expect("clear oauth");
        assert!(!http::default_oauth_token_path(&data_dir, "remote_demo").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
