//! MCP 工具的 Tool trait 适配：命名 `mcp__<server>__<tool>`（清洗 + 64 字符截断），
//! annotations.readOnlyHint 映射只读判定，输出按 30KB 预算头/尾截断。

use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::mcp::client::{McpClient, McpToolSpec};
use crate::mcp::naming;
use crate::tool::{Tool, ToolContext, ToolEffect};

/// 单次调用输出字符预算（对齐 Bash 的 30KB）：超出头/尾预览
const MAX_MCP_OUTPUT_CHARS: usize = 30 * 1024;
const HEAD_CHARS: usize = 20 * 1024;
const TAIL_CHARS: usize = 8 * 1024;

/// MCP tool annotations（2024-11-05）：readOnly 驱动审批，其余 hint 保留供后续权限细化
#[derive(Debug, Default, Clone, Copy, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct McpToolAnnotations {
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub idempotent_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

impl McpToolAnnotations {
    /// 只读判定：仅 readOnlyHint == true；无 annotations 保守按非只读（自然走审批）
    pub fn is_read_only(&self) -> bool {
        self.read_only_hint.unwrap_or(false)
    }
}

/// 一个 MCP tool 的 Tool 包装：调用经 Arc<McpClient> 进 server 子进程
#[derive(Clone)]
pub struct McpTool {
    /// 清洗后的组合名（intern 成 &'static，同串只泄漏一次）
    name: &'static str,
    /// 原始 MCP 工具名（tools/call 的 params.name）
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
                .unwrap_or_else(|| format!("MCP server {server_name} 的工具 {}", spec.name)),
            input_schema: spec.input_schema,
            annotations: spec.annotations,
            client,
            tool_name: spec.name,
        }
    }

    /// annotations 原文（供后续权限矩阵使用：destructive/idempotent/openWorld）
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
                        format!("{e}\n\nserver stderr 尾部：\n{tail}")
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

/// tools/call result → (文本, isError)：text 拼接；image/audio/resource 占位描述（不内联 base64）
fn render_result(result: &Value) -> (String, bool) {
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    for item in result["content"].as_array().into_iter().flatten() {
        match item["type"].as_str().unwrap_or("") {
            "text" => parts.push(item["text"].as_str().unwrap_or("").to_string()),
            "image" | "audio" => {
                let kind = if item["type"] == "image" {
                    "图片"
                } else {
                    "音频"
                };
                let mime = item["mimeType"].as_str().unwrap_or("未知类型");
                let kb = item["data"].as_str().map(str::len).unwrap_or(0) * 3 / 4 / 1024;
                parts.push(format!(
                    "[{kind} content: {mime}，约 {kb} KB，二进制内容不内联]"
                ));
            }
            "resource" => {
                let resource = &item["resource"];
                let uri = resource["uri"].as_str().unwrap_or("?");
                match resource["text"].as_str() {
                    Some(text) => parts.push(format!("[资源 {uri}]\n{text}")),
                    None => {
                        let mime = resource["mimeType"].as_str().unwrap_or("未知类型");
                        parts.push(format!("[资源 {uri}: {mime}，二进制内容不内联]"));
                    }
                }
            }
            other => parts.push(format!("[未知 content 类型: {other}]")),
        }
    }
    let text = parts.join("\n");
    let text = if text.is_empty() {
        if is_error {
            "MCP 工具返回错误（无详情）".to_string()
        } else {
            "（无输出）".to_string()
        }
    } else {
        text
    };
    (truncate_output(text), is_error)
}

/// 超预算头/尾预览（Bash 同款思路；MCP 无 spill 文件，省略段直接丢弃）
fn truncate_output(text: String) -> String {
    let total = text.chars().count();
    if total <= MAX_MCP_OUTPUT_CHARS {
        return text;
    }
    let head: String = text.chars().take(HEAD_CHARS).collect();
    let tail = crate::task::tail_chars(&text, TAIL_CHARS);
    let omitted = total - head.chars().count() - tail.chars().count();
    format!("{head}\n\n[...中间省略 {omitted} 字符...]\n\n{tail}")
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
            {"type": "text", "text": "第一行"},
            {"type": "image", "data": "a".repeat(1024), "mimeType": "image/png"},
            {"type": "text", "text": "第二行"}
        ]}));
        assert!(!is_error);
        assert!(text.contains("第一行\n[图片 content: image/png"), "{text}");
        assert!(text.ends_with("第二行"));
    }

    #[test]
    fn render_is_error_flag() {
        let (text, is_error) =
            render_result(&json!({"isError": true, "content": [{"type": "text", "text": "boom"}]}));
        assert!(is_error);
        assert_eq!(text, "boom");
        // 空 content 的兜底文案
        let (text, is_error) = render_result(&json!({"isError": true}));
        assert!(is_error);
        assert_eq!(text, "MCP 工具返回错误（无详情）");
    }

    #[test]
    fn truncate_long_output() {
        let long = "x".repeat(MAX_MCP_OUTPUT_CHARS + 4096);
        let (text, _) = render_result(&json!({"content": [{"type": "text", "text": long}]}));
        assert!(text.contains("[...中间省略"), "{text}");
        assert!(text.len() < MAX_MCP_OUTPUT_CHARS + 4096);
    }
}
