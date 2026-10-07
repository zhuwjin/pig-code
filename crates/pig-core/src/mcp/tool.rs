//! Tool trait adapter for MCP tools: naming `mcp__<server>__<tool>` (sanitized + truncated to 64 chars),
//! annotations.readOnlyHint maps to the read-only decision, output is truncated head/tail against a 30KB budget.

use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::mcp::client::{McpClient, McpToolSpec};
use crate::mcp::naming;
use crate::tool::{Tool, ToolContext, ToolEffect};

/// Character budget for a single call's output (aligned with Bash's 30KB): beyond it, a head/tail preview
const MAX_MCP_OUTPUT_CHARS: usize = 30 * 1024;
const HEAD_CHARS: usize = 20 * 1024;
const TAIL_CHARS: usize = 8 * 1024;

/// MCP tool annotations (2024-11-05): readOnly drives approval, other hints are kept for future permission refinement
#[derive(Debug, Default, Clone, Copy, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpToolAnnotations {
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

impl McpToolAnnotations {
    /// Read-only decision: only readOnlyHint == true; missing annotations conservatively count as non-read-only (thus go through approval)
    pub fn is_read_only(&self) -> bool {
        self.read_only_hint.unwrap_or(false)
    }
}

/// Tool wrapper for one MCP tool: calls go through Arc<McpClient> into the server subprocess
#[derive(Clone)]
pub struct McpTool {
    /// Sanitized combined name (interned as &'static; the same string leaks only once)
    name: &'static str,
    /// Original MCP tool name (params.name of tools/call)
    tool_name: String,
    description: String,
    input_schema: Value,
    annotations: McpToolAnnotations,
    client: Arc<McpClient>,
}

impl McpTool {
    pub fn new(server_name: &str, spec: McpToolSpec, client: Arc<McpClient>) -> Self {
        Self {
            name: naming::intern(naming::tool_name(server_name, &spec.name)),
            description: spec
                .description
                .unwrap_or_else(|| format!("Tool {} from MCP server {server_name}", spec.name)),
            input_schema: spec.input_schema,
            annotations: spec.annotations,
            client,
            tool_name: spec.name,
        }
    }

    /// Raw annotations (for a future permission matrix: destructive/idempotent/openWorld)
    pub fn annotations(&self) -> &McpToolAnnotations {
        &self.annotations
    }
}

impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn read_only(&self) -> bool {
        self.annotations.is_read_only()
    }

    fn schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.input_schema,
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: ToolContext<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolEffect, String>> + Send + 'a>> {
        Box::pin(async move {
            let result = self
                .client
                .call_tool(&self.tool_name, args)
                .await
                .map_err(|e| {
                    let tail = self.client.stderr_tail();
                    if tail.is_empty() {
                        e
                    } else {
                        format!("{e}\n\nserver stderr tail:\n{tail}")
                    }
                })?;
            let (text, is_error) = render_result(&result);
            if is_error {
                Err(text)
            } else {
                Ok(ToolEffect {
                    output: text,
                    file_change: None,
                    edit_diff: None,
                    images: vec![],
                })
            }
        })
    }
}

/// tools/call result -> (text, isError): text parts joined; image/audio/resource get placeholder descriptions (base64 not inlined)
fn render_result(result: &Value) -> (String, bool) {
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    for item in result["content"].as_array().into_iter().flatten() {
        match item["type"].as_str().unwrap_or("") {
            "text" => parts.push(item["text"].as_str().unwrap_or("").to_string()),
            "image" | "audio" => {
                let kind = if item["type"] == "image" {
                    "image"
                } else {
                    "audio"
                };
                let mime = item["mimeType"].as_str().unwrap_or("unknown");
                let kb = item["data"].as_str().map(str::len).unwrap_or(0) * 3 / 4 / 1024;
                parts.push(format!(
                    "[{kind} content: {mime}, ~{kb} KB, binary content not inlined]"
                ));
            }
            "resource" => {
                let resource = &item["resource"];
                let uri = resource["uri"].as_str().unwrap_or("?");
                match resource["text"].as_str() {
                    Some(text) => parts.push(format!("[resource {uri}]\n{text}")),
                    None => {
                        let mime = resource["mimeType"].as_str().unwrap_or("unknown");
                        parts.push(format!(
                            "[resource {uri}: {mime}, binary content not inlined]"
                        ));
                    }
                }
            }
            other => parts.push(format!("[unknown content type: {other}]")),
        }
    }
    let text = parts.join("\n");
    let text = if text.is_empty() {
        if is_error {
            "MCP tool returned an error (no details)".to_string()
        } else {
            "(no output)".to_string()
        }
    } else {
        text
    };
    (truncate_output(text), is_error)
}

/// Over-budget head/tail preview (same idea as Bash; MCP has no spill file, the omitted section is simply dropped)
fn truncate_output(text: String) -> String {
    let total = text.chars().count();
    if total <= MAX_MCP_OUTPUT_CHARS {
        return text;
    }
    let head: String = text.chars().take(HEAD_CHARS).collect();
    let tail = crate::task::tail_chars(&text, TAIL_CHARS);
    let omitted = total - head.chars().count() - tail.chars().count();
    format!("{head}\n\n[... {omitted} characters omitted ...]\n\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotations_read_only_mapping() {
        assert!(
            McpToolAnnotations {
                read_only_hint: Some(true),
                ..Default::default()
            }
            .is_read_only()
        );
        assert!(
            !McpToolAnnotations {
                read_only_hint: Some(false),
                ..Default::default()
            }
            .is_read_only()
        );
        assert!(!McpToolAnnotations::default().is_read_only());
    }

    #[test]
    fn annotations_parse_camel_case() {
        let parsed: McpToolAnnotations = serde_json::from_value(json!({
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
            "openWorldHint": true
        }))
        .expect("parse");
        assert!(parsed.is_read_only());
        assert_eq!(parsed.destructive_hint, Some(false));
        assert_eq!(parsed.idempotent_hint, Some(true));
        assert_eq!(parsed.open_world_hint, Some(true));
    }

    #[test]
    fn render_text_join_and_image_placeholder() {
        let (text, is_error) = render_result(&json!({"content": [
            {"type": "text", "text": "first line"},
            {"type": "image", "data": "a".repeat(1024), "mimeType": "image/png"},
            {"type": "text", "text": "second line"}
        ]}));
        assert!(!is_error);
        assert!(
            text.contains("first line\n[image content: image/png"),
            "{text}"
        );
        assert!(text.ends_with("second line"));
    }

    #[test]
    fn render_is_error_flag() {
        let (text, is_error) =
            render_result(&json!({"isError": true, "content": [{"type": "text", "text": "boom"}]}));
        assert!(is_error);
        assert_eq!(text, "boom");
        // Fallback text for empty content
        let (text, is_error) = render_result(&json!({"isError": true}));
        assert!(is_error);
        assert_eq!(text, "MCP tool returned an error (no details)");
    }

    #[test]
    fn truncate_long_output() {
        let long = "x".repeat(MAX_MCP_OUTPUT_CHARS + 4096);
        let (text, _) = render_result(&json!({"content": [{"type": "text", "text": long}]}));
        assert!(text.contains("characters omitted"), "{text}");
        assert!(text.len() < MAX_MCP_OUTPUT_CHARS + 4096);
    }
}
