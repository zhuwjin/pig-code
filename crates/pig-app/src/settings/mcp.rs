use super::*;
use std::path::{Path, PathBuf};

/// Aligned with core's mcp::config::DEFAULT_TIMEOUT (display value when the
/// config omits it)
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Onboarding example for an empty config (the stdio shape core supports)
const MCP_EXAMPLE: &str = r#"{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/dir"]
    }
  }
}"#;

/// MCP page scope (aligned with ZCode's PluginScopeMenu): user-level or one of
/// the workspace list
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum McpScope {
    /// Read/write only the user-level `<data_dir>/mcp.json` (applies to all
    /// workspaces)
    User,
    /// Workspace: view the merge of user level plus that workspace's
    /// `.pigcode/mcp.json` (same-name overrides)
    Workspace(PathBuf),
}

/// Workspace display name: alias first, else the directory name, finally
/// falling back to the full path
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
    fn label(self) -> String {
        match self {
            Self::User => rust_i18n::t!("settings.common.scope_user").to_string(),
            Self::Project => rust_i18n::t!("settings.common.scope_project").to_string(),
        }
    }
}

/// Transport kinds selectable in the dialog (core does not support legacy SSE,
/// only these two)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpTransportKind {
    Stdio,
    Http,
}

impl McpTransportKind {
    fn label(self) -> String {
        match self {
            Self::Stdio => rust_i18n::t!("settings.mcp.transport_stdio").to_string(),
            Self::Http => rust_i18n::t!("settings.mcp.transport_http").to_string(),
        }
    }
}

/// Server transport shape: stdio (command/args) or remote (url) — a projection
/// for list display
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

/// Display projection of one mcp.json server entry: raw keeps the full JSON
/// (fidelity for edit write-back), the parse conclusion goes into transport /
/// invalid_reason (same criteria as core's config::parse_server)
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct McpServerInfo {
    pub name: String,
    pub source: McpSource,
    /// Raw entry JSON (including env/headers/type and unknown fields like oauth)
    pub raw: serde_json::Value,
    pub transport: Option<McpTransport>,
    /// Disabled by `disabled: true`: core does not connect to this entry
    pub disabled: bool,
    pub timeout_ms: Option<u64>,
    /// Why the entry failed to parse (core skips it when connecting)
    pub invalid_reason: Option<String>,
    /// This project-level entry overrides a user-level entry with the same name
    pub overrides_user: bool,
}

/// The two config source files plus the merged server list (the settings
/// page's MCP data snapshot)
pub(crate) struct McpConfigSnapshot {
    pub user_path: PathBuf,
    pub user_exists: bool,
    /// None = no active session (workspace undetermined, project level does not
    /// participate)
    pub project_path: Option<PathBuf>,
    pub project_exists: bool,
    pub servers: Vec<McpServerInfo>,
}

/// Read the user-level `<data_dir>/mcp.json` plus the project-level
/// `<workspace>/.pigcode/mcp.json` and merge
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

/// Parse a single mcp.json: file-level invalid returns empty; entry-level
/// invalid keeps the entry with its reason annotated (for edit-and-fix).
/// Validation criteria match core's config::parse_server (explicit type first,
/// otherwise inferred from url/command)
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
            Some(rust_i18n::t!("settings.mcp.err_sse").to_string()),
        ),
        Some(other) => (
            None,
            Some(rust_i18n::t!("settings.mcp.err_unknown_type", other = other).to_string()),
        ),
        None => match (non_empty("command"), non_empty("url")) {
            (Some(_), _) => stdio_projection(value),
            (None, Some(_)) => remote_projection(value),
            (None, None) => (
                None,
                Some(rust_i18n::t!("settings.mcp.err_missing_command_or_url").to_string()),
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
        return (
            None,
            Some(rust_i18n::t!("settings.mcp.err_stdio_missing_command").to_string()),
        );
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
        return (
            None,
            Some(rust_i18n::t!("settings.mcp.err_remote_missing_url").to_string()),
        );
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return (
            None,
            Some(rust_i18n::t!("settings.mcp.err_url_prefix", url = url).to_string()),
        );
    }
    (
        Some(McpTransport::Remote {
            url: url.to_string(),
        }),
        None,
    )
}

/// User level as the base, project level overrides same names (same merge
/// criteria as core); output sorted by name. Invalid/disabled entries are kept
/// too (the page annotates them and offers edit-and-fix)
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

// ---------- mcp.json writes (create/edit/delete/enable-disable all persist,
// then the whole page refreshes) ----------

/// Read the mcpServers object of mcp.json (missing file/no mcpServers → empty
/// object)
fn read_servers_object(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|file| file.get("mcpServers").and_then(|v| v.as_object()).cloned())
        .unwrap_or_default()
}

/// Write mcp.json back: keep file-level fields other than mcpServers; refuse to
/// rewrite when the existing content is invalid (no silent overwrite); create
/// missing parent directories
fn write_servers_object(
    path: &Path,
    servers: serde_json::Map<String, serde_json::Value>,
) -> std::io::Result<()> {
    let mut file = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw).map_err(|e| {
            std::io::Error::other(
                rust_i18n::t!("settings.mcp.err_existing_invalid_json", error = e).to_string(),
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e),
    };
    if !file.is_object() {
        return Err(std::io::Error::other(
            rust_i18n::t!("settings.mcp.err_existing_not_object").to_string(),
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

/// Create/overwrite one server entry
fn upsert_mcp_server(path: &Path, name: &str, entry: serde_json::Value) -> std::io::Result<()> {
    let mut servers = read_servers_object(path);
    servers.insert(name.to_string(), entry);
    write_servers_object(path, servers)
}

/// Delete one server entry (missing file/missing entry counts as success)
fn delete_mcp_server(path: &Path, name: &str) -> std::io::Result<()> {
    let mut servers = read_servers_object(path);
    servers.remove(name);
    write_servers_object(path, servers)
}

/// Enable/disable one server entry (disabled: true / remove the key)
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
        None => rust_i18n::t!(
            "settings.mcp.timeout_default",
            secs = DEFAULT_TIMEOUT_MS / 1000
        )
        .to_string(),
        Some(ms) if ms % 1000 == 0 => {
            rust_i18n::t!("settings.mcp.timeout_secs", secs = ms / 1000).to_string()
        }
        Some(ms) => rust_i18n::t!("settings.mcp.timeout_ms", ms = ms).to_string(),
    }
}

/// Display/backfill format of a JSON object draft: empty object or absent → "{}"
fn pretty_object(value: Option<&serde_json::Value>) -> String {
    match value.filter(|v| v.as_object().is_some_and(|m| !m.is_empty())) {
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string()),
        None => "{}".to_string(),
    }
}

/// Environment variable/header draft text → JSON object (blank text/empty
/// object → empty map)
fn parse_object_json(
    raw: &str,
    label: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Default::default());
    }
    let parsed: serde_json::Value = serde_json::from_str(raw).map_err(|e| {
        rust_i18n::t!("settings.mcp.err_label_not_json", label = label, error = e).to_string()
    })?;
    match parsed {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(rust_i18n::t!("settings.mcp.err_label_must_be_object", label = label).to_string()),
    }
}

/// Form → (name, entry JSON). Overlay form fields on the edit base: unknown
/// fields like oauth are written back faithfully
fn entry_from_form(dialog: &McpDialog, cx: &App) -> Result<(String, serde_json::Value), String> {
    let name = dialog.name.read(cx).value().trim().to_string();
    if name.is_empty() {
        return Err(rust_i18n::t!("settings.mcp.err_name_empty").to_string());
    }
    let mut entry = if dialog.base.is_object() {
        dialog.base.clone()
    } else {
        serde_json::json!({})
    };
    let map = entry
        .as_object_mut()
        .expect("base should be an object or empty object");
    // Either command or url suffices for inference; drop a leftover type to
    // avoid conflicting with the form choice
    map.remove("type");
    match dialog.kind {
        McpTransportKind::Stdio => {
            let command = dialog.command.read(cx).value().trim().to_string();
            if command.is_empty() {
                return Err(rust_i18n::t!("settings.mcp.err_stdio_command").to_string());
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
            let env = parse_object_json(
                &dialog.env_draft,
                rust_i18n::t!("settings.mcp.env_label").as_ref(),
            )?;
            if env.is_empty() {
                map.remove("env");
            } else {
                map.insert("env".to_string(), serde_json::Value::Object(env));
            }
        }
        McpTransportKind::Http => {
            let url = dialog.url.read(cx).value().trim().to_string();
            if url.is_empty() {
                return Err(rust_i18n::t!("settings.mcp.err_http_url").to_string());
            }
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(rust_i18n::t!("settings.mcp.err_url_scheme").to_string());
            }
            map.insert("url".to_string(), serde_json::json!(url));
            map.remove("command");
            map.remove("args");
            map.remove("env");
            let headers = parse_object_json(
                &dialog.headers_draft,
                rust_i18n::t!("settings.mcp.headers_label").as_ref(),
            )?;
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
            .map_err(|_| rust_i18n::t!("settings.mcp.err_timeout").to_string())?;
        if ms == 0 {
            return Err(rust_i18n::t!("settings.mcp.err_timeout").to_string());
        }
        map.insert("timeoutMs".to_string(), serde_json::json!(ms));
    }
    Ok((name, entry))
}

/// JSON mode → (name, entry JSON). Accepts both `{"server-name": {...}}` and
/// Claude-style `{"mcpServers": {...}}`; only one server is edited at a time
fn entry_from_json(dialog: &McpDialog, cx: &App) -> Result<(String, serde_json::Value), String> {
    let raw = dialog.json_text.read(cx).value().trim().to_string();
    if raw.is_empty() {
        return Err(rust_i18n::t!("settings.mcp.err_json_empty").to_string());
    }
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| rust_i18n::t!("settings.mcp.err_invalid_json", error = e).to_string())?;
    let mut obj = parsed
        .as_object()
        .cloned()
        .ok_or_else(|| rust_i18n::t!("settings.mcp.err_top_not_object").to_string())?;
    if let Some(inner) = obj.get("mcpServers") {
        obj = inner
            .as_object()
            .cloned()
            .ok_or_else(|| rust_i18n::t!("settings.mcp.err_mcpservers_not_object").to_string())?;
    }
    if obj.len() != 1 {
        return Err(rust_i18n::t!("settings.mcp.err_json_single_server").to_string());
    }
    let (name, entry) = obj.into_iter().next().expect("len == 1");
    if name.trim().is_empty() {
        return Err(rust_i18n::t!("settings.mcp.err_json_name_empty").to_string());
    }
    if !entry.is_object() {
        return Err(rust_i18n::t!("settings.mcp.err_server_config_object").to_string());
    }
    if let Some(editing) = &dialog.editing
        && name != *editing
    {
        return Err(rust_i18n::t!("settings.mcp.err_rename", editing = editing).to_string());
    }
    let has = |key: &str| {
        entry
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
    };
    if !has("command") && !has("url") {
        return Err(rust_i18n::t!("settings.mcp.err_config_missing_command_url").to_string());
    }
    Ok((name, entry))
}

/// Backfill the form after JSON parses successfully (for JSON → form switching)
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

/// Whether an environment variable is "configured" (same criteria as core's
/// tool/websearch.rs: present and non-blank)
fn api_key_configured(raw: Option<String>) -> bool {
    raw.is_some_and(|value| !value.trim().is_empty())
}

/// (tavily, brave) configuration status; Tavily wins when both are configured
fn websearch_backends() -> (bool, bool) {
    (
        api_key_configured(std::env::var("TAVILY_API_KEY").ok()),
        api_key_configured(std::env::var("BRAVE_API_KEY").ok()),
    )
}

impl SettingsView {
    /// Refresh the MCP page: invalidate the status query result; on the event
    /// AppView re-reads config and queries core again
    pub(crate) fn refresh_mcp(&mut self, cx: &mut Context<Self>) {
        self.mcp_connection = None;
        cx.emit(SettingsEvent::RefreshMcp);
        cx.notify();
    }

    /// Read by AppView when refreshing the MCP snapshot: the currently selected
    /// scope
    pub(crate) fn mcp_scope(&self) -> &McpScope {
        &self.mcp_scope
    }

    /// Workspace list feed (same source as the sidebar: visible workspaces ∪
    /// session cwd, with display names)
    pub(crate) fn set_scope_workspaces(
        &mut self,
        workspaces: Vec<(PathBuf, String)>,
        cx: &mut Context<Self>,
    ) {
        self.scope_workspaces = workspaces;
        // The archived page workspace filter dropdown's options rebuild from
        // this list (synced via the dirty flag before render)
        self.archived_ws_dirty = true;
        cx.notify();
    }

    /// Whether connection status applies to the config being viewed: not
    /// applicable when the pinned workspace ≠ session workspace
    fn mcp_status_applicable(&self) -> bool {
        match &self.mcp_scope {
            // User-level entries participate in the session connection (the
            // session connects the merge of user level plus the session
            // workspace)
            McpScope::User => true,
            McpScope::Workspace(path) => self.session_cwd.as_deref() == Some(path.as_path()),
        }
    }

    /// Switch scope: refresh the whole page after changing the target (AppView
    /// re-reads the snapshot for the new scope)
    fn set_mcp_scope(&mut self, scope: McpScope, cx: &mut Context<Self>) {
        self.mcp_scope_popup = false;
        if self.mcp_scope == scope {
            cx.notify();
            return;
        }
        self.mcp_scope = scope;
        self.refresh_mcp(cx);
    }

    /// Scope selector (pill button; the dropdown goes through a deferred popup
    /// so list scrolling doesn't clip it)
    pub(crate) fn render_mcp_scope(&self, cx: &mut Context<Self>) -> AnyElement {
        let (icon, label) = match &self.mcp_scope {
            McpScope::User => (
                IconName::User,
                rust_i18n::t!("settings.common.scope_user").to_string(),
            ),
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
                        // Clicking the button while the popup is open: the
                        // press first triggers the popup's outside-close
                        // (recording the press position), and the immediately
                        // following click is swallowed by matching the same
                        // position, avoiding collapse-then-reopen (same
                        // approach as the API format popup)
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

    /// Scope dropdown: user level plus the workspace list (the session's
    /// workspace gets a "current session" badge)
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
                // User level: selection uses no highlight fill, a check at
                // the row end (like ZCode); hover highlights with a rounded
                // border
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
                            .child(
                                div()
                                    .text_sm()
                                    .child(rust_i18n::t!("settings.common.scope_user").to_string()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(
                                        rust_i18n::t!("settings.mcp.scope_user_hint").to_string(),
                                    ),
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
                    .child(rust_i18n::t!("settings.common.workspace_section").to_string()),
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
                                                    .child(
                                                        rust_i18n::t!(
                                                            "settings.common.current_session"
                                                        )
                                                        .to_string(),
                                                    ),
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

    /// The dialog's target file path (by scope). Project-level entries only
    /// exist when the project path is known (a session is open); if the
    /// snapshot has since refreshed to a no-session state, return None and the
    /// caller gives up writing
    fn mcp_target_path(snapshot: &McpConfigSnapshot, scope: McpSource) -> Option<PathBuf> {
        match scope {
            McpSource::User => Some(snapshot.user_path.clone()),
            McpSource::Project => snapshot.project_path.clone(),
        }
    }

    /// Enable/disable toggle: write disabled into the entry's source file, then
    /// refresh the whole page
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
                self.mcp_write_error = Some(
                    rust_i18n::t!(
                        "settings.mcp.write_failed",
                        path = path.display(),
                        error = e
                    )
                    .to_string(),
                );
                cx.notify();
            }
        }
    }

    /// Open the create (name=None)/edit dialog, prefilled from the existing
    /// entry
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
        // Workspace name for a project-level target (two levels up from the
        // snapshot's project path = workspace root; display name via the alias
        // table)
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
                    .placeholder(rust_i18n::t!("settings.mcp.name_placeholder"))
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
                    .placeholder(rust_i18n::t!("settings.mcp.timeout_placeholder"))
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
        // Environment variable/header drafts follow the textarea's content
        // (swapped for display when switching transport kind)
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

    /// Save the dialog: form/JSON mode each validate, then write the target
    /// file and refresh
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
                dialog.error = Some(rust_i18n::t!("settings.mcp.err_not_loaded").to_string());
            }
            cx.notify();
            return;
        };
        let Some(dialog) = self.mcp_dialog.as_ref() else {
            return;
        };
        let Some(path) = Self::mcp_target_path(snapshot, dialog.scope) else {
            if let Some(dialog) = self.mcp_dialog.as_mut() {
                dialog.error =
                    Some(rust_i18n::t!("settings.mcp.err_project_unavailable").to_string());
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
                let message = rust_i18n::t!(
                    "settings.mcp.write_failed",
                    path = path.display(),
                    error = e
                )
                .to_string();
                self.mcp_write_error = Some(message.clone());
                if let Some(dialog) = self.mcp_dialog.as_mut() {
                    dialog.error = Some(message);
                }
                cx.notify();
            }
        }
    }

    /// Delete the server being edited (two-step confirm); removed from its
    /// source file — after a project-level deletion the user-level same-name
    /// entry takes effect again
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
                dialog.error =
                    Some(rust_i18n::t!("settings.mcp.err_project_delete_unavailable").to_string());
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
                let message = rust_i18n::t!(
                    "settings.mcp.delete_failed",
                    path = path.display(),
                    error = e
                )
                .to_string();
                self.mcp_write_error = Some(message.clone());
                if let Some(dialog) = self.mcp_dialog.as_mut() {
                    dialog.delete_armed = false;
                    dialog.error = Some(message);
                }
                cx.notify();
            }
        }
    }

    /// Switch transport kind: save the current textarea content back into its
    /// draft, then display the other draft
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

    /// Switch scope (only selectable when creating; project level requires an
    /// open session)
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

    /// Form ⇄ JSON mode switch: both directions re-derive from the other side
    /// (a JSON parse failure stays in JSON mode)
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
                .child(
                    div()
                        .text_sm()
                        .child(rust_i18n::t!("settings.mcp.loading").to_string()),
                )
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
        // Scope row: the selector plus (when servers exist) the connection
        // status summary
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
                    .child(rust_i18n::t!("settings.mcp.no_match").to_string()),
            );
        } else {
            page = page
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .child(if snapshot.servers.len() == 1 {
                            rust_i18n::t!("settings.mcp.server_count_one", n = 1).to_string()
                        } else {
                            rust_i18n::t!("settings.mcp.server_count", n = snapshot.servers.len())
                                .to_string()
                        }),
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

    /// Help dialog: manual edit example plus config file paths plus when
    /// changes take effect (popped up by the header info button; when config is
    /// not loaded the paths section is omitted and only the format notes are
    /// shown)
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
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child(rust_i18n::t!("settings.mcp.help_title").to_string()),
                    )
                    .when_some(self.mcp_snapshot.as_ref(), |this, snapshot| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    div().text_sm().font_semibold().child(
                                        rust_i18n::t!("settings.mcp.config_files").to_string(),
                                    ),
                                )
                                .child(self.render_mcp_sources(snapshot, cx)),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(rust_i18n::t!("settings.mcp.manual_edit").to_string()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        rust_i18n::t!("settings.mcp.manual_edit_hint").to_string(),
                                    ),
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
                            .child(rust_i18n::t!("settings.mcp.effective_hint").to_string()),
                    )
                    .child(
                        h_flex().gap_2().child(div().flex_1()).child(
                            Button::new("close-mcp-help")
                                .primary()
                                .small()
                                .label(rust_i18n::t!("settings.common.close"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.mcp_help_open = false;
                                    cx.notify();
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// Connection status summary row: other workspace/no session/querying/lazy
    /// connection not started/connected count
    fn render_mcp_status(
        &self,
        snapshot: &McpConfigSnapshot,
        statuses: Option<&[McpServerStatus]>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        // Pinned viewing workspace ≠ session workspace: connection status does
        // not apply (core connects the session workspace's config)
        if !self.mcp_status_applicable() {
            return h_flex()
                .gap_1()
                .items_center()
                .child(Icon::new(IconName::Info).size_3p5().text_color(muted))
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .truncate()
                        .child(rust_i18n::t!("settings.mcp.status_other_workspace").to_string()),
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
                        .child(rust_i18n::t!("settings.mcp.status_no_session").to_string()),
                )
                .into_any_element(),
            (Some(_), None) => h_flex()
                .gap_2()
                .items_center()
                .child(Spinner::new().small())
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(rust_i18n::t!("settings.mcp.status_querying").to_string()),
                )
                .into_any_element(),
            (Some(_), Some(None)) => h_flex()
                .gap_1()
                .items_center()
                .child(Icon::new(IconName::Info).size_3p5().text_color(muted))
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(rust_i18n::t!("settings.mcp.status_lazy").to_string()),
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
                    .child(
                        rust_i18n::t!(
                            "settings.mcp.status_connected",
                            connected = connected,
                            total = snapshot.servers.len()
                        )
                        .to_string(),
                    )
                    .into_any_element()
            }
        }
    }

    /// Onboarding when no servers exist: create entry point plus example JSON
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
                    .child(rust_i18n::t!("settings.mcp.empty_title").to_string()),
            )
            .child(
                Button::new("new-mcp-empty")
                    .primary()
                    .icon(IconName::Plus)
                    .label(rust_i18n::t!("settings.mcp.new_mcp_server"))
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
                    .child(websearch_backend_row(
                        "Tavily",
                        "TAVILY_API_KEY",
                        rust_i18n::t!("settings.websearch.role_primary").as_ref(),
                        tavily,
                        cx,
                    ))
                    .child(websearch_backend_row(
                        "Brave",
                        "BRAVE_API_KEY",
                        rust_i18n::t!("settings.websearch.role_fallback").as_ref(),
                        brave,
                        cx,
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(match active {
                        Some(name) => {
                            rust_i18n::t!("settings.websearch.active_backend", name = name)
                                .to_string()
                        }
                        None => rust_i18n::t!("settings.websearch.no_backend").to_string(),
                    }),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.websearch.env_hint").to_string()),
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
                            .child(rust_i18n::t!("settings.websearch.env_example").to_string()),
                    ),
            )
            .into_any_element()
    }

    /// Two config source rows: user-level/project-level file paths and whether
    /// they exist
    fn render_mcp_sources(
        &self,
        snapshot: &McpConfigSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        v_flex()
            .gap_1()
            .child(mcp_source_row(
                rust_i18n::t!("settings.common.scope_user").as_ref(),
                Some(&snapshot.user_path),
                snapshot.user_exists,
                cx,
            ))
            .child(match &snapshot.project_path {
                Some(path) => mcp_source_row(
                    rust_i18n::t!("settings.common.scope_project").as_ref(),
                    Some(path),
                    snapshot.project_exists,
                    cx,
                ),
                None => mcp_source_row(
                    rust_i18n::t!("settings.common.scope_project").as_ref(),
                    None,
                    false,
                    cx,
                ),
            })
            .into_any_element()
    }

    /// One server card: status dot plus name plus status/tool count plus
    /// source/transport chips plus timeout plus edit/enable-disable; second row
    /// the command/URL summary, third row the failure reason (connection
    /// failure or invalid entry)
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
        // Status priority: disabled > invalid entry > connection failed >
        // connected > disconnected/unknown
        let (dot, status_text) = if server.disabled {
            (
                cx.theme().muted_foreground,
                rust_i18n::t!("settings.mcp.state_disabled").to_string(),
            )
        } else if server.invalid_reason.is_some() {
            (
                cx.theme().danger,
                rust_i18n::t!("settings.mcp.state_invalid").to_string(),
            )
        } else {
            match status {
                Some(s) if s.connected => (
                    cx.theme().success,
                    rust_i18n::t!("settings.mcp.state_connected").to_string(),
                ),
                Some(_) => (
                    cx.theme().danger,
                    rust_i18n::t!("settings.mcp.state_failed").to_string(),
                ),
                None => (
                    cx.theme().muted_foreground,
                    rust_i18n::t!("settings.mcp.state_disconnected").to_string(),
                ),
            }
        };
        let name = server.name.clone();
        let source = server.source;
        let edit_name = server.name.clone();
        let enabled = !server.disabled;
        let error_line = if let Some(reason) = &server.invalid_reason {
            Some(rust_i18n::t!("settings.mcp.invalid_prefix", reason = reason).to_string())
        } else {
            // Structured CoreError → localized single line (the mapped result
            // is already single-line, no need to trim the first line)
            status
                .filter(|s| !s.connected)
                .and_then(|s| s.error.as_ref())
                .map(|error| {
                    rust_i18n::t!(
                        "settings.mcp.failed_prefix",
                        error = crate::errors::core_error_text(error)
                    )
                    .to_string()
                })
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
                        |this, s| {
                            this.child(chip(
                                &(if s.tool_count == 1 {
                                    rust_i18n::t!("settings.mcp.tool_count_one", n = 1)
                                } else {
                                    rust_i18n::t!("settings.mcp.tool_count", n = s.tool_count)
                                }),
                                cx,
                            ))
                        },
                    )
                    .child(chip(&server.source.label(), cx))
                    .when_some(transport_chip, |this, label| this.child(chip(label, cx)))
                    .when(server.overrides_user, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(rust_i18n::t!("settings.mcp.overrides_user").to_string()),
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

    /// Create/edit dialog (dual form/JSON modes)
    pub(crate) fn render_mcp_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.mcp_dialog else {
            return div().into_any_element();
        };
        let editing = dialog.editing.is_some();

        // Form mode body (name/scope/type/transport fields/timeout/advanced
        // env|headers)
        let form = v_flex()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.common.field_name").to_string()),
                    )
                    .child(Input::new(&dialog.name).disabled(editing))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .opacity(0.7)
                            .child(rust_i18n::t!("settings.mcp.name_hint").to_string()),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rust_i18n::t!("settings.common.scope").to_string()),
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
                                    .label(rust_i18n::t!("settings.common.scope_user"))
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
                                        Some(name) => rust_i18n::t!(
                                            "settings.common.scope_project_named",
                                            name = name
                                        )
                                        .to_string(),
                                        None => rust_i18n::t!("settings.common.scope_project")
                                            .to_string(),
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
                                    rust_i18n::t!("settings.common.project_unavailable_hint")
                                        .to_string(),
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
                            .child(rust_i18n::t!("settings.mcp.field_type").to_string()),
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
                                .child(rust_i18n::t!("settings.mcp.field_command").to_string()),
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
                                .child(rust_i18n::t!("settings.mcp.field_args").to_string()),
                        )
                        .child(Input::new(&dialog.args))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .opacity(0.7)
                                .child(rust_i18n::t!("settings.mcp.args_hint").to_string()),
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
                            .child(rust_i18n::t!("settings.mcp.field_timeout").to_string()),
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
                                McpTransportKind::Stdio => {
                                    rust_i18n::t!("settings.mcp.env_optional").to_string()
                                }
                                McpTransportKind::Http => {
                                    rust_i18n::t!("settings.mcp.headers_optional").to_string()
                                }
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
                                                rust_i18n::t!("settings.mcp.env_example")
                                                    .to_string()
                                            }
                                            McpTransportKind::Http => {
                                                rust_i18n::t!("settings.mcp.headers_example")
                                                    .to_string()
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
                    .child(rust_i18n::t!("settings.mcp.json_hint").to_string()),
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
                        rust_i18n::t!(
                            "settings.mcp.edit_title",
                            name = dialog.editing.clone().expect("editing")
                        )
                        .to_string()
                    } else {
                        rust_i18n::t!("settings.mcp.new_mcp_server").to_string()
                    }))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("mcp-mode-form")
                                    .small()
                                    .when(!dialog.json_mode, |this| this.primary())
                                    .when(dialog.json_mode, |this| this.outline())
                                    .label(rust_i18n::t!("settings.mcp.form_mode"))
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
                                            rust_i18n::t!("settings.common.confirm_delete")
                                        } else {
                                            rust_i18n::t!("common.delete")
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
                                    .label(rust_i18n::t!("common.cancel"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.mcp_dialog = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("mcp-dialog-save")
                                    .primary()
                                    .small()
                                    .label(rust_i18n::t!("settings.common.save"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_mcp_dialog(cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

/// One config source row: level label plus file path plus a "not created" note
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
                    None => rust_i18n::t!("settings.mcp.source_pick_workspace").to_string(),
                }),
        )
        .when(path.is_some() && !exists, |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(rust_i18n::t!("settings.common.not_created").to_string()),
            )
        })
}

/// WebSearch backend row: name plus role chip plus environment variable name
/// plus configuration status (never shows key contents)
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
                .child(if configured {
                    rust_i18n::t!("settings.websearch.configured")
                } else {
                    rust_i18n::t!("settings.websearch.not_configured")
                }),
        )
}

#[cfg(test)]
mod tests;
