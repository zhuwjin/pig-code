//! MCP (Model Context Protocol) support: stdio (newline JSON-RPC 2.0) and streamable HTTP
//! transports, with Claude Code-compatible config (project-level `.pigcode/mcp.json` overrides user-level
//! `<data_dir>/mcp.json`). Each server's tools are wrapped as Tool trait impls
//! (`mcp__<server>__<tool>`); connections are established lazily with the session's first step and torn down via `shutdown` on exit.
//! The remote shape is streamable HTTP (2025-03-26+ single-endpoint POST, JSON/SSE dual response shapes);
//! legacy SSE (2024-11-05 dual-endpoint) is not yet supported; the config layer logs and skips `"type": "sse"` entries.
//! Future items: OAuth, resources/prompts, roots/elicitation, legacy SSE.

mod client;
mod config;
mod http;
mod naming;
mod tool;

pub use client::{McpClient, McpToolSpec};
pub use config::{
    McpEntryError, McpEntryShape, McpHttpConfig, McpServerConfig, McpStdioConfig, McpTransport,
    validate_server,
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
            async move {
                match client::McpClient::connect(&config, &workspace_root).await {
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

    /// All MCP tools (Box<dyn Tool>, same shape as tool::all(), extendable directly into the toolset)
    pub fn tools(&self) -> Vec<Box<dyn Tool>> {
        self.servers
            .iter()
            .flat_map(|server| {
                server
                    .tools
                    .iter()
                    .map(|tool| Box::new((**tool).clone()) as Box<dyn Tool>)
            })
            .collect()
    }

    /// Get a single tool handle by combined name (`mcp__<server>__<tool>`) (concurrent group tasks fetch by name on demand;
    /// cloning an McpTool is just an Arc clone, very cheap)
    pub fn tool_named(&self, name: &str) -> Option<Box<dyn Tool>> {
        self.servers
            .iter()
            .flat_map(|server| server.tools.iter())
            .find(|tool| tool.name() == name)
            .map(|tool| Box::new((**tool).clone()) as Box<dyn Tool>)
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

    /// Shut down all connections (stdio kills and reaps the subprocess; http sends a best-effort DELETE to terminate the session)
    pub async fn shutdown(&self) {
        futures_util::future::join_all(self.servers.iter().map(|server| server.client.shutdown()))
            .await;
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
}
