//! MCP protocol capabilities: server capability flags, resources, prompts,
//! elicitation (server -> client structured user prompts), and connection
//! lifecycle state.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Negotiated MCP server capability summary parsed from `initialize.result.capabilities`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct McpServerCapabilities {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
    pub logging: bool,
}

impl McpServerCapabilities {
    pub fn from_initialize_result(result: &Value) -> Self {
        let caps = &result["capabilities"];
        Self {
            tools: caps.get("tools").is_some_and(|v| !v.is_null()),
            resources: caps.get("resources").is_some_and(|v| !v.is_null()),
            prompts: caps.get("prompts").is_some_and(|v| !v.is_null()),
            logging: caps.get("logging").is_some_and(|v| !v.is_null()),
        }
    }
}

/// Lifecycle state of an [`super::McpClient`] connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpConnectionState {
    Connected,
    Reconnecting { attempt: u32 },
    Disconnected { reason: String },
}

/// One resource advertised by `resources/list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResourceSpec {
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

/// One URI template advertised by `resources/templates/list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResourceTemplate {
    pub uri_template: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

/// One content block returned by `resources/read`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResourceContent {
    pub uri: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, rename = "blob")]
    pub blob_base64: Option<String>,
}

/// One parameter accepted by an MCP prompt (`prompts/list`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// One prompt template advertised by `prompts/list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<McpPromptArgument>,
}

/// One message returned by `prompts/get`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpPromptMessage {
    pub role: String,
    pub content: Value,
}

/// Result of `prompts/get`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpPromptResult {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub messages: Vec<McpPromptMessage>,
}

/// User decision for a server-initiated `elicitation/create` request (MCP 2025-06-18+).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpElicitationAction {
    Accept,
    Decline,
    Cancel,
}

/// Server-initiated `elicitation/create` request payload.
#[derive(Clone, Debug, PartialEq)]
pub struct McpElicitationRequest {
    pub server: String,
    pub message: String,
    pub requested_schema: Value,
}

/// Client reply to `elicitation/create`.
#[derive(Clone, Debug, PartialEq)]
pub struct McpElicitationResponse {
    pub action: McpElicitationAction,
    pub content: Option<Value>,
}

impl McpElicitationResponse {
    pub fn accept(content: Value) -> Self {
        Self {
            action: McpElicitationAction::Accept,
            content: Some(content),
        }
    }

    pub fn decline() -> Self {
        Self {
            action: McpElicitationAction::Decline,
            content: None,
        }
    }

    pub fn cancel() -> Self {
        Self {
            action: McpElicitationAction::Cancel,
            content: None,
        }
    }

    pub fn to_json_result(&self) -> Value {
        let mut obj = serde_json::Map::new();
        let action = match self.action {
            McpElicitationAction::Accept => "accept",
            McpElicitationAction::Decline => "decline",
            McpElicitationAction::Cancel => "cancel",
        };
        obj.insert("action".to_string(), Value::String(action.to_string()));
        if let Some(content) = &self.content {
            obj.insert("content".to_string(), content.clone());
        }
        Value::Object(obj)
    }
}

/// Async callback handling server-initiated MCP elicitation prompts.
pub type ElicitationHandler =
    Arc<dyn Fn(McpElicitationRequest) -> BoxFuture<'static, McpElicitationResponse> + Send + Sync>;

/// Dispatch a server-initiated JSON-RPC request (`id` + `method`) to the
/// appropriate client handler and construct the JSON-RPC 2.0 response envelope.
pub(super) async fn handle_server_request(
    server_name: &str,
    id: Value,
    method: &str,
    params: &Value,
    elicitation: Option<&ElicitationHandler>,
) -> Value {
    if method == "elicitation/create"
        && let Some(handler) = elicitation
    {
        let request = McpElicitationRequest {
            server: server_name.to_string(),
            message: params["message"].as_str().unwrap_or("").to_string(),
            requested_schema: params
                .get("requestedSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"})),
        };
        let response = handler(request).await;
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": response.to_json_result(),
        });
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32601,
            "message": format!("Pig Code does not support {method}"),
        },
    })
}
