//! MCP server config loading: Claude Code-compatible shape
//! `{ "mcpServers": { "<name>": { "command", "args", "env", "timeoutMs" } } }` (stdio)
//! and the remote shape `{ "url", "headers", "timeoutMs" }` (streamable HTTP).
//! The `type` field may be omitted: presence of `url` infers remote, presence of `command` infers stdio;
//! an explicit `"type": "stdio" | "http"` is validated as declared. `"type": "sse"` (2024-11-05
//! legacy HTTP+SSE) is not yet supported; it is logged and skipped.
//! `"disabled": true` disables an entry: kept through parsing, filtered after merging (same-name override), excluded from connecting.
//! User-level `<data_dir>/mcp.json` is the base; project-level `<workspace>/.pigcode/mcp.json` overrides same-name entries
//! **only when the workspace is trusted** (the same `[hooks] trusted_projects` decision that admits hooks, project
//! config and permission rules). [`load`] is fail-closed (user level only); the runtime passes a
//! [`WorkspaceTrust`] to [`load_with_trust`].

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use crate::config::WorkspaceTrust;

/// Default timeout for a single call (initialize/tools/list/tools/call/ping)
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub timeout: Duration,
    /// `disabled: true` disables the entry: kept through parsing/merging (project level can disable a user-level same-name entry),
    /// filtered out wholesale at the connection stage
    pub disabled: bool,
}

/// Transport config: stdio subprocess / streamable HTTP remote endpoint
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    Stdio(McpStdioConfig),
    Http(McpHttpConfig),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStdioConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// OAuth 2.0 + PKCE configuration for remote MCP HTTP servers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthConfig {
    #[serde(alias = "client_id")]
    pub client_id: String,
    #[serde(default, alias = "client_secret")]
    pub client_secret: Option<String>,
    #[serde(default, alias = "authorization_url")]
    pub authorization_url: Option<String>,
    #[serde(alias = "token_url")]
    pub token_url: String,
    #[serde(default, alias = "redirect_uri")]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default, alias = "access_token")]
    pub access_token: Option<String>,
    #[serde(default, alias = "refresh_token")]
    pub refresh_token: Option<String>,
    #[serde(default, alias = "expires_at")]
    pub expires_at: Option<u64>,
    #[serde(default, alias = "token_file")]
    pub token_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHttpConfig {
    pub url: String,
    /// Custom request headers (carried on every POST/DELETE)
    pub headers: HashMap<String, String>,
    /// Optional OAuth 2.0 / PKCE configuration for automatic bearer injection & 401 refresh
    pub oauth: Option<McpOAuthConfig>,
}

#[derive(serde::Deserialize)]
struct RawServer {
    command: Option<String>,
    url: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    oauth: Option<serde_json::Value>,
    #[serde(rename = "timeoutMs")]
    timeout_ms: Option<u64>,
    #[serde(default)]
    disabled: bool,
}

/// Load the user level only (fail-closed). Project servers are repository
/// content: until the workspace is trusted they are neither parsed nor started.
/// Runtime call sites use [`load_with_trust`].
pub fn load(
    workspace_root: &Path,
    data_dir: &Path,
    plugins: &crate::plugins::SharedPluginRegistry,
) -> Vec<McpServerConfig> {
    load_with_trust(
        workspace_root,
        data_dir,
        &WorkspaceTrust::untrusted(workspace_root),
        plugins,
    )
}

/// Load and merge both sources under one workspace-trust decision. A missing
/// or invalid file does not panic but is logged and skipped; `disabled` takes
/// effect after the override merge (a trusted project can disable a user-level
/// same-name entry).
pub fn load_with_trust(
    workspace_root: &Path,
    data_dir: &Path,
    trust: &WorkspaceTrust,
    plugins: &crate::plugins::SharedPluginRegistry,
) -> Vec<McpServerConfig> {
    let user = load_file(&data_dir.join("mcp.json"));
    let project_path = workspace_root.join(".pigcode").join("mcp.json");
    let project = if trust.admits(workspace_root) {
        load_file(&project_path)
    } else {
        if project_path.is_file() {
            tracing::debug!(
                "the project MCP config {} is not trusted; no project server will be started",
                project_path.display()
            );
        }
        Vec::new()
    };
    let configured = merged_enabled(user, project);
    merge_missing(
        configured,
        crate::plugins::mcp_servers(workspace_root, plugins, trust),
    )
    .into_iter()
    .filter(|server| !server.disabled)
    .collect()
}

impl McpServerConfig {
    /// Public entry point for the trust-aware loader. The `mcp::config` module
    /// is private, so core call sites reach the loader through this associated
    /// function (or through an `mcp` re-export added by the runtime layer).
    pub fn load_with_trust(
        workspace_root: &Path,
        data_dir: &Path,
        trust: &WorkspaceTrust,
        plugins: &crate::plugins::SharedPluginRegistry,
    ) -> Vec<Self> {
        crate::mcp::config::load_with_trust(workspace_root, data_dir, trust, plugins)
    }
}

/// Merge same-name overrides, then filter out disabled entries (the body of load, exercised directly by unit tests)
fn merged_enabled(
    user: Vec<McpServerConfig>,
    project: Vec<McpServerConfig>,
) -> Vec<McpServerConfig> {
    merge(user, project)
        .into_iter()
        .filter(|server| !server.disabled)
        .collect()
}

/// Plugin servers only fill names the user/project configuration did not
/// define: a package can never replace a user's server (endpoint, headers or
/// credentials). The first package by id wins among plugins.
fn merge_missing(
    mut base: Vec<McpServerConfig>,
    additions: Vec<McpServerConfig>,
) -> Vec<McpServerConfig> {
    let mut taken: std::collections::BTreeSet<String> =
        base.iter().map(|server| server.name.clone()).collect();
    for server in additions {
        if taken.insert(server.name.clone()) {
            base.push(server);
        } else {
            tracing::warn!(
                server = server.name,
                "a plugin MCP server was ignored: the name is already configured by the user or project"
            );
        }
    }
    base.sort_by(|a, b| a.name.cmp(&b.name));
    base
}

/// User level as the base, project level overrides same names; output sorted by name (stable connection order)
fn merge(user: Vec<McpServerConfig>, project: Vec<McpServerConfig>) -> Vec<McpServerConfig> {
    let mut merged: HashMap<String, McpServerConfig> = HashMap::new();
    for server in user.into_iter().chain(project) {
        merged.insert(server.name.clone(), server);
    }
    let mut out: Vec<McpServerConfig> = merged.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Read a single file: missing -> empty; read failure -> logged and skipped
fn load_file(path: &Path) -> Vec<McpServerConfig> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return vec![],
        Err(e) => {
            tracing::warn!("failed to read config {}: {e}", path.display());
            return vec![];
        }
    };
    parse_file(&raw, path)
}

/// Parse file text: file-level invalid JSON skips the whole file; a single invalid server skips only that entry
fn parse_file(raw: &str, path: &Path) -> Vec<McpServerConfig> {
    let file: serde_json::Value = match serde_json::from_str(raw) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!("failed to parse config {}: {e}", path.display());
            return vec![];
        }
    };
    let Some(servers) = file.get("mcpServers").and_then(|v| v.as_object()) else {
        tracing::warn!("{} has no mcpServers object, skipped", path.display());
        return vec![];
    };
    let mut out = Vec::new();
    for (name, value) in servers {
        match parse_server(value.clone()) {
            Ok((transport, timeout, disabled)) => out.push(McpServerConfig {
                name: name.clone(),
                transport,
                timeout,
                disabled,
            }),
            Err(e) => tracing::warn!(
                "server {name} in {} is invalid, skipped: {e}",
                path.display()
            ),
        }
    }
    out
}

/// Structured validation failure for one `mcpServers` entry. Core renders
/// [`Self::detail`] into its English log line; the settings page localizes the
/// variants instead, so validation ownership stays in core while display text
/// stays in the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpEntryError {
    /// Legacy HTTP+SSE transport (not implemented).
    UnsupportedSse,
    /// An explicit `type` other than stdio/http/streamable-http.
    UnknownType(String),
    /// No `command` and no `url` to infer a transport from.
    MissingCommandOrUrl,
    /// Explicit stdio without a command.
    MissingCommand,
    /// Explicit/remote shape without a url.
    MissingUrl,
    /// The url is not an http(s) URL.
    InvalidUrl(String),
    /// The entry does not match the raw server schema at all.
    InvalidShape(String),
}

impl McpEntryError {
    /// English diagnostic for logs (the settings page localizes by variant).
    pub fn detail(&self) -> String {
        match self {
            Self::UnsupportedSse => "legacy SSE transport (2024-11-05 HTTP+SSE) is not supported; use a streamable HTTP endpoint".to_string(),
            Self::UnknownType(kind) => {
                format!("unknown type \"{kind}\" (stdio/http supported; omit to infer from url)")
            }
            Self::MissingCommandOrUrl => {
                "missing command (stdio) or url (remote)".to_string()
            }
            Self::MissingCommand => "missing command (stdio)".to_string(),
            Self::MissingUrl => "remote server config is missing url".to_string(),
            Self::InvalidUrl(url) => {
                format!("url must start with http:// or https://: {url}")
            }
            Self::InvalidShape(detail) => format!("invalid server config: {detail}"),
        }
    }
}

/// Validated shape of one entry: the transport core would use plus its
/// per-call timeout and disabled flag. The settings page projects this instead
/// of re-implementing the validation rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpEntryShape {
    pub transport: McpTransport,
    pub timeout: Duration,
    pub disabled: bool,
}

/// Validate one `mcpServers` value under exactly the criteria the loader uses.
/// Shared by [`parse_server`] and the settings page so the engine and its
/// display cannot disagree about which entries would connect.
pub fn validate_server(value: &serde_json::Value) -> Result<McpEntryShape, McpEntryError> {
    let raw: RawServer = serde_json::from_value(value.clone())
        .map_err(|error| McpEntryError::InvalidShape(error.to_string()))?;
    let disabled = raw.disabled;
    let timeout = raw
        .timeout_ms
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT);
    let declared = raw
        .kind
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase);
    let transport = match declared.as_deref() {
        Some("stdio") => stdio_transport(raw, true)?,
        Some("http") | Some("streamable-http") => http_transport(raw)?,
        Some("sse") => return Err(McpEntryError::UnsupportedSse),
        Some(other) => return Err(McpEntryError::UnknownType(other.to_string())),
        None => {
            // The url wins when both are present (the shape core has always
            // connected); the page previously inferred stdio first.
            if raw.url.is_some() {
                http_transport(raw)?
            } else {
                stdio_transport(raw, false)?
            }
        }
    };
    Ok(McpEntryShape {
        transport,
        timeout,
        disabled,
    })
}

/// A single server config -> transport shape + timeout + disabled flag: explicit type wins, otherwise inferred from url/command
fn parse_server(value: serde_json::Value) -> Result<(McpTransport, Duration, bool), String> {
    let shape = validate_server(&value).map_err(|error| error.detail())?;
    Ok((shape.transport, shape.timeout, shape.disabled))
}

fn stdio_transport(raw: RawServer, explicit: bool) -> Result<McpTransport, McpEntryError> {
    let command = raw.command.unwrap_or_default();
    if command.trim().is_empty() {
        return Err(if explicit {
            McpEntryError::MissingCommand
        } else {
            McpEntryError::MissingCommandOrUrl
        });
    }
    Ok(McpTransport::Stdio(McpStdioConfig {
        command,
        args: raw.args,
        env: raw.env,
    }))
}

fn http_transport(raw: RawServer) -> Result<McpTransport, McpEntryError> {
    let url = raw.url.unwrap_or_default();
    let url = url.trim();
    if url.is_empty() {
        return Err(McpEntryError::MissingUrl);
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(McpEntryError::InvalidUrl(url.to_string()));
    }
    let oauth = raw.oauth.and_then(|value| {
        serde_json::from_value::<McpOAuthConfig>(value)
            .ok()
            .filter(|cfg| !cfg.client_id.trim().is_empty() && !cfg.token_url.trim().is_empty())
    });
    Ok(McpTransport::Http(McpHttpConfig {
        url: url.to_string(),
        headers: raw.headers,
        oauth,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio_of<'a>(servers: &'a [McpServerConfig], name: &str) -> &'a McpStdioConfig {
        let server = servers.iter().find(|s| s.name == name).expect(name);
        match &server.transport {
            McpTransport::Stdio(stdio) => stdio,
            other => panic!("{name} should be stdio: {other:?}"),
        }
    }

    #[test]
    fn parse_claude_shape() {
        let raw = r#"{
            "mcpServers": {
                "fs": {"command": "npx", "args": ["-y", "@mcp/fs"], "env": {"A": "1"}, "timeoutMs": 60000},
                "bare": {"command": "srv"}
            }
        }"#;
        let servers = parse_file(raw, Path::new("mcp.json"));
        assert_eq!(servers.len(), 2);
        let fs = stdio_of(&servers, "fs");
        assert_eq!(fs.command, "npx");
        assert_eq!(fs.args, vec!["-y", "@mcp/fs"]);
        assert_eq!(fs.env.get("A").map(String::as_str), Some("1"));
        let fs_cfg = servers.iter().find(|s| s.name == "fs").expect("fs");
        assert_eq!(fs_cfg.timeout, Duration::from_secs(60));
        let bare = servers.iter().find(|s| s.name == "bare").expect("bare");
        assert_eq!(bare.timeout, DEFAULT_TIMEOUT);
        assert!(stdio_of(&servers, "bare").args.is_empty());
        assert!(stdio_of(&servers, "bare").env.is_empty());
    }

    #[test]
    fn parse_remote_shape_inferred_by_url() {
        let raw = r#"{
            "mcpServers": {
                "remote": {
                    "url": "https://mcp.example.com/mcp",
                    "headers": {"Authorization": "Bearer t", "X-Tenant": "a"},
                    "timeoutMs": 5000
                }
            }
        }"#;
        let servers = parse_file(raw, Path::new("mcp.json"));
        assert_eq!(servers.len(), 1);
        let McpTransport::Http(http) = &servers[0].transport else {
            panic!("url should infer http: {:?}", servers[0].transport);
        };
        assert_eq!(http.url, "https://mcp.example.com/mcp");
        assert_eq!(
            http.headers.get("Authorization").map(String::as_str),
            Some("Bearer t")
        );
        assert_eq!(http.headers.get("X-Tenant").map(String::as_str), Some("a"));
        assert_eq!(servers[0].timeout, Duration::from_secs(5));
    }

    #[test]
    fn parse_explicit_type_field() {
        let raw = r#"{"mcpServers": {
            "a": {"type": "http", "url": "http://127.0.0.1:8000/mcp"},
            "b": {"type": "stdio", "command": "srv"},
            "c": {"type": "streamable-http", "url": "https://x.example/mcp"}
        }}"#;
        let servers = parse_file(raw, Path::new("mcp.json"));
        assert_eq!(servers.len(), 3);
        assert!(matches!(servers[0].transport, McpTransport::Http(_)));
        assert!(matches!(servers[1].transport, McpTransport::Stdio(_)));
        assert!(matches!(servers[2].transport, McpTransport::Http(_)));
    }

    #[test]
    fn bad_entries_skipped_not_fatal() {
        let raw = r#"{"mcpServers": {
            "good": {"command": "srv"},
            "nocmd": {"args": []},
            "badtype": {"command": 42},
            "badurl": {"url": "ftp://x"},
            "sse": {"type": "sse", "url": "https://x.example/sse"},
            "unknowntype": {"type": "grpc", "url": "https://x.example/mcp"}
        }}"#;
        let servers = parse_file(raw, Path::new("mcp.json"));
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "good");
    }

    /// One validator is shared with the settings page: these are the exact
    /// acceptance rules both sides now use.
    #[test]
    fn validate_server_is_the_shared_validation_source() {
        let stdio = validate_server(&serde_json::json!({
            "command": "srv",
            "timeoutMs": 1500,
            "disabled": true
        }))
        .expect("stdio entry");
        assert!(matches!(stdio.transport, McpTransport::Stdio(_)));
        assert_eq!(stdio.timeout, Duration::from_millis(1500));
        assert!(stdio.disabled);

        // A url wins when both shapes are present (the engine's historical
        // inference); the settings page must project the same choice.
        let both = validate_server(&serde_json::json!({
            "command": "srv",
            "url": "https://x.example/mcp"
        }))
        .expect("both shapes");
        assert!(
            matches!(both.transport, McpTransport::Http(_)),
            "url inference wins over command when both are present"
        );

        assert_eq!(
            validate_server(&serde_json::json!({"url": "ftp://x"})).unwrap_err(),
            McpEntryError::InvalidUrl("ftp://x".into())
        );
        assert_eq!(
            validate_server(&serde_json::json!({"type": "sse", "url": "https://x"})).unwrap_err(),
            McpEntryError::UnsupportedSse
        );
        assert_eq!(
            validate_server(&serde_json::json!({"type": "grpc", "url": "https://x"})).unwrap_err(),
            McpEntryError::UnknownType("grpc".into())
        );
        assert_eq!(
            validate_server(&serde_json::json!({})).unwrap_err(),
            McpEntryError::MissingCommandOrUrl
        );
        assert_eq!(
            validate_server(&serde_json::json!({"type": "stdio"})).unwrap_err(),
            McpEntryError::MissingCommand
        );
        assert_eq!(
            validate_server(&serde_json::json!({"type": "http"})).unwrap_err(),
            McpEntryError::MissingUrl
        );
        assert!(matches!(
            validate_server(&serde_json::json!({"command": 42})),
            Err(McpEntryError::InvalidShape(_))
        ));
    }

    #[test]
    fn invalid_json_skipped() {
        assert!(parse_file("not json", Path::new("mcp.json")).is_empty());
        assert!(parse_file("{}", Path::new("mcp.json")).is_empty());
    }

    #[test]
    fn project_overrides_user() {
        let user = parse_file(
            r#"{"mcpServers": {"a": {"command": "ua"}, "b": {"command": "ub"}}}"#,
            Path::new("user/mcp.json"),
        );
        let project = parse_file(
            r#"{"mcpServers": {"b": {"url": "https://b.example/mcp"}, "c": {"command": "pc"}}}"#,
            Path::new("proj/.pigcode/mcp.json"),
        );
        let merged = merge(user, project);
        assert_eq!(merged.len(), 3);
        // b is overridden to the http shape by the project level
        let b = merged.iter().find(|s| s.name == "b").expect("b");
        assert!(matches!(b.transport, McpTransport::Http(_)));
        assert_eq!(stdio_of(&merged, "a").command, "ua");
        assert_eq!(stdio_of(&merged, "c").command, "pc");
    }

    #[test]
    fn disabled_filtered_after_merge() {
        let user = parse_file(
            r#"{"mcpServers": {
                "a": {"command": "ua"},
                "b": {"command": "ub", "disabled": true},
                "d": {"command": "ud", "disabled": false}
            }}"#,
            Path::new("user/mcp.json"),
        );
        let project = parse_file(
            r#"{"mcpServers": {"a": {"command": "pa", "disabled": true}}}"#,
            Path::new("proj/.pigcode/mcp.json"),
        );
        // Parsing keeps disabled entries (for the override merge); load semantics = merged_enabled
        assert_eq!(user.iter().filter(|s| s.disabled).count(), 1);
        let enabled = merged_enabled(user, project);
        let names: Vec<&str> = enabled.iter().map(|s| s.name.as_str()).collect();
        // a is disabled by the project-level override, b is disabled at user level; d's disabled:false explicitly enables it
        assert_eq!(names, vec!["d"]);
    }

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pig-mcp-trust-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A plugin's MCP server never replaces a user/project server of the same
    /// name; it only fills names nobody configured.
    #[test]
    fn plugin_servers_never_replace_configured_names() {
        let tmp = TempDir::new("plugin-precedence");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("workspace");
        std::fs::create_dir_all(&ws).expect("workspace");
        std::fs::create_dir_all(&data).expect("data");
        std::fs::write(
            data.join("mcp.json"),
            r#"{"mcpServers": {"docs": {"command": "user-server", "timeoutMs": 15000}}}"#,
        )
        .expect("user mcp");
        let package = tmp.0.join("package");
        std::fs::create_dir_all(&package).expect("package");
        std::fs::write(
            package.join("pig-plugin.json"),
            r#"{
              "apiVersion": 1,
              "id": "aaa.plugin",
              "name": "Plugin",
              "version": "1.0.0",
              "build": 1,
              "minHostVersion": "1.0.0",
              "protocols": {"ui": 1, "services": 1},
              "capabilities": [],
              "contributions": {
                "actions": ["provide_mcp"],
                "mcp": [
                  {"name": "docs", "config": {"command": "plugin-server", "timeoutMs": 99000}},
                  {"name": "extra", "config": {"command": "extra-server"}}
                ]
              }
            }"#,
        )
        .expect("manifest");
        let mut registry = crate::plugins::PluginRegistry::new(data.clone());
        let snapshot = registry.sync(vec![pig_protocol::PluginPackageSource {
            root: package,
            source: pig_protocol::PluginSource::User,
        }]);
        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        registry
            .set_enabled("aaa.plugin", &snapshot.packages[0].digest, true)
            .expect("enable");
        let plugins = registry.into_shared();
        let servers = load_with_trust(&ws, &data, &WorkspaceTrust::untrusted(&ws), &plugins);
        assert_eq!(
            servers
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            vec!["docs", "extra"]
        );
        let docs = servers.iter().find(|server| server.name == "docs").unwrap();
        assert_eq!(
            docs.timeout,
            std::time::Duration::from_secs(15),
            "the user's server stays in force"
        );
    }

    /// A project MCP file is repository content: no server may spawn until the
    /// workspace is trusted, and a trust token for another root admits nothing.
    #[test]
    fn project_servers_require_trust() {
        let tmp = TempDir::new("spawn");
        let data = tmp.0.join("data");
        let ws = tmp.0.join("workspace");
        let other = tmp.0.join("other");
        std::fs::create_dir_all(ws.join(".pigcode")).expect("workspace");
        std::fs::create_dir_all(&data).expect("data");
        std::fs::create_dir_all(&other).expect("other");
        std::fs::write(
            data.join("mcp.json"),
            r#"{"mcpServers": {"user": {"command": "user-server"}}}"#,
        )
        .expect("user mcp");
        std::fs::write(
            ws.join(".pigcode/mcp.json"),
            r#"{"mcpServers": {"project": {"command": "project-server"}}}"#,
        )
        .expect("project mcp");

        let names = |servers: Vec<McpServerConfig>| {
            servers
                .into_iter()
                .map(|server| server.name)
                .collect::<Vec<_>>()
        };
        let plugins = crate::plugins::PluginRegistry::shared(data.clone());
        assert_eq!(names(load(&ws, &data, &plugins)), vec!["user"]);
        assert_eq!(
            names(load_with_trust(
                &ws,
                &data,
                &WorkspaceTrust::new(&ws, /*trusted*/ false),
                &plugins,
            )),
            vec!["user"]
        );
        assert_eq!(
            names(load_with_trust(
                &ws,
                &data,
                &WorkspaceTrust::new(&ws, /*trusted*/ true),
                &plugins,
            )),
            vec!["project", "user"]
        );
        assert_eq!(
            names(load_with_trust(
                &ws,
                &data,
                &WorkspaceTrust::new(&other, /*trusted*/ true),
                &plugins,
            )),
            vec!["user"],
            "a token for another root must not admit this workspace"
        );
    }
}
