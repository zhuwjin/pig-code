use serde::{Deserialize, Serialize};

use crate::events::ProviderEvent;

use super::documents::ChatDocument;
use super::replay::AssistantReplay;

/// Images entering the context with a message (ReadMediaFile output): Anthropic puts them into content blocks,
/// OpenAI splits them into a following user image_url message (see to_openai_messages)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatImage {
    pub media_type: String,
    pub data_base64: String,
    /// For display/labeling (the text part of the OpenAI split message); not consumed on the Anthropic path
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Subagent context persistence ({session}.agents/*.jsonl) needs deserialization, hence Deserialize
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMsg {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallWire>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Images from tool output: the OpenAI path bypasses direct serde serialization (see to_openai_messages),
    /// this field is only read by Anthropic's custom construction
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ChatImage>,
    /// PDF documents from tool output (ReadMediaFile). Never serialized by the
    /// generic Chat projection: core persists document references, and the
    /// provider builds the per-format wire blocks explicitly.
    #[serde(default, skip_serializing)]
    pub documents: Vec<ChatDocument>,
    /// Plaintext fallback and display text; native replay items carry provider-specific credentials.
    #[serde(skip_serializing)]
    pub reasoning: Option<String>,
    /// Ordered provider-native replay projection; persisted explicitly by core, never by Chat Completions serde.
    #[serde(default, skip_serializing)]
    pub replay: Option<AssistantReplay>,
    /// Execution status of a tool result, carried from the real tool outcome
    /// (never inferred from the output text). Only Anthropic has a wire field
    /// for it (`tool_result.is_error`); the other formats ignore it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    /// Cancellation receipt state for display and replay. This is persisted
    /// explicitly, never inferred from the output text or sent on provider wires.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stopped: bool,
}

pub const STOPPED_TOOL_OUTPUT: &str = "The user stopped this tool call. Do not retry it automatically; wait for the user's next instruction.";

pub(crate) fn tool_result_output(msg: &ChatMsg) -> String {
    if msg.stopped {
        STOPPED_TOOL_OUTPUT.to_string()
    } else {
        msg.content.clone().unwrap_or_default()
    }
}

impl ChatMsg {
    pub fn system(content: String) -> Self {
        Self {
            role: "system".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
            documents: vec![],
            reasoning: None,
            replay: None,
            is_error: false,
            stopped: false,
        }
    }

    pub fn user(content: String) -> Self {
        Self {
            role: "user".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
            documents: vec![],
            reasoning: None,
            replay: None,
            is_error: false,
            stopped: false,
        }
    }

    pub fn assistant(
        content: String,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<String>,
    ) -> Self {
        Self::assistant_with_replay(content, tool_calls, reasoning, None)
    }

    pub fn assistant_with_replay(
        content: String,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<String>,
        replay: Option<AssistantReplay>,
    ) -> Self {
        Self {
            role: "assistant".into(),
            content: (!content.is_empty()).then_some(content),
            tool_calls: (!tool_calls.is_empty())
                .then(|| tool_calls.iter().map(ToolCall::to_wire).collect()),
            tool_call_id: None,
            images: vec![],
            documents: vec![],
            reasoning: reasoning.filter(|r| !r.is_empty()),
            replay,
            is_error: false,
            stopped: false,
        }
    }

    pub fn tool_result(call_id: &str, output: String) -> Self {
        Self {
            role: "tool".into(),
            content: Some(output),
            tool_calls: None,
            tool_call_id: Some(call_id.to_string()),
            images: vec![],
            documents: vec![],
            reasoning: None,
            replay: None,
            is_error: false,
            stopped: false,
        }
    }

    /// Build a cancellation receipt: distinct in display state, but still an
    /// error to the model through Anthropic's `tool_result.is_error` field.
    pub fn stopped_tool_result(call_id: &str, output: String) -> Self {
        let mut msg = Self::tool_result(call_id, output);
        msg.stopped = true;
        msg.is_error = true;
        msg
    }

    /// Record the real execution status of this tool result. Anthropic sends
    /// it as `tool_result.is_error`; Chat and Responses do not serialize it.
    pub fn with_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    /// Tool result carrying images (ReadMediaFile): the images enter the context along with history
    pub fn tool_result_with_images(call_id: &str, output: String, images: Vec<ChatImage>) -> Self {
        let mut msg = Self::tool_result(call_id, output);
        msg.images = images;
        msg
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Subagent context persistence ({session}.agents/*.jsonl) needs deserialization, hence Deserialize
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallWire {
    pub id: String,
    /// Always "function": &'static str cannot be deserialized, so store a String (serialized shape unchanged)
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionWire,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionWire {
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    pub fn to_wire(&self) -> ToolCallWire {
        ToolCallWire {
            id: self.id.clone(),
            kind: "function".to_string(),
            function: FunctionWire {
                name: self.name.clone(),
                arguments: self.arguments.clone(),
            },
        }
    }
}

pub(crate) fn finish(
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    tool_calls: Vec<ToolCall>,
    reason: pig_protocol::FinishReason,
) {
    // Indexed protocols can contain placeholders for non-tool blocks; only those empty entries are filtered.
    let calls: Vec<ToolCall> = tool_calls
        .into_iter()
        .filter(|call| !call.name.trim().is_empty())
        .collect();
    if !calls.is_empty() {
        let _ = tx.send(ProviderEvent::ToolCalls(calls));
    }
    let _ = tx.send(ProviderEvent::Finished(reason));
}

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}
