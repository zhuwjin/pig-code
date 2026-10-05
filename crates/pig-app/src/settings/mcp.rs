use super::*;
use std::path::{Path, PathBuf};

/// 与 core mcp::config::DEFAULT_TIMEOUT 对齐（配置缺省时的展示值）
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// 空配置时的引导示例（core 支持的 stdio 形态）
const MCP_EXAMPLE: &str = r#"{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/dir"]
    }
  }
}"#;

/// MCP 页作用域（对齐 ZCode PluginScopeMenu）：用户级 + 工作区清单二选一
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum McpScope {
    /// 只看/只写用户级 `<data_dir>/mcp.json`（对所有工作区生效）
    User,
    /// 工作区：查看用户级 + 该工作区 `.pigcode/mcp.json` 的合并（同名覆盖）
    Workspace(PathBuf),
}

/// 工作区显示名：别名优先，否则取目录名，最后回落完整路径
pub(crate) fn workspace_display_name(path: &Path, alias: Option<&str>) -> String {
    if let Some(alias) = alias.filter(|s| !s.trim().is_empty()) {
        return alias.to_string();
    }
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpSource {
    User,
    Project,
}

impl McpSource {
    fn label(self) -> &'static str {
        match self {
            Self::User => "用户级",
            Self::Project => "项目级",
        }
    }
}

/// 对话框可选的传输类型（core 不支持 legacy SSE，只有这两种）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpTransportKind {
    Stdio,
    Http,
}

impl McpTransportKind {
    fn label(self) -> &'static str {
        match self {
            Self::Stdio => "stdio（本地命令）",
            Self::Http => "HTTP（远程端点）",
        }
    }
}

/// server 传输形态：stdio（command/args）或远程（url）——列表展示投影
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum McpTransport {
    Stdio { command: String, args: Vec<String> },
    Remote { url: String },
}

impl McpTransport {
    fn kind(&self) -> McpTransportKind {
        match self {
            Self::Stdio { .. } => McpTransportKind::Stdio,
            Self::Remote { .. } => McpTransportKind::Http,
        }
    }

    fn chip_label(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Remote { .. } => "http",
        }
    }

    fn summary(&self) -> String {
        match self {
            Self::Stdio { command, args } => {
                let mut line = command.clone();
                for arg in args {
                    line.push(' ');
                    line.push_str(arg);
                }
                line
            }
            Self::Remote { url } => url.clone(),
        }
    }
}

/// 一个 mcp.json server 条目的展示投影：raw 保留完整 JSON（编辑回写保真），
/// 解析结论放 transport / invalid_reason（与 core config::parse_server 同口径）
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct McpServerInfo {
    pub name: String,
    pub source: McpSource,
    /// 条目原始 JSON（含 env/headers/type 及 oauth 等未知字段）
    pub raw: serde_json::Value,
    pub transport: Option<McpTransport>,
    /// `disabled: true` 停用：core 不连接该条
    pub disabled: bool,
    pub timeout_ms: Option<u64>,
    /// 条目解析失败原因（core 连接时会跳过该条）
    pub invalid_reason: Option<String>,
    /// 项目级条目覆盖了用户级同名条目
    pub overrides_user: bool,
}

/// 两个配置来源文件 + 合并后的 server 清单（设置页 MCP 数据快照）
pub(crate) struct McpConfigSnapshot {
    pub user_path: PathBuf,
    pub user_exists: bool,
    /// None = 无活动会话（未确定工作区，项目级不参与）
    pub project_path: Option<PathBuf>,
    pub project_exists: bool,
    pub servers: Vec<McpServerInfo>,
}

/// 读用户级 `<data_dir>/mcp.json` + 项目级 `<workspace>/.pigcode/mcp.json` 并合并
pub(crate) fn load_mcp_snapshot(workspace: Option<&Path>) -> McpConfigSnapshot {
    let user_path = pig_core::data_dir().join("mcp.json");
    let project_path = workspace.map(|root| root.join(".pigcode").join("mcp.json"));
    let user = read_servers(&user_path, McpSource::User);
    let project = project_path
        .as_ref()
        .map(|path| read_servers(path, McpSource::Project))
        .unwrap_or_default();
    McpConfigSnapshot {
        project_exists: project_path.as_ref().is_some_and(|p| p.is_file()),
        servers: merge_servers(user, project),
        user_exists: user_path.is_file(),
        user_path,
        project_path,
    }
}

fn read_servers(path: &Path, source: McpSource) -> Vec<McpServerInfo> {
    std::fs::read_to_string(path)
        .ok()
        .map(|raw| parse_servers(&raw, source))
        .unwrap_or_default()
}

/// 解析单个 mcp.json：文件级非法返回空；条目级非法保留该条并标注原因（供编辑修复）。
/// 校验口径与 core config::parse_server 一致（显式 type 优先，否则按 url/command 推断）
fn parse_servers(raw: &str, source: McpSource) -> Vec<McpServerInfo> {
    let Ok(file) = serde_json::from_str::<serde_json::Value>(raw) else {
        return vec![];
    };
    let Some(servers) = file.get("mcpServers").and_then(|v| v.as_object()) else {
        return vec![];
    };
    servers
        .iter()
        .map(|(name, value)| parse_server_entry(name, value, source))
        .collect()
}

fn parse_server_entry(name: &str, value: &serde_json::Value, source: McpSource) -> McpServerInfo {
    let non_empty = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
    };
    let declared = value
        .get("type")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .map(str::to_ascii_lowercase);
    let (transport, invalid_reason) = match declared.as_deref() {
        Some("stdio") => stdio_projection(value),
        Some("http") | Some("streamable-http") => remote_projection(value),
        Some("sse") => (
            None,
            Some("legacy SSE（HTTP+SSE）传输暂不支持，请用 streamable HTTP 端点".to_string()),
        ),
        Some(other) => (
            None,
            Some(format!("未知 type \"{other}\"（支持 stdio/http）")),
        ),
        None => match (non_empty("command"), non_empty("url")) {
            (Some(_), _) => stdio_projection(value),
            (None, Some(_)) => remote_projection(value),
            (None, None) => (
                None,
                Some("缺少 command（stdio 形态）或 url（远程形态）".to_string()),
            ),
        },
    };
    McpServerInfo {
        name: name.to_string(),
        source,
        raw: value.clone(),
        transport,
        disabled: value
            .get("disabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        timeout_ms: value
            .get("timeoutMs")
            .and_then(|v| v.as_u64())
            .filter(|&ms| ms > 0),
        invalid_reason,
        overrides_user: false,
    }
}

fn stdio_projection(value: &serde_json::Value) -> (Option<McpTransport>, Option<String>) {
    let Some(command) = value
        .get("command")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    else {
        return (None, Some("stdio 形态缺少 command".to_string()));
    };
    let args = value
        .get("args")
        .and_then(|v| v.as_array())
        .map(|args| {
            args.iter()
                .filter_map(|arg| arg.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (
        Some(McpTransport::Stdio {
            command: command.to_string(),
            args,
        }),
        None,
    )
}

fn remote_projection(value: &serde_json::Value) -> (Option<McpTransport>, Option<String>) {
    let Some(url) = value
        .get("url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    else {
        return (None, Some("远程形态缺少 url".to_string()));
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return (
            None,
            Some(format!("url 须以 http:// 或 https:// 开头: {url}")),
        );
    }
    (
        Some(McpTransport::Remote {
            url: url.to_string(),
        }),
        None,
    )
}

/// 用户级为底、项目级覆盖同名（与 core 合并口径一致）；输出按名字排序。
/// 非法/停用条目也保留（页面负责标注与编辑修复）
fn merge_servers(user: Vec<McpServerInfo>, project: Vec<McpServerInfo>) -> Vec<McpServerInfo> {
    let mut merged = user;
    for mut server in project {
        if let Some(existing) = merged.iter_mut().find(|s| s.name == server.name) {
            server.overrides_user = true;
            *existing = server;
        } else {
            merged.push(server);
        }
    }
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    merged
}

// ---------- mcp.json 写入（新建/编辑/删除/启停都落盘，之后整页刷新） ----------

/// 读 mcp.json 的 mcpServers 对象（文件缺失/无 mcpServers → 空对象）
fn read_servers_object(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|file| file.get("mcpServers").and_then(|v| v.as_object()).cloned())
        .unwrap_or_default()
}

/// 写回 mcp.json：保留 mcpServers 之外的文件级字段；既有内容非法时拒绝改写（不静默覆盖），
/// 父目录缺失则创建
fn write_servers_object(
    path: &Path,
    servers: serde_json::Map<String, serde_json::Value>,
) -> std::io::Result<()> {
    let mut file = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw).map_err(|e| {
            std::io::Error::other(format!("既有 mcp.json 不是合法 JSON，拒绝改写: {e}"))
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e),
    };
    if !file.is_object() {
        return Err(std::io::Error::other(
            "既有 mcp.json 顶层不是对象，拒绝改写",
        ));
    }
    file["mcpServers"] = serde_json::Value::Object(servers);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(&file)?;
    text.push('\n');
    std::fs::write(path, text)
}

/// 新建/覆盖一个 server 条目
fn upsert_mcp_server(path: &Path, name: &str, entry: serde_json::Value) -> std::io::Result<()> {
    let mut servers = read_servers_object(path);
    servers.insert(name.to_string(), entry);
    write_servers_object(path, servers)
}

/// 删除一个 server 条目（文件缺失/条目不存在视为成功）
fn delete_mcp_server(path: &Path, name: &str) -> std::io::Result<()> {
    let mut servers = read_servers_object(path);
    servers.remove(name);
    write_servers_object(path, servers)
}

/// 启用/停用一个 server 条目（disabled: true / 移除该键）
fn set_mcp_disabled(path: &Path, name: &str, disabled: bool) -> std::io::Result<()> {
    let mut servers = read_servers_object(path);
    let Some(entry) = servers.get_mut(name) else {
        return Ok(());
    };
    if let Some(map) = entry.as_object_mut() {
        if disabled {
            map.insert("disabled".to_string(), serde_json::json!(true));
        } else {
            map.remove("disabled");
        }
    }
    write_servers_object(path, servers)
}

fn format_timeout(timeout_ms: Option<u64>) -> String {
    match timeout_ms {
        None => format!("超时 {}s（默认）", DEFAULT_TIMEOUT_MS / 1000),
        Some(ms) if ms % 1000 == 0 => format!("超时 {}s", ms / 1000),
        Some(ms) => format!("超时 {ms}ms"),
    }
}

/// JSON 对象草稿的展示/回填格式：空对象或缺省 → "{}"
fn pretty_object(value: Option<&serde_json::Value>) -> String {
    match value.filter(|v| v.as_object().is_some_and(|m| !m.is_empty())) {
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string()),
        None => "{}".to_string(),
    }
}

/// 环境变量/请求头草稿文本 → JSON 对象（空文本/空对象 → 空 map）
fn parse_object_json(
    raw: &str,
    label: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Default::default());
    }
    let parsed: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("{label}不是合法 JSON: {e}"))?;
    match parsed {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(format!("{label}必须是 {{\"KEY\": \"value\"}} 形式的对象")),
    }
}

/// 表单 → (名称, 条目 JSON)。以编辑底稿为底覆盖表单字段：oauth 等未知字段保真回写
fn entry_from_form(dialog: &McpDialog, cx: &App) -> Result<(String, serde_json::Value), String> {
    let name = dialog.name.read(cx).value().trim().to_string();
    if name.is_empty() {
        return Err("名称不能为空".to_string());
    }
    let mut entry = if dialog.base.is_object() {
        dialog.base.clone()
    } else {
        serde_json::json!({})
    };
    let map = entry.as_object_mut().expect("base 为对象或空对象");
    // command/url 二选一即可推断，去掉残留 type 避免与表单选择冲突
    map.remove("type");
    match dialog.kind {
        McpTransportKind::Stdio => {
            let command = dialog.command.read(cx).value().trim().to_string();
            if command.is_empty() {
                return Err("stdio 形态需要填写命令".to_string());
            }
            map.insert("command".to_string(), serde_json::json!(command));
            let args_text = dialog.args.read(cx).value().to_string();
            let args: Vec<&str> = args_text.split_whitespace().collect();
            if args.is_empty() {
                map.remove("args");
            } else {
                map.insert("args".to_string(), serde_json::json!(args));
            }
            map.remove("url");
            map.remove("headers");
            let env = parse_object_json(&dialog.env_draft, "环境变量")?;
            if env.is_empty() {
                map.remove("env");
            } else {
                map.insert("env".to_string(), serde_json::Value::Object(env));
            }
        }
        McpTransportKind::Http => {
            let url = dialog.url.read(cx).value().trim().to_string();
            if url.is_empty() {
                return Err("HTTP 形态需要填写 URL".to_string());
            }
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err("URL 须以 http:// 或 https:// 开头".to_string());
            }
            map.insert("url".to_string(), serde_json::json!(url));
            map.remove("command");
            map.remove("args");
            map.remove("env");
            let headers = parse_object_json(&dialog.headers_draft, "请求头")?;
            if headers.is_empty() {
                map.remove("headers");
            } else {
                map.insert("headers".to_string(), serde_json::Value::Object(headers));
            }
        }
    }
    let timeout_raw = dialog.timeout.read(cx).value().trim().to_string();
    if timeout_raw.is_empty() {
        map.remove("timeoutMs");
    } else {
        let ms: u64 = timeout_raw
            .parse()
            .map_err(|_| "超时必须是正整数（毫秒）".to_string())?;
        if ms == 0 {
            return Err("超时必须是正整数（毫秒）".to_string());
        }
        map.insert("timeoutMs".to_string(), serde_json::json!(ms));
    }
    Ok((name, entry))
}

/// JSON 模式 → (名称, 条目 JSON)。兼容 `{"server-name": {...}}` 与
/// Claude 风格 `{"mcpServers": {...}}`；一次只编辑一个 server
fn entry_from_json(dialog: &McpDialog, cx: &App) -> Result<(String, serde_json::Value), String> {
    let raw = dialog.json_text.read(cx).value().trim().to_string();
    if raw.is_empty() {
        return Err("JSON 配置不能为空".to_string());
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("不是合法 JSON: {e}"))?;
    let mut obj = parsed
        .as_object()
        .cloned()
        .ok_or_else(|| "顶层必须是对象".to_string())?;
    if let Some(inner) = obj.get("mcpServers") {
        obj = inner
            .as_object()
            .cloned()
            .ok_or_else(|| "mcpServers 字段必须是对象".to_string())?;
    }
    if obj.len() != 1 {
        return Err("JSON 模式一次只编辑一个 server（顶层恰好一个键值对）".to_string());
    }
    let (name, entry) = obj.into_iter().next().expect("len == 1");
    if name.trim().is_empty() {
        return Err("服务器名（JSON 键）不能为空".to_string());
    }
    if !entry.is_object() {
        return Err("server 配置必须是对象".to_string());
    }
    if let Some(editing) = &dialog.editing
        && name != *editing
    {
        return Err(format!("编辑时不能改名（JSON 键必须是 \"{editing}\"）"));
    }
    let has = |key: &str| {
        entry
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
    };
    if !has("command") && !has("url") {
        return Err("配置缺少 command（stdio 形态）或 url（远程形态）".to_string());
    }
    Ok((name, entry))
}

/// JSON 解析成功后回填表单（JSON → 表单切换用）
fn refill_mcp_form(
    dialog: &mut McpDialog,
    name: &str,
    entry: &serde_json::Value,
    window: &mut Window,
    cx: &mut App,
) {
    dialog.base = entry.clone();
    dialog.kind = if entry.get("url").is_some() {
        McpTransportKind::Http
    } else {
        McpTransportKind::Stdio
    };
    let command = entry
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let args = entry
        .get("args")
        .and_then(|v| v.as_array())
        .map(|args| {
            args.iter()
                .filter_map(|arg| arg.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let url = entry
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let timeout = entry
        .get("timeoutMs")
        .and_then(|v| v.as_u64())
        .map(|ms| ms.to_string())
        .unwrap_or_default();
    dialog.env_draft = pretty_object(entry.get("env"));
    dialog.headers_draft = pretty_object(entry.get("headers"));
    let draft = match dialog.kind {
        McpTransportKind::Stdio => dialog.env_draft.clone(),
        McpTransportKind::Http => dialog.headers_draft.clone(),
    };
    dialog
        .name
        .update(cx, |i, cx| i.set_value(name.to_string(), window, cx));
    dialog
        .command
        .update(cx, |i, cx| i.set_value(command, window, cx));
    dialog
        .args
        .update(cx, |i, cx| i.set_value(args, window, cx));
    dialog.url.update(cx, |i, cx| i.set_value(url, window, cx));
    dialog
        .timeout
        .update(cx, |i, cx| i.set_value(timeout, window, cx));
    dialog
        .env_headers
        .update(cx, |i, cx| i.set_value(draft, window, cx));
}

/// 环境变量是否「已配置」（与 core tool/websearch.rs 同口径：存在且非空白）
fn api_key_configured(raw: Option<String>) -> bool {
    raw.is_some_and(|value| !value.trim().is_empty())
}

/// (tavily, brave) 配置状态；同时配置时 Tavily 优先
fn websearch_backends() -> (bool, bool) {
    (
        api_key_configured(std::env::var("TAVILY_API_KEY").ok()),
        api_key_configured(std::env::var("BRAVE_API_KEY").ok()),
    )
}

impl SettingsView {
    /// 刷新 MCP 页：作废状态查询结果，AppView 收到事件后重读配置并重新查询 core
    pub(crate) fn refresh_mcp(&mut self, cx: &mut Context<Self>) {
        self.mcp_connection = None;
        cx.emit(SettingsEvent::RefreshMcp);
        cx.notify();
    }

    /// AppView 刷新 MCP 快照时读取：当前选中的作用域
    pub(crate) fn mcp_scope(&self) -> &McpScope {
        &self.mcp_scope
    }

    /// 工作区清单喂入（侧栏同口径：可见工作区 ∪ 会话 cwd，含显示名）
    pub(crate) fn set_scope_workspaces(
        &mut self,
        workspaces: Vec<(PathBuf, String)>,
        cx: &mut Context<Self>,
    ) {
        self.scope_workspaces = workspaces;
        // 归档页工作区过滤下拉的选项随该清单重建（render 前经脏标记同步）
        self.archived_ws_dirty = true;
        cx.notify();
    }

    /// 连接状态是否适用于当前查看的配置：固定工作区 ≠ 会话工作区时不适用
    fn mcp_status_applicable(&self) -> bool {
        match &self.mcp_scope {
            // 用户级条目参与会话连接（session 连的是 用户级+会话工作区 的合并）
            McpScope::User => true,
            McpScope::Workspace(path) => self.session_cwd.as_deref() == Some(path.as_path()),
        }
    }

    /// 切换作用域：换目标后整页刷新（AppView 按新作用域重读快照）
    fn set_mcp_scope(&mut self, scope: McpScope, cx: &mut Context<Self>) {
        self.mcp_scope_popup = false;
        if self.mcp_scope == scope {
            cx.notify();
            return;
        }
        self.mcp_scope = scope;
        self.refresh_mcp(cx);
    }

    /// 作用域选择器（pill 按钮；下拉经 deferred 弹层，列表区滚动不裁剪）
    pub(crate) fn render_mcp_scope(&self, cx: &mut Context<Self>) -> AnyElement {
        let (icon, label) = match &self.mcp_scope {
            McpScope::User => (IconName::User, "用户级".to_string()),
            McpScope::Workspace(path) => {
                let alias = self
                    .scope_workspaces
                    .iter()
                    .find(|(p, _)| p == path)
                    .map(|(_, name)| name.clone());
                (
                    IconName::Folder,
                    workspace_display_name(path, alias.as_deref()),
                )
            }
        };
        div()
            .on_prepaint({
                let cell = self.mcp_scope_btn_bounds.clone();
                move |bounds, _, _| cell.set(bounds)
            })
            .child(
                Button::new("mcp-scope")
                    .outline()
                    .small()
                    .icon(icon)
                    .label(label)
                    .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                        // 弹层打开时点按钮：按下先触发弹层 outside-close（记录按下位置），
                        // 紧随的 click 按同一位置吞掉，避免收起又马上弹开（API 格式弹层同款）
                        let down_pos = match event {
                            ClickEvent::Mouse(e) => Some(e.down.position),
                            _ => None,
                        };
                        if this
                            .mcp_scope_outside_close
                            .take()
                            .is_some_and(|pos| Some(pos) == down_pos)
                        {
                            return;
                        }
                        this.mcp_scope_popup = !this.mcp_scope_popup;
                        cx.notify();
                    })),
            )
            .when(self.mcp_scope_popup, |this| {
                this.child(self.render_mcp_scope_popup(cx))
            })
            .into_any_element()
    }

    /// 作用域下拉：用户级 + 工作区清单（会话所在工作区带「当前会话」标记）
    fn render_mcp_scope_popup(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut popup = v_flex()
            .id("mcp-scope-popup")
            .w(px(380.))
            .max_h(px(320.))
            .overflow_y_scroll()
            .py_1()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                this.mcp_scope_popup = false;
                this.mcp_scope_outside_close = Some(event.position);
                cx.notify();
            }))
            .child(
                // 用户级：选中不打高亮，行尾打勾（ZCode 同款）；hover 圆角描边高亮
                h_flex()
                    .id("mcp-scope-user")
                    .gap_2()
                    .mx_1()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(gpui_kit::black().opacity(0.))
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().accent).border_color(cx.theme().border))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_mcp_scope(McpScope::User, cx);
                    }))
                    .child(
                        Icon::new(IconName::User)
                            .size_4()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().text_sm().child("用户级"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child("全局配置，对所有工作区生效"),
                            ),
                    )
                    .when(self.mcp_scope == McpScope::User, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .size_4()
                                .flex_shrink_0()
                                .text_color(cx.theme().primary),
                        )
                    }),
            );
        if !self.scope_workspaces.is_empty() {
            popup = popup.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("工作区"),
            );
            for (path, display) in self.scope_workspaces.clone() {
                let selected = self.mcp_scope == McpScope::Workspace(path.clone());
                let is_session_ws = self.session_cwd.as_deref() == Some(path.as_path());
                let path_text = path.display().to_string();
                popup = popup.child(
                    h_flex()
                        .id(gpui_kit::SharedString::from(path_text.clone()))
                        .gap_2()
                        .mx_1()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(gpui_kit::black().opacity(0.))
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().accent).border_color(cx.theme().border))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_mcp_scope(McpScope::Workspace(path.clone()), cx);
                        }))
                        .child(
                            Icon::new(IconName::Folder)
                                .size_4()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .min_w_0()
                                        .child(div().text_sm().truncate().child(display.clone()))
                                        .when(is_session_ws, |this| {
                                            this.child(
                                                div()
                                                    .text_xs()
                                                    .px_1()
                                                    .rounded_sm()
                                                    .bg(cx.theme().accent)
                                                    .flex_shrink_0()
                                                    .child("当前会话"),
                                            )
                                        }),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .truncate()
                                        .child(path_text),
                                ),
                        )
                        .when(selected, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .size_4()
                                    .flex_shrink_0()
                                    .text_color(cx.theme().primary),
                            )
                        }),
                );
            }
        }
        deferred(
            Positioner::side(self.mcp_scope_btn_bounds.get())
                .placement(Placement::Bottom)
                .align(Align::Start)
                .offset(px(4.))
                .margin(px(8.))
                .occlude()
                .child(popup),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// 对话框目标文件路径（按作用域）。项目级条目只在项目路径已知（已打开会话）时
    /// 才会出现；若快照已刷新为无会话状态则返回 None，调用方放弃写入
    fn mcp_target_path(snapshot: &McpConfigSnapshot, scope: McpSource) -> Option<PathBuf> {
        match scope {
            McpSource::User => Some(snapshot.user_path.clone()),
            McpSource::Project => snapshot.project_path.clone(),
        }
    }

    /// 启停开关：把 disabled 写进条目所在文件，然后整页刷新
    fn toggle_mcp_server(
        &mut self,
        name: &str,
        source: McpSource,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(snapshot) = &self.mcp_snapshot else {
            return;
        };
        let Some(path) = Self::mcp_target_path(snapshot, source) else {
            return;
        };
        let name = name.to_string();
        match set_mcp_disabled(&path, &name, !enabled) {
            Ok(()) => {
                self.mcp_write_error = None;
                self.refresh_mcp(cx);
            }
            Err(e) => {
                self.mcp_write_error = Some(format!("写入 {} 失败：{e}", path.display()));
                cx.notify();
            }
        }
    }

    /// 打开新建（name=None）/编辑对话框，按现有条目预填
    pub(crate) fn open_mcp_dialog(
        &mut self,
        name: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshot = self.mcp_snapshot.as_ref();
        let existing = name
            .and_then(|n| snapshot.and_then(|s| s.servers.iter().find(|server| server.name == n)));
        let scope = existing
            .map(|server| server.source)
            .unwrap_or(McpSource::User);
        let base = existing
            .map(|server| server.raw.clone())
            .unwrap_or_else(|| serde_json::json!({}));
        let kind = existing
            .and_then(|server| server.transport.as_ref())
            .map(McpTransport::kind)
            .unwrap_or(McpTransportKind::Stdio);
        let (command, args) = match existing.and_then(|server| server.transport.as_ref()) {
            Some(McpTransport::Stdio { command, args }) => (command.clone(), args.join(" ")),
            _ => (String::new(), String::new()),
        };
        let url = match existing.and_then(|server| server.transport.as_ref()) {
            Some(McpTransport::Remote { url }) => url.clone(),
            _ => String::new(),
        };
        let timeout = existing
            .and_then(|server| server.timeout_ms)
            .map(|ms| ms.to_string())
            .unwrap_or_default();
        // 项目级目标的工作区名（快照项目路径上溯两级 = 工作区根；显示名走别名表）
        let project_root = snapshot
            .and_then(|s| s.project_path.as_ref())
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(Path::to_path_buf);
        let project_workspace = project_root.map(|root| {
            let alias = self
                .scope_workspaces
                .iter()
                .find(|(path, _)| *path == root)
                .map(|(_, name)| name.clone());
            workspace_display_name(&root, alias.as_deref())
        });
        let env_draft = pretty_object(base.get("env"));
        let headers_draft = pretty_object(base.get("headers"));
        let env_headers_default = match kind {
            McpTransportKind::Stdio => env_draft.clone(),
            McpTransportKind::Http => headers_draft.clone(),
        };
        let json_default = serde_json::to_string_pretty(&serde_json::json!({
            name.unwrap_or("server-name"): base,
        }))
        .unwrap_or_default();
        let dialog = McpDialog {
            editing: name.map(str::to_string),
            scope,
            project_available: snapshot.is_some_and(|s| s.project_path.is_some()),
            kind,
            name: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("例如 filesystem")
                    .default_value(name.map(str::to_string).unwrap_or_default())
            }),
            command: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("npx")
                    .default_value(command)
            }),
            args: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("-y @modelcontextprotocol/server-filesystem /path/to/dir")
                    .default_value(args)
            }),
            url: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("https://mcp.example.com/mcp")
                    .default_value(url)
            }),
            timeout: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("30000（默认）")
                    .default_value(timeout)
            }),
            project_workspace,
            advanced_open: false,
            env_headers: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(3, 8)
                    .default_value(env_headers_default)
            }),
            env_draft,
            headers_draft,
            json_mode: false,
            json_text: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(12, 20)
                    .default_value(json_default)
            }),
            delete_armed: false,
            base,
            error: None,
        };
        // 环境变量/请求头草稿随编辑框内容变化（切换传输类型时交换显示）
        let textarea = dialog.env_headers.clone();
        self._subscriptions.push(cx.subscribe_in(
            &textarea,
            window,
            |this: &mut Self, emitter, event: &gpui_kit::component::input::InputEvent, _, cx| {
                let Some(dialog) = &this.mcp_dialog else {
                    return;
                };
                if dialog.env_headers.entity_id() != emitter.entity_id() {
                    return;
                }
                if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                    let text = dialog.env_headers.read(cx).value().to_string();
                    let dialog = this.mcp_dialog.as_mut().expect("checked above");
                    match dialog.kind {
                        McpTransportKind::Stdio => dialog.env_draft = text,
                        McpTransportKind::Http => dialog.headers_draft = text,
                    }
                }
            },
        ));
        self.mcp_dialog = Some(dialog);
        cx.notify();
    }

    /// 保存对话框：表单/JSON 模式各自校验后写入目标文件并刷新
    fn save_mcp_dialog(&mut self, cx: &mut Context<Self>) {
        let parsed = {
            let Some(dialog) = self.mcp_dialog.as_ref() else {
                return;
            };
            if dialog.json_mode {
                entry_from_json(dialog, cx)
            } else {
                entry_from_form(dialog, cx)
            }
        };
        let (name, entry) = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                if let Some(dialog) = self.mcp_dialog.as_mut() {
                    dialog.error = Some(error);
                }
                cx.notify();
                return;
            }
        };
        let Some(snapshot) = self.mcp_snapshot.as_ref() else {
            if let Some(dialog) = self.mcp_dialog.as_mut() {
                dialog.error = Some("配置尚未加载完成，请稍后重试".to_string());
            }
            cx.notify();
            return;
        };
        let Some(dialog) = self.mcp_dialog.as_ref() else {
            return;
        };
        let Some(path) = Self::mcp_target_path(snapshot, dialog.scope) else {
            if let Some(dialog) = self.mcp_dialog.as_mut() {
                dialog.error = Some("项目级路径不可用（会话已关闭），请改用用户级".to_string());
            }
            cx.notify();
            return;
        };
        match upsert_mcp_server(&path, &name, entry) {
            Ok(()) => {
                self.mcp_dialog = None;
                self.mcp_write_error = None;
                self.refresh_mcp(cx);
            }
            Err(e) => {
                let message = format!("写入 {} 失败：{e}", path.display());
                self.mcp_write_error = Some(message.clone());
                if let Some(dialog) = self.mcp_dialog.as_mut() {
                    dialog.error = Some(message);
                }
                cx.notify();
            }
        }
    }

    /// 删除正在编辑的 server（两步确认）；从其来源文件删除——项目级删除后用户级同名条目自然生效
    fn delete_mcp_dialog_server(&mut self, cx: &mut Context<Self>) {
        let (armed, editing, scope) = {
            let Some(dialog) = &self.mcp_dialog else {
                return;
            };
            (dialog.delete_armed, dialog.editing.clone(), dialog.scope)
        };
        if !armed {
            if let Some(dialog) = self.mcp_dialog.as_mut() {
                dialog.delete_armed = true;
                dialog.error = None;
            }
            cx.notify();
            return;
        }
        let Some(name) = editing else {
            return;
        };
        let Some(snapshot) = self.mcp_snapshot.as_ref() else {
            return;
        };
        let Some(path) = Self::mcp_target_path(snapshot, scope) else {
            if let Some(dialog) = self.mcp_dialog.as_mut() {
                dialog.delete_armed = false;
                dialog.error = Some("项目级路径不可用（会话已关闭），删除已取消".to_string());
            }
            cx.notify();
            return;
        };
        match delete_mcp_server(&path, &name) {
            Ok(()) => {
                self.mcp_dialog = None;
                self.mcp_write_error = None;
                self.refresh_mcp(cx);
            }
            Err(e) => {
                let message = format!("删除失败（{}）：{e}", path.display());
                self.mcp_write_error = Some(message.clone());
                if let Some(dialog) = self.mcp_dialog.as_mut() {
                    dialog.delete_armed = false;
                    dialog.error = Some(message);
                }
                cx.notify();
            }
        }
    }

    /// 切换传输类型：当前编辑框内容存回草稿，再换另一份草稿显示
    fn set_mcp_dialog_kind(
        &mut self,
        kind: McpTransportKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dialog) = self.mcp_dialog.as_mut() else {
            return;
        };
        if dialog.kind == kind {
            return;
        }
        let text = dialog.env_headers.read(cx).value().to_string();
        match dialog.kind {
            McpTransportKind::Stdio => dialog.env_draft = text,
            McpTransportKind::Http => dialog.headers_draft = text,
        }
        dialog.kind = kind;
        dialog.error = None;
        let draft = match kind {
            McpTransportKind::Stdio => dialog.env_draft.clone(),
            McpTransportKind::Http => dialog.headers_draft.clone(),
        };
        dialog
            .env_headers
            .update(cx, |state, cx| state.set_value(draft, window, cx));
        cx.notify();
    }

    /// 切换作用域（仅新建时可选；项目级要求已打开会话）
    fn set_mcp_dialog_scope(&mut self, scope: McpSource, cx: &mut Context<Self>) {
        let Some(dialog) = self.mcp_dialog.as_mut() else {
            return;
        };
        if dialog.editing.is_some() {
            return;
        }
        if scope == McpSource::Project && !dialog.project_available {
            return;
        }
        dialog.scope = scope;
        cx.notify();
    }

    /// 表单 ⇄ JSON 模式切换：双向都从对侧重新推导（JSON 解析失败则留在 JSON 模式）
    fn set_mcp_json_mode(&mut self, json_mode: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.mcp_dialog.as_mut() else {
            return;
        };
        if dialog.json_mode == json_mode {
            return;
        }
        dialog.error = None;
        if json_mode {
            let name = dialog.name.read(cx).value().trim().to_string();
            let name = if name.is_empty() {
                dialog
                    .editing
                    .clone()
                    .unwrap_or_else(|| "server-name".to_string())
            } else {
                name
            };
            let entry = entry_from_form(dialog, cx)
                .map(|(_, entry)| entry)
                .unwrap_or_else(|_| dialog.base.clone());
            let text = serde_json::to_string_pretty(&serde_json::json!({ name: entry }))
                .unwrap_or_default();
            dialog.json_mode = true;
            dialog
                .json_text
                .update(cx, |state, cx| state.set_value(text, window, cx));
        } else {
            match entry_from_json(dialog, cx) {
                Ok((name, entry)) => {
                    refill_mcp_form(dialog, &name, &entry, window, cx);
                    dialog.json_mode = false;
                }
                Err(error) => {
                    dialog.error = Some(error);
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn render_mcp(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = &self.mcp_snapshot else {
            return h_flex()
                .w_full()
                .items_center()
                .justify_center()
                .gap_2()
                .py_8()
                .text_color(cx.theme().muted_foreground)
                .child(Spinner::new().small())
                .child(div().text_sm().child("正在读取配置…"))
                .into_any_element();
        };

        let statuses = if self.mcp_status_applicable() {
            match (&self.mcp_session, &self.mcp_connection) {
                (Some(_), Some(list)) => list.as_deref(),
                _ => None,
            }
        } else {
            None
        };
        let query = self.mcp_search.read(cx).value().trim().to_lowercase();
        let filtered: Vec<&McpServerInfo> = snapshot
            .servers
            .iter()
            .filter(|server| {
                query.is_empty()
                    || server.name.to_lowercase().contains(&query)
                    || server
                        .transport
                        .as_ref()
                        .is_some_and(|t| t.summary().to_lowercase().contains(&query))
            })
            .collect();

        let mut page = v_flex().gap_4();
        if let Some(error) = &self.mcp_write_error {
            page = page.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        // 作用域行：选择器 +（有 server 时）连接状态摘要
        page = page.child(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(self.render_mcp_scope(cx))
                .when(!snapshot.servers.is_empty(), |this| {
                    this.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.render_mcp_status(snapshot, statuses, cx)),
                    )
                }),
        );
        if snapshot.servers.is_empty() {
            page = page.child(self.render_mcp_empty(cx));
        } else if filtered.is_empty() {
            page = page.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .py_6()
                    .child("没有匹配的 MCP 服务器"),
            );
        } else {
            page = page
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .child(format!("服务器（{}）", snapshot.servers.len())),
                )
                .children(
                    filtered
                        .iter()
                        .enumerate()
                        .map(|(ix, server)| {
                            let status = statuses
                                .and_then(|list| list.iter().find(|s| s.name == server.name));
                            self.render_mcp_server(ix, server, status, cx)
                        })
                        .collect::<Vec<_>>(),
                );
        }
        page.into_any_element()
    }

    /// 帮助弹窗：手动编辑示例 + 配置文件路径 + 生效时机
    ///（页头信息按钮弹出；配置未加载时省略路径段只显示格式说明）
    pub(crate) fn render_mcp_help_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(0.5))
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .id("mcp-help-dialog")
                    .w(px(560.))
                    .max_h(px(640.))
                    .overflow_y_scroll()
                    .gap_3()
                    .p_4()
                    .rounded(cx.theme().radius_lg)
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(div().text_lg().font_semibold().child("MCP 配置说明"))
                    .when_some(self.mcp_snapshot.as_ref(), |this, snapshot| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(div().text_sm().font_semibold().child("配置文件"))
                                .child(self.render_mcp_sources(snapshot, cx)),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_sm().font_semibold().child("手动编辑"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("参考以下格式（项目级覆盖用户级同名条目）："),
                            )
                            .child(
                                div()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .px_3()
                                    .py_2()
                                    .text_xs()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_color(cx.theme().muted_foreground)
                                    .child(MCP_EXAMPLE),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("配置对之后新建的会话生效（会话级懒连接，首个回合时连接）。"),
                    )
                    .child(
                        h_flex().gap_2().child(div().flex_1()).child(
                            Button::new("close-mcp-help")
                                .primary()
                                .small()
                                .label("关闭")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.mcp_help_open = false;
                                    cx.notify();
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// 连接状态摘要行：其他工作区/无会话/查询中/未发起懒连接/已连接计数
    fn render_mcp_status(
        &self,
        snapshot: &McpConfigSnapshot,
        statuses: Option<&[McpServerStatus]>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        // 固定查看的工作区 ≠ 会话工作区：连接状态不适用（core 连的是会话工作区的配置）
        if !self.mcp_status_applicable() {
            return h_flex()
                .gap_1()
                .items_center()
                .child(Icon::new(IconName::Info).size_3p5().text_color(muted))
                .child(
                    div().text_xs().text_color(muted).truncate().child(
                        "正在查看其他工作区的项目级配置；连接状态仅对当前会话的工作区显示。",
                    ),
                )
                .into_any_element();
        }
        match (&self.mcp_session, &self.mcp_connection) {
            (None, _) => h_flex()
                .gap_1()
                .items_center()
                .child(Icon::new(IconName::Info).size_3p5().text_color(muted))
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("未打开会话：连接状态在打开会话后显示。"),
                )
                .into_any_element(),
            (Some(_), None) => h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small())
                .child(div().text_xs().text_color(muted).child("正在查询连接状态…"))
                .into_any_element(),
            (Some(_), Some(None)) => h_flex()
                .gap_1()
                .items_center()
                .child(Icon::new(IconName::Info).size_3p5().text_color(muted))
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("当前会话尚未发起 MCP 连接（在首个回合按需建立）。"),
                )
                .into_any_element(),
            (Some(_), Some(Some(_))) => {
                let connected = statuses
                    .map(|list| {
                        list.iter()
                            .filter(|status| {
                                status.connected
                                    && snapshot
                                        .servers
                                        .iter()
                                        .any(|server| server.name == status.name)
                            })
                            .count()
                    })
                    .unwrap_or(0);
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!(
                        "当前会话已连接 {connected}/{} 个服务器（未连接 = 连接失败或已停用）。",
                        snapshot.servers.len()
                    ))
                    .into_any_element()
            }
        }
    }

    /// 无 server 时的引导：新建入口 + 示例 JSON
    fn render_mcp_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_3()
            .items_center()
            .py_6()
            .child(
                Icon::new(IconName::Inbox)
                    .size_8()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("尚未配置 MCP 服务器"),
            )
            .child(
                Button::new("new-mcp-empty")
                    .primary()
                    .icon(IconName::Plus)
                    .label("新建 MCP 服务器")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_mcp_dialog(None, window, cx);
                    })),
            )
            .into_any_element()
    }

    pub(crate) fn render_websearch(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (tavily, brave) = websearch_backends();
        let active = if tavily {
            Some("Tavily")
        } else if brave {
            Some("Brave")
        } else {
            None
        };
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(websearch_backend_row("Tavily", "TAVILY_API_KEY", "优先", tavily, cx))
                    .child(websearch_backend_row("Brave", "BRAVE_API_KEY", "备选", brave, cx)),
            )
            .child(
                div().text_xs().text_color(cx.theme().muted_foreground).child(
                    match active {
                        Some(name) => format!("当前生效后端：{name}（同时配置时优先 Tavily）。"),
                        None => "当前无生效后端：两个环境变量都未配置，WebSearch 工具不可用。"
                            .to_string(),
                    },
                ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("在启动应用的环境（shell / 启动器）中设置后重启生效："),
                    )
                    .child(
                        div()
                            .rounded_lg()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().group_box)
                            .px_3()
                            .py_2()
                            .text_xs()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(cx.theme().muted_foreground)
                            .child("export TAVILY_API_KEY=<密钥>   # tavily.com\nexport BRAVE_API_KEY=<密钥>     # brave.com/search/api"),
                    ),
            )
            .into_any_element()
    }

    /// 配置来源两行：用户级/项目级文件路径与是否存在
    fn render_mcp_sources(
        &self,
        snapshot: &McpConfigSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .gap_1()
            .child(mcp_source_row(
                "用户级",
                Some(&snapshot.user_path),
                snapshot.user_exists,
                cx,
            ))
            .child(match &snapshot.project_path {
                Some(path) => mcp_source_row("项目级", Some(path), snapshot.project_exists, cx),
                None => mcp_source_row("项目级", None, false, cx),
            })
            .into_any_element()
    }

    /// 单个 server 卡片：状态点 + 名称 + 状态/工具数 + 来源/传输 chip + 超时 + 编辑/启停，
    /// 第二行命令/URL 摘要，第三行失败原因（连接失败或条目非法）
    fn render_mcp_server(
        &self,
        ix: usize,
        server: &McpServerInfo,
        status: Option<&McpServerStatus>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chip = |label: &str, cx: &mut Context<SettingsView>| {
            div()
                .text_xs()
                .px_1()
                .rounded_sm()
                .bg(cx.theme().accent)
                .child(label.to_string())
        };
        // 状态优先级：已停用 > 条目非法 > 连接失败 > 已连接 > 未连接/未知
        let (dot, status_text) = if server.disabled {
            (cx.theme().muted_foreground, "已停用")
        } else if server.invalid_reason.is_some() {
            (cx.theme().danger, "配置无效")
        } else {
            match status {
                Some(s) if s.connected => (cx.theme().success, "已连接"),
                Some(_) => (cx.theme().danger, "连接失败"),
                None => (cx.theme().muted_foreground, "未连接"),
            }
        };
        let name = server.name.clone();
        let source = server.source;
        let edit_name = server.name.clone();
        let enabled = !server.disabled;
        let error_line = if let Some(reason) = &server.invalid_reason {
            Some(format!("配置无效：{reason}"))
        } else {
            status
                .filter(|s| !s.connected)
                .and_then(|s| s.error.as_ref())
                .and_then(|error| error.lines().next())
                .map(|line| format!("连接失败：{line}"))
        };
        let transport_chip = server.transport.as_ref().map(|t| t.chip_label());
        let summary = server
            .transport
            .as_ref()
            .map(McpTransport::summary)
            .unwrap_or_else(|| "—".to_string());

        v_flex()
            .w_full()
            .gap_1()
            .px_3()
            .py_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().size_2().rounded_full().bg(dot))
                    .child(div().text_sm().font_semibold().child(server.name.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(status_text),
                    )
                    .when_some(
                        status.filter(|s| s.connected && s.tool_count > 0),
                        |this, s| this.child(chip(&format!("{} 个工具", s.tool_count), cx)),
                    )
                    .child(chip(server.source.label(), cx))
                    .when_some(transport_chip, |this, label| this.child(chip(label, cx)))
                    .when(server.overrides_user, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("覆盖用户级同名配置"),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format_timeout(server.timeout_ms)),
                    )
                    .child(
                        Button::new(("mcp-edit", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Settings2)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_mcp_dialog(Some(&edit_name), window, cx);
                            })),
                    )
                    .child(Switch::new(("mcp-enabled", ix)).checked(enabled).on_click(
                        cx.listener(move |this, checked: &bool, _, cx| {
                            this.toggle_mcp_server(&name, source, *checked, cx);
                        }),
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().muted_foreground)
                    .child(summary),
            )
            .when_some(error_line, |this, line| {
                this.child(div().text_xs().text_color(cx.theme().danger).child(line))
            })
            .into_any_element()
    }

    /// 新建/编辑对话框（表单/JSON 双模式）
    pub(crate) fn render_mcp_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.mcp_dialog else {
            return div().into_any_element();
        };
        let editing = dialog.editing.is_some();

        // 表单模式主体（名称/作用域/类型/传输字段/超时/高级 env|headers）
        let form = v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("名称"),
                    )
                    .child(Input::new(&dialog.name).disabled(editing))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .opacity(0.7)
                            .child("工具名前缀 mcp__<名称>__<工具>；编辑时不可改名（删除后重建）"),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("作用域"),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("mcp-scope-user")
                                    .small()
                                    .when(dialog.scope == McpSource::User, |this| this.primary())
                                    .when(dialog.scope != McpSource::User, |this| this.outline())
                                    .disabled(editing)
                                    .label("用户级")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_mcp_dialog_scope(McpSource::User, cx);
                                    })),
                            )
                            .child(
                                Button::new("mcp-scope-project")
                                    .small()
                                    .when(dialog.scope == McpSource::Project, |this| this.primary())
                                    .when(dialog.scope != McpSource::Project, |this| this.outline())
                                    .disabled(editing || !dialog.project_available)
                                    .label(match &dialog.project_workspace {
                                        Some(name) => format!("项目级（{name}）"),
                                        None => "项目级".to_string(),
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_mcp_dialog_scope(McpSource::Project, cx);
                                    })),
                            ),
                    )
                    .when(!dialog.project_available, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .opacity(0.7)
                                .child(
                                    "用户级视图只写用户级；在列表上方切换到具体工作区后可写项目级",
                                ),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("类型"),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("mcp-kind-stdio")
                                    .small()
                                    .when(dialog.kind == McpTransportKind::Stdio, |this| {
                                        this.primary()
                                    })
                                    .when(dialog.kind != McpTransportKind::Stdio, |this| {
                                        this.outline()
                                    })
                                    .label(McpTransportKind::Stdio.label())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.set_mcp_dialog_kind(
                                            McpTransportKind::Stdio,
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("mcp-kind-http")
                                    .small()
                                    .when(dialog.kind == McpTransportKind::Http, |this| {
                                        this.primary()
                                    })
                                    .when(dialog.kind != McpTransportKind::Http, |this| {
                                        this.outline()
                                    })
                                    .label(McpTransportKind::Http.label())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.set_mcp_dialog_kind(
                                            McpTransportKind::Http,
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    ),
            )
            .when(dialog.kind == McpTransportKind::Stdio, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("命令"),
                        )
                        .child(Input::new(&dialog.command)),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("参数（空格分隔）"),
                        )
                        .child(Input::new(&dialog.args))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .opacity(0.7)
                                .child("含空格的参数请用 JSON 模式填写 args 数组"),
                        ),
                )
            })
            .when(dialog.kind == McpTransportKind::Http, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("URL"),
                        )
                        .child(Input::new(&dialog.url)),
                )
            })
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("超时（毫秒）"),
                    )
                    .child(Input::new(&dialog.timeout)),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .id("mcp-advanced-toggle")
                            .gap_2()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(dialog) = &mut this.mcp_dialog {
                                    dialog.advanced_open = !dialog.advanced_open;
                                }
                                cx.notify();
                            }))
                            .child(
                                Icon::new(if dialog.advanced_open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size_4()
                                .text_color(cx.theme().muted_foreground),
                            )
                            .child(div().text_sm().child(match dialog.kind {
                                McpTransportKind::Stdio => "环境变量（可选，JSON）",
                                McpTransportKind::Http => "请求头（可选，JSON）",
                            })),
                    )
                    .when(dialog.advanced_open, |this| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(Textarea::new(&dialog.env_headers))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .opacity(0.7)
                                        .child(match dialog.kind {
                                            McpTransportKind::Stdio => {
                                                "如 {\"MY_API_KEY\": \"your-key\"}"
                                            }
                                            McpTransportKind::Http => {
                                                "如 {\"Authorization\": \"Bearer your-token\"}"
                                            }
                                        }),
                                ),
                        )
                    }),
            );

        let json_view = v_flex()
            .gap_1()
            .child(Textarea::new(&dialog.json_text))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .opacity(0.7)
                    .child("支持粘贴 {\"server-name\": {…}} 或 Claude 风格 {\"mcpServers\": {…}}；一次只编辑一个 server"),
            );

        div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::black().opacity(0.5))
            .flex()
            .items_center()
            .justify_center()
            .child(
                v_flex()
                    .id("mcp-dialog")
                    .w(px(560.))
                    .max_h(px(640.))
                    .overflow_y_scroll()
                    .gap_3()
                    .p_4()
                    .rounded(cx.theme().radius_lg)
                    .bg(cx.theme().popover)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(div().text_lg().font_semibold().child(if editing {
                        format!(
                            "编辑 MCP 服务器「{}」",
                            dialog.editing.clone().expect("editing")
                        )
                    } else {
                        "新建 MCP 服务器".to_string()
                    }))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("mcp-mode-form")
                                    .small()
                                    .when(!dialog.json_mode, |this| this.primary())
                                    .when(dialog.json_mode, |this| this.outline())
                                    .label("表单")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.set_mcp_json_mode(false, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("mcp-mode-json")
                                    .small()
                                    .when(dialog.json_mode, |this| this.primary())
                                    .when(!dialog.json_mode, |this| this.outline())
                                    .label("JSON")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.set_mcp_json_mode(true, window, cx);
                                    })),
                            ),
                    )
                    .child(if dialog.json_mode { json_view } else { form })
                    .when_some(dialog.error.clone(), |this, error| {
                        this.child(div().text_xs().text_color(cx.theme().danger).child(error))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .mt_1()
                            .when(editing, |this| {
                                this.child(
                                    Button::new("mcp-dialog-delete")
                                        .ghost()
                                        .small()
                                        .label(if dialog.delete_armed {
                                            "确认删除？"
                                        } else {
                                            "删除"
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.delete_mcp_dialog_server(cx);
                                        })),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                Button::new("mcp-dialog-cancel")
                                    .outline()
                                    .small()
                                    .label("取消")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.mcp_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("mcp-dialog-save")
                                    .primary()
                                    .small()
                                    .label("保存")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_mcp_dialog(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

/// 配置来源行：级别标签 + 文件路径 + 「未创建」标注
fn mcp_source_row(
    label: &str,
    path: Option<&Path>,
    exists: bool,
    cx: &mut Context<SettingsView>,
) -> Div {
    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .w(px(48.))
                .flex_shrink_0()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label.to_string()),
        )
        .child(
            div()
                .flex_1()
                .text_xs()
                .font_family(cx.theme().mono_font_family.clone())
                .child(match path {
                    Some(path) => path.display().to_string(),
                    None => "选择工作区后显示其项目级配置".to_string(),
                }),
        )
        .when(path.is_some() && !exists, |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("（未创建）"),
            )
        })
}

/// WebSearch 后端行：名称 + 角色 chip + 环境变量名 + 配置状态（不展示密钥内容）
fn websearch_backend_row(
    name: &str,
    env_var: &str,
    role: &str,
    configured: bool,
    cx: &mut Context<SettingsView>,
) -> Div {
    h_flex()
        .w_full()
        .gap_2()
        .px_3()
        .py_2()
        .items_center()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            v_flex()
                .flex_1()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_sm().font_semibold().child(name.to_string()))
                        .child(
                            div()
                                .text_xs()
                                .px_1()
                                .rounded_sm()
                                .bg(cx.theme().accent)
                                .child(role.to_string()),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_color(cx.theme().muted_foreground)
                        .child(env_var.to_string()),
                ),
        )
        .child(div().size_2().rounded_full().bg(if configured {
            cx.theme().success
        } else {
            cx.theme().muted_foreground
        }))
        .child(
            div()
                .text_xs()
                .text_color(if configured {
                    cx.theme().success
                } else {
                    cx.theme().muted_foreground
                })
                .child(if configured { "已配置" } else { "未配置" }),
        )
}

#[cfg(test)]
mod tests;
