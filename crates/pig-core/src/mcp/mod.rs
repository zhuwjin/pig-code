//! MCP（Model Context Protocol）支持：stdio（newline JSON-RPC 2.0）与 streamable HTTP
//! 两种传输，Claude Code 兼容配置（项目级 `.pigcode/mcp.json` 覆盖用户级
//! `<data_dir>/mcp.json`）。每个 server 工具包装为 Tool trait 实现
//!（`mcp__<server>__<tool>`），连接随 session 首个 step 懒建立、退出时 `shutdown`。
//! 远程形态即 streamable HTTP（2025-03-26+ 单端点 POST，JSON/SSE 双响应形态）；
//! legacy SSE（2024-11-05 双端点）暂不支持，config 层遇到 `"type": "sse"` 记录后跳过。
//! 后续项：OAuth、resources/prompts、roots/elicitation、legacy SSE。

mod client;
mod config;
mod http;
mod naming;
mod tool;

pub use client::{McpClient, McpToolSpec};
pub use config::{McpHttpConfig, McpServerConfig, McpStdioConfig, McpTransport};
pub use tool::{McpTool, McpToolAnnotations};

use std::path::Path;
use std::sync::Arc;

use crate::tool::Tool;

/// 一个已连接的 server：客户端句柄 + 工具清单
struct McpServer {
    client: Arc<McpClient>,
    tools: Vec<Arc<McpTool>>,
}

/// MCP 连接管理器：读配置、并发连接全部 server；单 server 失败不影响其他
pub struct McpManager {
    servers: Vec<McpServer>,
}

impl McpManager {
    /// 读配置并并发连接所有 server；失败逐个记录并跳过（不致命）
    pub async fn connect_all(workspace_root: &Path, data_dir: &Path) -> Self {
        let configs = config::load(workspace_root, data_dir);
        let servers =
            futures_util::future::join_all(configs.into_iter().map(|config| async move {
                let (client, specs) = match client::McpClient::connect(&config).await {
                    Ok(connected) => connected,
                    Err(e) => {
                        eprintln!("[mcp] 连接 {} 失败，已跳过: {e}", config.name);
                        return None;
                    }
                };
                let tools = specs
                    .into_iter()
                    .map(|spec| Arc::new(McpTool::new(&config.name, spec, client.clone())))
                    .collect();
                Some(McpServer { client, tools })
            }))
            .await
            .into_iter()
            .flatten()
            .collect();
        Self { servers }
    }

    /// 全部 MCP 工具（Box<dyn Tool>，与 tool::all() 同形态，直接 extend 进工具集）
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

    /// 按组合名（`mcp__<server>__<tool>`）取单个工具句柄（并发组任务按名现取；
    /// McpTool clone 即 Arc 克隆，很便宜）
    pub fn tool_named(&self, name: &str) -> Option<Box<dyn Tool>> {
        self.servers
            .iter()
            .flat_map(|server| server.tools.iter())
            .find(|tool| tool.name() == name)
            .map(|tool| Box::new((**tool).clone()) as Box<dyn Tool>)
    }

    /// 子代理继承的 MCP 工具集：inherits_all=false（只读档案）只给 readOnlyHint 工具
    pub fn child_tools(&self, inherits_all: bool) -> Vec<Box<dyn Tool>> {
        self.tools()
            .into_iter()
            .filter(|tool| inherits_all || tool.read_only())
            .collect()
    }

    /// 已连接 server 名清单（诊断/设置页展示用）
    pub fn server_names(&self) -> Vec<&str> {
        self.servers
            .iter()
            .map(|server| server.client.name())
            .collect()
    }

    /// ping 全部 server（探活）
    pub async fn ping_all(&self) -> Vec<(String, Result<(), String>)> {
        futures_util::future::join_all(
            self.servers.iter().map(|server| async {
                (server.client.name().to_string(), server.client.ping().await)
            }),
        )
        .await
    }

    /// 关闭全部连接（stdio 杀子进程并回收；http 尽力 DELETE 终止会话）
    pub async fn shutdown(&self) {
        futures_util::future::join_all(self.servers.iter().map(|server| server.client.shutdown()))
            .await;
    }

    /// 测试用：单个假 server 的 manager（Noop 传输，只承载工具清单/只读标记）
    #[cfg(test)]
    pub(crate) fn for_test(server: &str, specs: Vec<McpToolSpec>) -> Self {
        let client = McpClient::for_test(server);
        let tools = specs
            .into_iter()
            .map(|spec| Arc::new(McpTool::new(server, spec, client.clone())))
            .collect();
        Self {
            servers: vec![McpServer { client, tools }],
        }
    }
}
