use serde::{Deserialize, Serialize};

use pig_protocol::ApiFormat;

use crate::events::ProviderEvent;

/// Resolved model endpoint config (product of merging provider + model + reasoning level)
#[derive(Clone, Debug)]
pub struct ResolvedModel {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub api_format: ApiFormat,
    pub reasoning_params: Option<serde_json::Value>,
    pub cap_web_search: bool,
    /// Model supports native structured output (gates sidecar schema forcing;
    /// see sidecar::apply_structured)
    pub cap_structured: bool,
    pub web_search_tool: Option<serde_json::Value>,
    /// Model supports image input (gate for ReadMediaFile)
    pub input_image: bool,
    /// For display
    pub provider_name: String,
}

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
    /// Assistant thinking content: only echoed back to Anthropic endpoints (thinking mode requires
    /// content[].thinking, otherwise the second turn 400s); excluded from serialization -- OpenAI-compatible endpoints
    /// (DeepSeek native) require not echoing reasoning_content back.
    #[serde(skip_serializing)]
    pub reasoning: Option<String>,
}

impl ChatMsg {
    pub fn system(content: String) -> Self {
        Self {
            role: "system".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
            reasoning: None,
        }
    }

    pub fn user(content: String) -> Self {
        Self {
            role: "user".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
            images: vec![],
            reasoning: None,
        }
    }

    pub fn assistant(
        content: String,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<String>,
    ) -> Self {
        Self {
            role: "assistant".into(),
            content: (!content.is_empty()).then_some(content),
            tool_calls: (!tool_calls.is_empty())
                .then(|| tool_calls.iter().map(ToolCall::to_wire).collect()),
            tool_call_id: None,
            images: vec![],
            reasoning: reasoning.filter(|r| !r.is_empty()),
        }
    }

    pub fn tool_result(call_id: &str, output: String) -> Self {
        Self {
            role: "tool".into(),
            content: Some(output),
            tool_calls: None,
            tool_call_id: Some(call_id.to_string()),
            images: vec![],
            reasoning: None,
        }
    }

    /// Tool result carrying images (ReadMediaFile): the images enter the context along with history
    pub fn tool_result_with_images(call_id: &str, output: String, images: Vec<ChatImage>) -> Self {
        let mut msg = Self::tool_result(call_id, output);
        msg.images = images;
        msg
    }
}

#[derive(Clone, Debug, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Subagent context persistence ({session}.agents/*.jsonl) needs deserialization, hence Deserialize
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallWire {
    pub id: String,
    /// Always "function": &'static str cannot be deserialized, so store a String (serialized shape unchanged)
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionWire,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    tool_calls: &mut Vec<ToolCall>,
) {
    // Models occasionally emit nameless tool_use (name: null): filter them out to avoid empty "unknown tool" calls;
    // since they never enter history, no tool_result needs to be backfilled for them
    let calls: Vec<ToolCall> = std::mem::take(tool_calls)
        .into_iter()
        .filter(|call| !call.name.trim().is_empty())
        .collect();
    if !calls.is_empty() {
        let _ = tx.send(ProviderEvent::ToolCalls(calls));
    }
    let _ = tx.send(ProviderEvent::Finished);
}

pub(crate) fn merge_reasoning_params(body: &mut serde_json::Value, config: &ResolvedModel) {
    if let (serde_json::Value::Object(map), Some(params)) = (body, &config.reasoning_params)
        && let serde_json::Value::Object(extra) = params
    {
        for (key, value) in extra {
            map.insert(key.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use pig_protocol::ApiFormat;

    use super::{ChatImage, ChatMsg, ResolvedModel, ToolCall, finish};
    use crate::anthropic::{anthropic_request_tools, to_anthropic_messages};
    use crate::events::ProviderEvent;
    use crate::openai::{openai_request_tools, to_openai_messages};

    #[test]
    fn finish_filters_nameless_tool_calls() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut calls = vec![
            ToolCall {
                id: "a".into(),
                name: String::new(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "b".into(),
                name: "Bash".into(),
                arguments: "{}".into(),
            },
        ];
        finish(&tx, &mut calls);
        match rx.try_recv() {
            Ok(ProviderEvent::ToolCalls(calls)) => {
                assert_eq!(calls.len(), 1, "nameless calls should be filtered");
                assert_eq!(calls[0].name, "Bash");
            }
            other => panic!("remaining named call should come through: {other:?}"),
        }
        assert!(matches!(rx.try_recv(), Ok(ProviderEvent::Finished)));
    }

    #[test]
    fn anthropic_tool_result_with_images_becomes_blocks() {
        let messages = vec![ChatMsg::tool_result_with_images(
            "c1",
            "Read image x.png".into(),
            vec![ChatImage {
                media_type: "image/png".into(),
                data_base64: "QUJD".into(),
                label: Some("x.png".into()),
            }],
        )];
        let (_system, out) = to_anthropic_messages(&messages);
        let content = out[0]["content"].as_array().expect("user message blocks");
        assert_eq!(content[0]["type"], "tool_result");
        let blocks = content[0]["content"]
            .as_array()
            .expect("content is an array when images present");
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["type"], "base64");
        assert_eq!(blocks[0]["source"]["media_type"], "image/png");
        assert_eq!(blocks[0]["source"]["data"], "QUJD");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "Read image x.png");
    }

    #[test]
    fn anthropic_tool_result_without_images_stays_string() {
        // Regression: the imageless path matches the old behavior (content is a string)
        let messages = vec![ChatMsg::tool_result("c1", "ok".into())];
        let (_system, out) = to_anthropic_messages(&messages);
        let content = out[0]["content"].as_array().expect("blocks");
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(
            content[0]["content"], "ok",
            "content stays a string without images"
        );
    }

    #[test]
    fn openai_tool_result_with_images_splits_into_two_messages() {
        let messages = vec![ChatMsg::tool_result_with_images(
            "c1",
            "Read image x.png".into(),
            vec![ChatImage {
                media_type: "image/jpeg".into(),
                data_base64: "QUJD".into(),
                label: Some("x.png".into()),
            }],
        )];
        let out = to_openai_messages(&messages);
        assert_eq!(out.len(), 2, "splits into tool text + user image messages");
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "c1");
        assert_eq!(out[0]["content"], "Read image x.png");
        assert!(
            out[0].get("images").is_none(),
            "tool message carries no images"
        );
        assert_eq!(out[1]["role"], "user");
        let parts = out[1]["content"].as_array().expect("content parts");
        assert_eq!(parts[0]["type"], "text");
        assert!(parts[0]["text"].as_str().unwrap().contains("x.png"));
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/jpeg;base64,QUJD");
    }

    #[test]
    fn openai_no_image_matches_plain_serde() {
        // Regression: without images the custom construction is byte-identical to direct serde serialization
        let messages = vec![
            ChatMsg::system("s".into()),
            ChatMsg::user("u".into()),
            ChatMsg::tool_result("c1", "ok".into()),
        ];
        let built = to_openai_messages(&messages);
        let direct: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(built, direct);
    }

    #[test]
    fn user_message_with_images_both_formats() {
        let mut msg = ChatMsg::user("view image".into());
        msg.images = vec![ChatImage {
            media_type: "image/png".into(),
            data_base64: "QUJD".into(),
            label: Some("1.png".into()),
        }];
        // OpenAI: user content becomes parts (text first, image_url after)
        let out = to_openai_messages(&[msg.clone()]);
        assert_eq!(out.len(), 1, "user with images is not split");
        let parts = out[0]["content"].as_array().expect("parts");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "view image");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,QUJD");
        // Anthropic: user content becomes blocks (image first, text after)
        let (_s, out) = to_anthropic_messages(&[msg]);
        let blocks = out[0]["content"].as_array().expect("blocks");
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["source"]["data"], "QUJD");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "view image");
    }

    #[test]
    fn finish_all_nameless_emits_no_tool_calls() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut calls = vec![ToolCall::default()];
        finish(&tx, &mut calls);
        assert!(matches!(rx.try_recv(), Ok(ProviderEvent::Finished)));
        assert!(
            rx.try_recv().is_err(),
            "all-nameless calls should emit no ToolCalls"
        );
    }

    #[test]
    fn anthropic_messages_echo_thinking_first() {
        let messages = vec![
            ChatMsg::system("s".into()),
            ChatMsg::user("u".into()),
            ChatMsg::assistant(
                "answer".into(),
                vec![ToolCall {
                    id: "c1".into(),
                    name: "Bash".into(),
                    arguments: "{}".into(),
                }],
                Some("pondered".into()),
            ),
            ChatMsg::tool_result("c1", "ok".into()),
        ];
        let (_system, out) = to_anthropic_messages(&messages);
        let assistant = &out[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "thinking");
        assert_eq!(assistant["content"][0]["thinking"], "pondered");
        assert_eq!(assistant["content"][1]["type"], "text");
        assert_eq!(assistant["content"][2]["type"], "tool_use");
    }

    #[test]
    fn anthropic_messages_skip_empty_thinking() {
        let messages = vec![ChatMsg::assistant("answer".into(), vec![], None)];
        let (_system, out) = to_anthropic_messages(&messages);
        let content = out[0]["content"].as_array().expect("blocks");
        assert_eq!(
            content.len(),
            1,
            "no thinking block without thinking content"
        );
        assert_eq!(content[0]["type"], "text");
    }

    fn search_model(cap: bool, tool: Option<serde_json::Value>) -> ResolvedModel {
        ResolvedModel {
            base_url: "http://localhost".into(),
            api_key: String::new(),
            model: "m".into(),
            context_window: 0,
            max_output_tokens: 0,
            api_format: ApiFormat::AnthropicMessages,
            reasoning_params: None,
            cap_structured: false,
            cap_web_search: cap,
            web_search_tool: tool,
            input_image: false,
            provider_name: "p".into(),
        }
    }

    #[test]
    fn anthropic_web_search_tool_injection() {
        use crate::anthropic::anthropic_web_search_tool;
        // cap off -> no injection
        assert!(anthropic_web_search_tool(&search_model(false, None)).is_none());
        // cap on, no customization -> default web_search_20250305
        let tool = anthropic_web_search_tool(&search_model(true, None))
            .expect("cap enabled should inject");
        assert_eq!(
            tool,
            serde_json::json!({"type": "web_search_20250305", "name": "web_search"})
        );
        // cap on, with customization -> use the custom JSON
        let custom =
            serde_json::json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3});
        assert_eq!(
            anthropic_web_search_tool(&search_model(true, Some(custom.clone()))).unwrap(),
            custom
        );
    }

    #[test]
    fn openai_web_search_tool_injection() {
        use crate::openai::openai_web_search_tool;
        // cap off -> no injection
        assert!(openai_web_search_tool(&search_model(false, None)).is_none());
        // cap on but no customization -> no injection (OpenAI-compatible endpoints have no server-side search standard)
        assert!(openai_web_search_tool(&search_model(true, None)).is_none());
        // cap on with customization (e.g. Zhipu) -> inject
        let custom = serde_json::json!({
            "type": "web_search",
            "web_search": {"enable": true, "search_result": true}
        });
        assert_eq!(
            openai_web_search_tool(&search_model(true, Some(custom.clone()))).unwrap(),
            custom
        );
    }

    /// Request-level tool assembly (shared by streaming/non-streaming): the Anthropic shape must be
    /// name/description/input_schema -- leftover OpenAI wire shape gets a 422 from compatible endpoints
    #[test]
    fn anthropic_request_tools_converts_openai_wire_shape() {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "Read", "description": "read files", "parameters": {"type": "object"}}
        })];
        let out = anthropic_request_tools(&search_model(false, None), &tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["name"], "Read");
        assert_eq!(out[0]["description"], "read files");
        assert_eq!(out[0]["input_schema"]["type"], "object");
        assert!(
            out[0].get("function").is_none(),
            "OpenAI wire shape must not remain: {out:?}"
        );
        assert!(
            out[0].get("type").is_none(),
            "custom tool carries no type: {out:?}"
        );
        // cap on -> append the server-side search tool (same assembly as the streaming path)
        let out = anthropic_request_tools(&search_model(true, None), &tools);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["type"], "web_search_20250305");
    }

    /// OpenAI side: pass the wire shape through verbatim; append the server-side search tool only with cap+customization
    #[test]
    fn openai_request_tools_keeps_wire_shape() {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "Read", "description": "d", "parameters": {}}
        })];
        let out = openai_request_tools(&search_model(false, None), &tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "function");
        let custom = serde_json::json!({"type": "web_search", "web_search": {"enable": true}});
        let out = openai_request_tools(&search_model(true, Some(custom)), &tools);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["type"], "web_search");
    }
}
