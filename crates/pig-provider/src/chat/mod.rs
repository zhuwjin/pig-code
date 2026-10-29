use pig_protocol::ApiFormat;

use crate::tool_settings::ToolCallSettings;

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
    /// Endpoint honors strict function-calling semantics (see strict_tools)
    pub cap_strict_tools: bool,
    pub web_search_tool: Option<serde_json::Value>,
    /// Model supports image input (gate for ReadMediaFile)
    pub input_image: bool,
    /// Model supports PDF document input (gate for ReadMediaFile)
    pub input_pdf: bool,
    /// Endpoint accepts explicit Anthropic `cache_control` breakpoints; core
    /// resolves this from [`crate::endpoint::anthropic_cache_supported`]
    pub anthropic_cache_control: bool,
    /// Replay provider-native hosted-search items on the wire. Core resolves
    /// this from [`crate::endpoint::native_search_replay_supported`]; the
    /// durable replay always keeps the native items so the UI can derive
    /// sources even when only text is replayed.
    pub replay_native_search: bool,
    /// Stable session-routing key for OpenAI automatic prompt caching. Core
    /// injects `pig:{session_id}` (subagents `pig:{session_id}:{agent_id}`);
    /// title/connectivity one-shots leave it empty.
    pub prompt_cache_key: Option<String>,
    /// For display
    pub provider_name: String,
    /// Parallel tool-call compatibility override forwarded to every protocol
    /// encoder; defaults to parallel.
    pub tool_settings: ToolCallSettings,
}

mod documents;
mod messages;
mod replay;
mod sources;

#[cfg(test)]
mod tests;

pub(crate) use documents::validate_documents;
pub use documents::{ChatDocument, DocumentTokenEstimate};
pub use messages::{ChatImage, ChatMsg, FunctionWire, STOPPED_TOOL_OUTPUT, ToolCall, ToolCallWire};
pub(crate) use messages::{finish, tool_result_output};
pub use replay::{AssistantReplay, AssistantReplayItem, ReplayOrigin, TextCitation};
pub use sources::replay_sources;

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
pub(crate) fn test_model(api_format: ApiFormat) -> ResolvedModel {
    ResolvedModel {
        base_url: "http://localhost".into(),
        api_key: String::new(),
        model: "test-model".into(),
        context_window: 128_000,
        max_output_tokens: 4096,
        api_format,
        reasoning_params: None,
        cap_web_search: false,
        cap_structured: false,
        cap_strict_tools: false,
        web_search_tool: None,
        input_image: true,
        input_pdf: true,
        anthropic_cache_control: false,
        replay_native_search: false,
        prompt_cache_key: None,
        provider_name: "test".into(),
        tool_settings: Default::default(),
    }
}
