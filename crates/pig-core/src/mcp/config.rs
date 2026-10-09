//! MCP server config loading: Claude Code-compatible shape
//! `{ "mcpServers": { "<name>": { "command", "args", "env", "timeoutMs" } } }` (stdio)
//! and the remote shape `{ "url", "headers", "timeoutMs" }` (streamable HTTP).
//! The `type` field may be omitted: presence of `url` infers remote, presence of `command` infers stdio;
//! an explicit `"type": "stdio" | "http"` is validated as declared. `"type": "sse"` (2024-11-05
//! legacy HTTP+SSE) is not yet supported; it is logged and skipped.
//! `"disabled": true` disables an entry: kept through parsing, filtered after merging (same-name override), excluded from connecting.
//! User-level `<data_dir>/mcp.json` is the base; project-level `<workspace>/.pigcode/mcp.json` overrides same-name entries.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

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
#[derive(Debug, Clone)]
pub enum McpTransport {
    Stdio(McpStdioConfig),
    Http(McpHttpConfig),
}

#[derive(Debug, Clone)]
pub struct McpStdioConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct McpHttpConfig {
    pub url: String,
    /// Custom request headers (carried on every POST/DELETE; auth tokens go here, OAuth is a future item)
    pub headers: HashMap<String, String>,
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
    #[serde(rename = "timeoutMs")]
    timeout_ms: Option<u64>,
    #[serde(default)]
    disabled: bool,
}

/// Load and merge both sources; a missing/invalid file does not panic but is logged and skipped;
/// `disabled` takes effect after the override merge (project level can disable a user-level same-name entry)
pub fn load(workspace_root: &Path, data_dir: &Path) -> Vec<McpServerConfig> {
    let user = load_file(&data_dir.join("mcp.json"));
    let project = load_file(&workspace_root.join(".pigcode").join("mcp.json"));
    merged_enabled(user, project)
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

/// A single server config -> transport shape + timeout + disabled flag: explicit type wins, otherwise inferred from url/command
fn parse_server(value: serde_json::Value) -> Result<(McpTransport, Duration, bool), String> {
    let raw: RawServer =
        serde_json::from_value(value).map_err(|e| format!("invalid server config: {e}"))?;
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
        Some("stdio") => stdio_transport(raw),
        Some("http") | Some("streamable-http") => http_transport(raw),
        Some("sse") => Err(
            "legacy SSE transport (2024-11-05 HTTP+SSE) is not supported; use a streamable HTTP endpoint".to_string(),
        ),
        Some(other) => Err(format!(
            "unknown type \"{other}\" (stdio/http supported; omit to infer from url)"
        )),
        None => {
            if raw.url.is_some() {
                http_transport(raw)
            } else {
                stdio_transport(raw)
            }
        }
    }?;
    Ok((transport, timeout, disabled))
}

fn stdio_transport(raw: RawServer) -> Result<McpTransport, String> {
    let command = raw.command.unwrap_or_default();
    if command.trim().is_empty() {
        return Err("missing command (stdio) or url (remote)".to_string());
    }
    Ok(McpTransport::Stdio(McpStdioConfig {
        command,
        args: raw.args,
        env: raw.env,
    }))
}

fn http_transport(raw: RawServer) -> Result<McpTransport, String> {
    let url = raw.url.unwrap_or_default();
    let url = url.trim();
    if url.is_empty() {
        return Err("remote server config is missing url".to_string());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!("url must start with http:// or https://: {url}"));
    }
    Ok(McpTransport::Http(McpHttpConfig {
        url: url.to_string(),
        headers: raw.headers,
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
}
