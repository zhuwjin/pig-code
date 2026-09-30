//! MCP server 配置加载：Claude Code 兼容形态
//! `{ "mcpServers": { "<name>": { "command", "args", "env", "timeoutMs" } } }`（stdio）
//! 与远程形态 `{ "url", "headers", "timeoutMs" }`（streamable HTTP）。
//! `type` 字段可省略：有 `url` 按远程、有 `command` 按 stdio 推断；
//! 显式 `"type": "stdio" | "http"` 时按声明校验。`"type": "sse"`（2024-11-05
//! legacy HTTP+SSE）暂不支持，记录后跳过。
//! `"disabled": true` 停用条目：解析保留、合并（覆盖同名）后过滤，不参与连接。
//! 用户级 `<data_dir>/mcp.json` 为底，项目级 `<workspace>/.pigcode/mcp.json` 覆盖同名。

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// 单次调用（initialize/tools/list/tools/call/ping）默认超时
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub timeout: Duration,
    /// `disabled: true` 停用：解析/合并保留该条（项目级可停用用户级同名条目），
    /// 连接阶段整体过滤
    pub disabled: bool,
}

/// 传输配置：stdio 子进程 / streamable HTTP 远程端点
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
    /// 自定义请求头（每个 POST/DELETE 都携带；鉴权 token 走这里，OAuth 后续项）
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

/// 加载并合并两个来源；文件缺失/非法不 panic，记录后跳过；
/// `disabled` 停用在覆盖合并后生效（项目级停用用户级同名条目）
pub fn load(workspace_root: &Path, data_dir: &Path) -> Vec<McpServerConfig> {
    let user = load_file(&data_dir.join("mcp.json"));
    let project = load_file(&workspace_root.join(".pigcode").join("mcp.json"));
    merged_enabled(user, project)
}

/// 合并同名覆盖后过滤停用条目（load 的主体，单测直击）
fn merged_enabled(
    user: Vec<McpServerConfig>,
    project: Vec<McpServerConfig>,
) -> Vec<McpServerConfig> {
    merge(user, project)
        .into_iter()
        .filter(|server| !server.disabled)
        .collect()
}

/// 用户级为底、项目级覆盖同名；输出按名字排序（连接顺序稳定）
fn merge(user: Vec<McpServerConfig>, project: Vec<McpServerConfig>) -> Vec<McpServerConfig> {
    let mut merged: HashMap<String, McpServerConfig> = HashMap::new();
    for server in user.into_iter().chain(project) {
        merged.insert(server.name.clone(), server);
    }
    let mut out: Vec<McpServerConfig> = merged.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 读单个文件：缺失 → 空；读失败 → 记录并跳过
fn load_file(path: &Path) -> Vec<McpServerConfig> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return vec![],
        Err(e) => {
            eprintln!("[mcp] 读取配置失败 {}: {e}", path.display());
            return vec![];
        }
    };
    parse_file(&raw, path)
}

/// 解析文件文本：文件级 JSON 非法整文件跳过；单个 server 非法只跳过该条
fn parse_file(raw: &str, path: &Path) -> Vec<McpServerConfig> {
    let file: serde_json::Value = match serde_json::from_str(raw) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("[mcp] 解析配置失败 {}: {e}", path.display());
            return vec![];
        }
    };
    let Some(servers) = file.get("mcpServers").and_then(|v| v.as_object()) else {
        eprintln!("[mcp] {} 缺少 mcpServers 对象，已跳过", path.display());
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
            Err(e) => eprintln!(
                "[mcp] {} 中 server {name} 配置非法，已跳过: {e}",
                path.display()
            ),
        }
    }
    out
}

/// 单条 server 配置 → 传输形态 + 超时 + 停用标记：显式 type 优先，否则按 url/command 推断
fn parse_server(value: serde_json::Value) -> Result<(McpTransport, Duration, bool), String> {
    let raw: RawServer = serde_json::from_value(value).map_err(|e| format!("配置非法: {e}"))?;
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
            "legacy SSE（2024-11-05 HTTP+SSE）传输暂不支持，请用 streamable HTTP 端点".to_string(),
        ),
        Some(other) => Err(format!(
            "未知 type \"{other}\"（支持 stdio/http，可省略按 url 推断）"
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
        return Err("缺少 command（stdio 形态）或 url（远程形态）".to_string());
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
        return Err("远程形态缺少 url".to_string());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(format!("url 须以 http:// 或 https:// 开头: {url}"));
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
            other => panic!("{name} 应为 stdio: {other:?}"),
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
            panic!("应按 url 推断为 http: {:?}", servers[0].transport);
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
        // b 被项目级覆盖成 http 形态
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
        // 解析保留停用条目（供覆盖合并）；load 语义 = merged_enabled
        assert_eq!(user.iter().filter(|s| s.disabled).count(), 1);
        let enabled = merged_enabled(user, project);
        let names: Vec<&str> = enabled.iter().map(|s| s.name.as_str()).collect();
        // a 被项目级停用覆盖、b 用户级停用；d 的 disabled:false 显式启用
        assert_eq!(names, vec!["d"]);
    }
}
