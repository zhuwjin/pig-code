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

/// server 传输形态：stdio（command/args）或远程（url；core v1 仅支持 stdio 连接）
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum McpTransport {
    Stdio { command: String, args: Vec<String> },
    Remote { url: String },
}

impl McpTransport {
    fn chip_label(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Remote { .. } => "远程",
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

/// 一个 mcp.json server 条目的展示投影
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct McpServerInfo {
    pub name: String,
    pub source: McpSource,
    pub transport: McpTransport,
    pub timeout_ms: Option<u64>,
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

/// 解析单个 mcp.json：文件级/条目级非法都跳过（不 panic）；stdio 与远程形态兼容
fn parse_servers(raw: &str, source: McpSource) -> Vec<McpServerInfo> {
    let Ok(file) = serde_json::from_str::<serde_json::Value>(raw) else {
        return vec![];
    };
    let Some(servers) = file.get("mcpServers").and_then(|v| v.as_object()) else {
        return vec![];
    };
    servers
        .iter()
        .filter_map(|(name, value)| {
            let non_empty = |key: &str| {
                value
                    .get(key)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
            };
            let transport = match (non_empty("command"), non_empty("url")) {
                (Some(command), _) => McpTransport::Stdio {
                    command: command.to_string(),
                    args: value
                        .get("args")
                        .and_then(|v| v.as_array())
                        .map(|args| {
                            args.iter()
                                .filter_map(|arg| arg.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                },
                (None, Some(url)) => McpTransport::Remote {
                    url: url.to_string(),
                },
                (None, None) => return None,
            };
            Some(McpServerInfo {
                name: name.clone(),
                source,
                transport,
                timeout_ms: value
                    .get("timeoutMs")
                    .and_then(|v| v.as_u64())
                    .filter(|&ms| ms > 0),
                overrides_user: false,
            })
        })
        .collect()
}

/// 用户级为底、项目级覆盖同名（与 core 合并口径一致）；输出按名字排序
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

fn format_timeout(timeout_ms: Option<u64>) -> String {
    match timeout_ms {
        None => format!("超时 {}s（默认）", DEFAULT_TIMEOUT_MS / 1000),
        Some(ms) if ms % 1000 == 0 => format!("超时 {}s", ms / 1000),
        Some(ms) => format!("超时 {ms}ms"),
    }
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
    /// 刷新 MCP 页：作废连接查询结果，AppView 收到事件后重读配置并重新查询 core
    pub(crate) fn refresh_mcp(&mut self, cx: &mut Context<Self>) {
        self.mcp_connection = None;
        cx.emit(SettingsEvent::RefreshMcp);
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

        let connected = match (&self.mcp_session, &self.mcp_connection) {
            (Some(_), Some(names)) => names.as_ref(),
            _ => None,
        };

        let mut page = v_flex()
            .gap_4()
            .child(self.render_mcp_sources(snapshot, cx));

        if snapshot.servers.is_empty() {
            page = page.child(self.render_mcp_empty(cx));
        } else {
            page = page
                .child(self.render_mcp_status(snapshot, cx))
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .child(format!("服务器（{}）", snapshot.servers.len())),
                )
                .children(
                    snapshot
                        .servers
                        .iter()
                        .map(|server| {
                            let status =
                                connected.map(|names| names.iter().any(|n| n == &server.name));
                            render_mcp_server(server, status, cx)
                        })
                        .collect::<Vec<_>>(),
                );
        }
        page.into_any_element()
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

    /// 连接状态摘要行：无会话/查询中/未发起懒连接/已连接计数
    fn render_mcp_status(
        &self,
        snapshot: &McpConfigSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
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
            (Some(_), Some(Some(names))) => {
                let connected = snapshot
                    .servers
                    .iter()
                    .filter(|server| names.contains(&server.name))
                    .count();
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!(
                        "当前会话已连接 {connected}/{} 个服务器（未连接 = 连接失败）。",
                        snapshot.servers.len()
                    ))
                    .into_any_element()
            }
        }
    }

    /// 无 server 时的引导：示例 JSON + 生效说明
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
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("在以上任一文件中添加 mcpServers 配置（项目级覆盖用户级同名条目）："),
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
                    .child(MCP_EXAMPLE),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("保存后点击右上角「刷新」更新列表；配置对之后新建的会话生效（会话级懒连接）。"),
            )
            .into_any_element()
    }

    pub(crate) fn render_websearch(&self, cx: &mut Context<Self>) -> AnyElement {
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
                    None => "未打开会话，暂不显示项目级配置".to_string(),
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

/// 单个 server 行：状态点 + 名称 + 来源/传输 chip + 超时 + 命令/URL 摘要
fn render_mcp_server(
    server: &McpServerInfo,
    connected: Option<bool>,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let (dot, status) = match connected {
        Some(true) => (cx.theme().success, "已连接"),
        Some(false) => (cx.theme().muted_foreground, "未连接"),
        None => (cx.theme().muted_foreground, "未知"),
    };
    let chip = |label: &str, cx: &mut Context<SettingsView>| {
        div()
            .text_xs()
            .px_1()
            .rounded_sm()
            .bg(cx.theme().accent)
            .child(label.to_string())
    };
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
                        .child(status),
                )
                .child(chip(server.source.label(), cx))
                .child(chip(server.transport.chip_label(), cx))
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
                ),
        )
        .child(
            div()
                .text_xs()
                .font_family(cx.theme().mono_font_family.clone())
                .text_color(cx.theme().muted_foreground)
                .child(server.transport.summary()),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests;
