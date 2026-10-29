//! Pig Code's model API layer: the wire-protocol crate every model call goes
//! through. Three API formats (OpenAI Chat Completions / OpenAI Responses /
//! Anthropic Messages), spec-level SSE decoding, Stainless-style SDK identity
//! headers, transport retry with visibility events ([ProviderEvent::Retrying] +
//! [CallControl::retry_now]), one-shot sidecar calls, explicit Anthropic prompt
//! caching, session prompt-cache routing, hosted-search native replay, PDF
//! document projection, opt-in wire logging (PIG_LOG_API), and a mock provider
//! for tests and the GUI self-test.
//!
//! English-only and UI-free (Clean/Hexagonal, same rules as pig-core): errors
//! are carried by pig-protocol's `CoreError` + English `detail`, with
//! localization deferred to pig-app render points. Nothing here may import
//! gpui or an i18n registry. Transient events ([ProviderEvent::Retrying]) are
//! for live rendering only and are never persisted.

pub mod api_log;
pub mod mock;

mod anthropic;
mod chat;
mod chat_tool_stream;
mod context_window;
mod endpoint;
mod events;
mod identity;
mod metadata;
mod openai;
mod responses;
mod retry;
mod sidecar;
mod sse;
mod stream;
mod strict_tools;
mod tool_settings;
mod tool_stream;
mod transport;

pub use anthropic::anthropic_web_search_tool;
pub use chat::{
    AssistantReplay, AssistantReplayItem, ChatDocument, ChatImage, ChatMsg, DocumentTokenEstimate,
    FunctionWire, ReplayOrigin, ResolvedModel, STOPPED_TOOL_OUTPUT, TextCitation, ToolCall,
    ToolCallWire, replay_sources,
};
pub use endpoint::{
    anthropic_cache_supported, chat_token_limit_key, is_official_anthropic, is_official_openai,
    native_search_replay_supported, native_web_search_enabled, prompt_cache_key_supported,
    session_prompt_cache_key, subagent_prompt_cache_key,
};
pub use events::{CallControl, ProviderEvent, RetryNotice, RetryReason};
pub use metadata::{ProviderResponseMetadata, ProviderUsage};
pub use responses::responses_web_search_tool;
pub use sidecar::{
    CompleteCall, CompleteCallError, StructuredOutput, complete_messages, complete_text,
    test_provider,
};
pub use stream::stream_chat;
pub use tool_settings::ToolCallSettings;

/// CoreError -> single-line English text (for the model channel: subagent
/// failure receipts, call trace persistence). Multilingual UI goes through
/// pig-app's kind mapping; core only produces English fallback primary-language text;
/// detail is always the upstream English original, passed through verbatim.
pub fn core_error_en(error: &pig_protocol::CoreError) -> String {
    match error {
        pig_protocol::CoreError::Network { detail } => format!("Network error: {detail}"),
        pig_protocol::CoreError::StreamRead { detail } => {
            format!("Failed to read stream: {detail}")
        }
        pig_protocol::CoreError::ContextWindowExceeded { detail } => {
            format!("Context window exceeded: {detail}")
        }
        pig_protocol::CoreError::Internal { detail } => detail.clone(),
        pig_protocol::CoreError::Unknown => "Unknown error".to_string(),
        // Other variants never go through the model channel (UI-only); fall back to the Debug shape for diagnosability
        other => format!("{other:?}"),
    }
}
