//! Read-only runtime tools exposing MCP `resources` and `prompts` capabilities
//! (`mcp_list_resources`, `mcp_read_resource`, `mcp_list_prompts`,
//! `mcp_get_prompt`).
//!
//! Because they are returned from [`super::McpManager::tools`] only when at
//! least one connected server advertises the matching capability, they ride the
//! existing `ToolSearch` + `CallTool` deferred proxy pair — keeping top-level
//! `tools[]` byte-identical across turns and inheriting into read-only
//! subagents automatically.

use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::mcp::client::McpClient;
use crate::mcp::tool::truncate_output;
use crate::tool::{Tool, ToolContext, ToolEffect};

pub const MCP_LIST_RESOURCES_TOOL: &str = "mcp_list_resources";
pub const MCP_READ_RESOURCE_TOOL: &str = "mcp_read_resource";
pub const MCP_LIST_PROMPTS_TOOL: &str = "mcp_list_prompts";
pub const MCP_GET_PROMPT_TOOL: &str = "mcp_get_prompt";

fn text_effect(output: String) -> ToolEffect {
    ToolEffect {
        output: truncate_output(output),
        file_changes: vec![],
        edit_diff: None,
        images: vec![],
        documents: vec![],
        delivered_notification_ids: vec![],
    }
}

/// Read-only tool: list resources advertised by connected MCP servers.
#[derive(Clone)]
pub(super) struct McpListResourcesTool {
    clients: Vec<Arc<McpClient>>,
}

impl McpListResourcesTool {
    pub(super) fn new(clients: Vec<Arc<McpClient>>) -> Self {
        Self { clients }
    }
}

impl Tool for McpListResourcesTool {
    fn name(&self) -> &str {
        MCP_LIST_RESOURCES_TOOL
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": MCP_LIST_RESOURCES_TOOL,
                "description": "List resources advertised by connected MCP servers (resources/list).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "server": {
                            "type": "string",
                            "description": "Optional MCP server name filter; omit to list across all resource-capable servers."
                        }
                    }
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let filter = args
                .get("server")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let targets: Vec<&Arc<McpClient>> = self
                .clients
                .iter()
                .filter(|c| c.capabilities().resources && filter.is_none_or(|f| c.name() == f))
                .collect();
            if targets.is_empty() {
                return match filter {
                    Some(name) => Err(format!(
                        "No connected MCP server named `{name}` advertises `resources` capability."
                    )),
                    None => Ok(text_effect(
                        "No connected MCP servers advertise resources.".to_string(),
                    )),
                };
            }
            let results = futures_util::future::join_all(targets.into_iter().map(|client| async {
                let name = client.name().to_string();
                let res = client.list_resources().await;
                let templates = client.list_resource_templates().await.ok();
                (name, res, templates)
            }))
            .await;

            let mut lines = Vec::new();
            for (server, result, templates) in results {
                match result {
                    Ok(specs) => {
                        lines.push(format!("## Server: {server} ({} resources)", specs.len()));
                        for spec in specs {
                            let mime = spec
                                .mime_type
                                .as_deref()
                                .map(|m| format!(" [{m}]"))
                                .unwrap_or_default();
                            let desc = spec
                                .description
                                .as_deref()
                                .map(|d| format!(" — {d}"))
                                .unwrap_or_default();
                            lines.push(format!("- `{}` ({}){mime}{desc}", spec.uri, spec.name));
                        }
                        if let Some(templates) = templates
                            && !templates.is_empty()
                        {
                            lines.push(format!("Templates ({}):", templates.len()));
                            for tpl in templates {
                                let desc = tpl
                                    .description
                                    .as_deref()
                                    .map(|d| format!(" — {d}"))
                                    .unwrap_or_default();
                                lines.push(format!(
                                    "- `{}` ({}){desc}",
                                    tpl.uri_template, tpl.name
                                ));
                            }
                        }
                    }
                    Err(err) => {
                        lines.push(format!("## Server: {server} (error: {err})"));
                    }
                }
            }
            Ok(text_effect(lines.join("\n")))
        })
    }
}

/// Read-only tool: read a specific resource URI from a named MCP server.
#[derive(Clone)]
pub(super) struct McpReadResourceTool {
    clients: Vec<Arc<McpClient>>,
}

impl McpReadResourceTool {
    pub(super) fn new(clients: Vec<Arc<McpClient>>) -> Self {
        Self { clients }
    }
}

impl Tool for McpReadResourceTool {
    fn name(&self) -> &str {
        MCP_READ_RESOURCE_TOOL
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": MCP_READ_RESOURCE_TOOL,
                "description": "Read a resource URI from a connected MCP server (resources/read).",
                "parameters": {
                    "type": "object",
                    "required": ["server", "uri"],
                    "properties": {
                        "server": {
                            "type": "string",
                            "description": "Name of the MCP server owning the resource."
                        },
                        "uri": {
                            "type": "string",
                            "description": "Resource URI to read (from mcp_list_resources)."
                        }
                    }
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let server = args
                .get("server")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "mcp_read_resource requires a non-empty `server`".to_string())?;
            let uri = args
                .get("uri")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "mcp_read_resource requires a non-empty `uri`".to_string())?;
            let client = self
                .clients
                .iter()
                .find(|c| c.name() == server)
                .ok_or_else(|| format!("MCP server not found: {server}"))?;
            let contents = client.read_resource(uri).await?;
            if contents.is_empty() {
                return Ok(text_effect(format!("[resource {uri}: empty]")));
            }
            let mut parts = Vec::with_capacity(contents.len());
            for item in contents {
                let mime = item
                    .mime_type
                    .as_deref()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default();
                if let Some(text) = item.text {
                    parts.push(format!("[resource {}{mime}]\n{text}", item.uri));
                } else if let Some(blob) = item.blob_base64 {
                    let kb = blob.len() * 3 / 4 / 1024;
                    parts.push(format!(
                        "[resource {}{mime}: ~{kb} KB binary content not inlined]",
                        item.uri
                    ));
                } else {
                    parts.push(format!("[resource {}{mime}: empty]", item.uri));
                }
            }
            Ok(text_effect(parts.join("\n\n")))
        })
    }
}

/// Read-only tool: list prompts advertised by connected MCP servers.
#[derive(Clone)]
pub(super) struct McpListPromptsTool {
    clients: Vec<Arc<McpClient>>,
}

impl McpListPromptsTool {
    pub(super) fn new(clients: Vec<Arc<McpClient>>) -> Self {
        Self { clients }
    }
}

impl Tool for McpListPromptsTool {
    fn name(&self) -> &str {
        MCP_LIST_PROMPTS_TOOL
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": MCP_LIST_PROMPTS_TOOL,
                "description": "List prompt templates advertised by connected MCP servers (prompts/list).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "server": {
                            "type": "string",
                            "description": "Optional MCP server name filter; omit to list across all prompt-capable servers."
                        }
                    }
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let filter = args
                .get("server")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let targets: Vec<&Arc<McpClient>> = self
                .clients
                .iter()
                .filter(|c| c.capabilities().prompts && filter.is_none_or(|f| c.name() == f))
                .collect();
            if targets.is_empty() {
                return match filter {
                    Some(name) => Err(format!(
                        "No connected MCP server named `{name}` advertises `prompts` capability."
                    )),
                    None => Ok(text_effect(
                        "No connected MCP servers advertise prompts.".to_string(),
                    )),
                };
            }
            let results = futures_util::future::join_all(targets.into_iter().map(|client| async {
                (client.name().to_string(), client.list_prompts().await)
            }))
            .await;

            let mut lines = Vec::new();
            for (server, result) in results {
                match result {
                    Ok(specs) => {
                        lines.push(format!("## Server: {server} ({} prompts)", specs.len()));
                        for spec in specs {
                            let desc = spec
                                .description
                                .as_deref()
                                .map(|d| format!(" — {d}"))
                                .unwrap_or_default();
                            let args_list: Vec<String> = spec
                                .arguments
                                .iter()
                                .map(|a| {
                                    if a.required {
                                        format!("{}*", a.name)
                                    } else {
                                        a.name.clone()
                                    }
                                })
                                .collect();
                            let args_note = if args_list.is_empty() {
                                String::new()
                            } else {
                                format!(" (args: {})", args_list.join(", "))
                            };
                            lines.push(format!("- `{}`{args_note}{desc}", spec.name));
                        }
                    }
                    Err(err) => {
                        lines.push(format!("## Server: {server} (error: {err})"));
                    }
                }
            }
            Ok(text_effect(lines.join("\n")))
        })
    }
}

/// Read-only tool: render/fetch a prompt template from a named MCP server.
#[derive(Clone)]
pub(super) struct McpGetPromptTool {
    clients: Vec<Arc<McpClient>>,
}

impl McpGetPromptTool {
    pub(super) fn new(clients: Vec<Arc<McpClient>>) -> Self {
        Self { clients }
    }
}

impl Tool for McpGetPromptTool {
    fn name(&self) -> &str {
        MCP_GET_PROMPT_TOOL
    }

    fn read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": MCP_GET_PROMPT_TOOL,
                "description": "Fetch and render a prompt template from a connected MCP server (prompts/get).",
                "parameters": {
                    "type": "object",
                    "required": ["server", "name"],
                    "properties": {
                        "server": {
                            "type": "string",
                            "description": "Name of the MCP server owning the prompt."
                        },
                        "name": {
                            "type": "string",
                            "description": "Prompt template name (from mcp_list_prompts)."
                        },
                        "arguments": {
                            "type": "object",
                            "description": "Optional template arguments."
                        }
                    }
                }
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let server = args
                .get("server")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "mcp_get_prompt requires a non-empty `server`".to_string())?;
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "mcp_get_prompt requires a non-empty `name`".to_string())?;
            let arguments = args.get("arguments").and_then(Value::as_object).cloned();
            let client = self
                .clients
                .iter()
                .find(|c| c.name() == server)
                .ok_or_else(|| format!("MCP server not found: {server}"))?;
            let rendered = client.get_prompt(name, arguments).await?;
            let mut parts = Vec::new();
            if let Some(desc) = rendered.description.filter(|d| !d.trim().is_empty()) {
                parts.push(format!("Description: {desc}"));
            }
            for msg in rendered.messages {
                let body = match &msg.content {
                    Value::String(s) => s.clone(),
                    Value::Object(map) => map
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| msg.content.to_string()),
                    other => other.to_string(),
                };
                parts.push(format!("[{}]\n{body}", msg.role));
            }
            if parts.is_empty() {
                parts.push("(prompt returned no messages)".to_string());
            }
            Ok(text_effect(parts.join("\n\n")))
        })
    }
}
