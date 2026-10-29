use futures_util::StreamExt as _;

use pig_protocol::{CoreError, FinishReason};

use crate::chat::{
    AssistantReplay, AssistantReplayItem, ChatDocument, ChatMsg, ResolvedModel, TextCitation,
    merge_reasoning_params,
};
use crate::events::{CallControl, CallError, ProviderEvent};
use crate::identity::apply_sdk_headers;
use crate::retry::{SendOutcome, http_client, send_with_retry};

fn is_official_anthropic(base_url: &str) -> bool {
    crate::endpoint::is_official_anthropic(base_url)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClaudeStrictSupport {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClaudeCapabilities {
    strict: ClaudeStrictSupport,
}

fn version_token(token: &str) -> Option<u32> {
    if token.is_empty() || token.len() > 2 || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    token.parse().ok()
}

fn claude_capabilities(model_id: &str) -> ClaudeCapabilities {
    let normalized = model_id
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| match character {
            '.' | '/' | '@' | ':' | '_' => '-',
            other => other,
        })
        .collect::<String>();
    let unknown = ClaudeCapabilities {
        strict: ClaudeStrictSupport::Unknown,
    };
    let Some(index) = normalized.rfind("claude-") else {
        return unknown;
    };
    if index > 0 && normalized.as_bytes()[index - 1] != b'-' {
        return unknown;
    }
    let tokens = normalized[index + "claude-".len()..]
        .split('-')
        .collect::<Vec<_>>();
    let family = tokens
        .first()
        .copied()
        .filter(|token| matches!(*token, "sonnet" | "opus" | "haiku" | "fable"));
    let (major, minor) = if family.is_some() {
        (
            tokens.get(1).and_then(|token| version_token(token)),
            tokens.get(2).and_then(|token| version_token(token)),
        )
    } else {
        (
            tokens.first().and_then(|token| version_token(token)),
            tokens.get(1).and_then(|token| version_token(token)),
        )
    };
    let legacy = family.is_none()
        && (matches!(major, Some(2 | 3)) || matches!(tokens.first(), Some(&"instant" | &"v2")));
    let strict = if legacy {
        ClaudeStrictSupport::Unsupported
    } else {
        match (family, major, minor) {
            (Some(_), Some(3), _) => ClaudeStrictSupport::Unsupported,
            (Some("opus"), Some(4), Some(1)) => ClaudeStrictSupport::Supported,
            (Some("sonnet" | "opus"), Some(4), minor) if !matches!(minor, Some(value) if value >= 5) => {
                ClaudeStrictSupport::Unsupported
            }
            (Some("sonnet" | "opus" | "haiku"), Some(4), Some(value)) if value >= 5 => {
                ClaudeStrictSupport::Supported
            }
            (Some("sonnet" | "opus" | "haiku" | "fable"), Some(5), _) => {
                ClaudeStrictSupport::Supported
            }
            _ => ClaudeStrictSupport::Unknown,
        }
    };
    ClaudeCapabilities { strict }
}

/// Internal ChatMsg list -> Anthropic messages array.
/// system is hoisted to the top level; assistant tool_calls -> tool_use block; tool results merge into a user message's tool_result block.
pub(crate) fn to_anthropic_messages(
    config: &ResolvedModel,
    messages: &[ChatMsg],
) -> (String, Vec<serde_json::Value>) {
    let mut system = String::new();
    let mut out: Vec<serde_json::Value> = vec![];

    for msg in messages {
        match msg.role.as_str() {
            "system" => {
                if let Some(content) = &msg.content {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(content);
                }
            }
            "user" => {
                if msg.images.is_empty() && msg.documents.is_empty() {
                    out.push(serde_json::json!({
                        "role": "user",
                        "content": msg.content.clone().unwrap_or_default(),
                    }));
                } else {
                    // User message with media (pasted image / attached PDF):
                    // media blocks first, text after. A labeled image gets a
                    // "[label]" text marker so the model can correlate text
                    // references with attachments.
                    let mut blocks: Vec<serde_json::Value> = msg
                        .images
                        .iter()
                        .flat_map(|img| {
                            let marker = img.label.as_ref().map(|label| {
                                serde_json::json!({
                                    "type": "text",
                                    "text": format!("[{label}]"),
                                })
                            });
                            marker.into_iter().chain([serde_json::json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": img.media_type,
                                    "data": img.data_base64,
                                },
                            })])
                        })
                        .collect();
                    for document in &msg.documents {
                        if let Some(label) = &document.label {
                            blocks.push(serde_json::json!({
                                "type": "text",
                                "text": format!("[{label}]"),
                            }));
                        }
                        blocks.push(anthropic_document_block(document));
                    }
                    // Media-only user turns: an empty text block is rejected by
                    // the Anthropic wire, and the media already carries the turn.
                    if let Some(text) = msg.content.as_deref().filter(|text| !text.is_empty()) {
                        blocks.push(serde_json::json!({"type": "text", "text": text}));
                    }
                    out.push(serde_json::json!({"role": "user", "content": blocks}));
                }
            }
            "assistant" => {
                let mut blocks: Vec<serde_json::Value> = vec![];
                if let Some(replay) = &msg.replay {
                    let same_origin = replay.origin_matches(config);
                    let replay_native =
                        same_origin && config.replay_native_search && config.cap_web_search;
                    let require_encrypted = is_official_anthropic(&config.base_url);
                    // A server_tool_use is replayed only when its result block
                    // is replayable; a missing credential must not leave an
                    // orphaned server tool call on the wire.
                    let replayable_server_ids: std::collections::HashSet<&str> = replay
                        .items
                        .iter()
                        .filter_map(|item| match item {
                            AssistantReplayItem::AnthropicWebSearchResult {
                                tool_use_id,
                                content,
                            } if replay_native
                                && (!require_encrypted
                                    || web_search_result_has_credentials(content)) =>
                            {
                                Some(tool_use_id.as_str())
                            }
                            _ => None,
                        })
                        .collect();
                    for item in &replay.items {
                        match item {
                            // An empty text block is invalid on the Anthropic
                            // wire; citations on empty text are dropped with it.
                            AssistantReplayItem::Text { text, citations } if !text.is_empty() => {
                                let mut block = serde_json::json!({"type": "text", "text": text});
                                if replay_native && !citations.is_empty() {
                                    let values: Vec<serde_json::Value> = citations
                                        .iter()
                                        .filter_map(|citation| {
                                            anthropic_citation_value(citation, require_encrypted)
                                        })
                                        .collect();
                                    if !values.is_empty() {
                                        block["citations"] = serde_json::Value::Array(values);
                                    }
                                }
                                blocks.push(block);
                            }
                            AssistantReplayItem::ToolCall { call } => {
                                let input = serde_json::from_str(&call.function.arguments)
                                    .unwrap_or(serde_json::Value::Object(Default::default()));
                                blocks.push(serde_json::json!({
                                    "type": "tool_use",
                                    "id": call.id,
                                    "name": call.function.name,
                                    "input": input,
                                }));
                            }
                            AssistantReplayItem::AnthropicThinking {
                                thinking,
                                signature,
                            } if same_origin
                                && !thinking.is_empty()
                                && (signature.is_some()
                                    || !is_official_anthropic(&config.base_url)) =>
                            {
                                let mut block = serde_json::json!({
                                    "type": "thinking",
                                    "thinking": thinking,
                                });
                                if let Some(signature) = signature {
                                    block["signature"] =
                                        serde_json::Value::String(signature.clone());
                                }
                                blocks.push(block);
                            }
                            AssistantReplayItem::AnthropicRedactedThinking { data }
                                if same_origin && !data.is_empty() =>
                            {
                                blocks.push(serde_json::json!({
                                    "type": "redacted_thinking",
                                    "data": data,
                                }));
                            }
                            AssistantReplayItem::AnthropicServerToolUse { id, name, input }
                                if replay_native && replayable_server_ids.contains(id.as_str()) =>
                            {
                                blocks.push(serde_json::json!({
                                    "type": "server_tool_use",
                                    "id": id,
                                    "name": name,
                                    "input": input,
                                }));
                            }
                            AssistantReplayItem::AnthropicWebSearchResult {
                                tool_use_id,
                                content,
                            } if replay_native => {
                                if require_encrypted && !web_search_result_has_credentials(content)
                                {
                                    // Official replay without the opaque index
                                    // is invalid: drop this block, keep the text.
                                    continue;
                                }
                                blocks.push(serde_json::json!({
                                    "type": "web_search_tool_result",
                                    "tool_use_id": tool_use_id,
                                    "content": content,
                                }));
                            }
                            _ => {}
                        }
                    }
                } else {
                    // Compatibility endpoints that return no signature still require plaintext thinking.
                    if let Some(reasoning) = &msg.reasoning
                        && !reasoning.is_empty()
                        && !is_official_anthropic(&config.base_url)
                    {
                        blocks.push(serde_json::json!({"type": "thinking", "thinking": reasoning}));
                    }
                    if let Some(content) = &msg.content
                        && !content.is_empty()
                    {
                        blocks.push(serde_json::json!({"type": "text", "text": content}));
                    }
                    if let Some(calls) = &msg.tool_calls {
                        for call in calls {
                            let input = serde_json::from_str(&call.function.arguments)
                                .unwrap_or(serde_json::Value::Object(Default::default()));
                            blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": call.id,
                                "name": call.function.name,
                                "input": input,
                            }));
                        }
                    }
                }
                // Anthropic rejects an assistant message with no content
                // blocks everywhere except a final prefill. Never emit one:
                // an unreplayable pause/thinking-only/cross-origin step is
                // skipped deterministically in live, restart, fork and compact.
                if blocks.is_empty() {
                    continue;
                }
                out.push(serde_json::json!({"role": "assistant", "content": blocks}));
            }
            "tool" => {
                // Tool output stays a plain string when it has no media
                // (regression-safe). With media, content becomes a block array:
                // images, then this result's own documents, then the text
                // summary. Documents MUST live inside the owning
                // tool_result.content — sibling blocks between results are
                // invalid when two parallel calls return PDFs.
                let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
                let output = crate::chat::tool_result_output(msg);
                let block = if msg.images.is_empty() && msg.documents.is_empty() {
                    serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": output,
                    })
                } else {
                    let mut blocks: Vec<serde_json::Value> = msg
                        .images
                        .iter()
                        .map(|img| {
                            serde_json::json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": img.media_type,
                                    "data": img.data_base64,
                                },
                            })
                        })
                        .collect();
                    blocks.extend(msg.documents.iter().map(anthropic_document_block));
                    // An empty text block is invalid on the Anthropic wire; a
                    // media-only tool result carries its content in the media.
                    if !output.is_empty() {
                        blocks.push(serde_json::json!({"type": "text", "text": output}));
                    }
                    serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": blocks,
                    })
                };
                // Execution status rides the tool_result block itself (the
                // documented Anthropic field). Success omits it; failures and
                // stopped/cancelled calls send `is_error: true`.
                let block = if msg.is_error {
                    let mut block = block;
                    block["is_error"] = serde_json::Value::Bool(true);
                    block
                } else {
                    block
                };
                // Consecutive tool results merge into the same user message;
                // each block already owns its documents.
                if let Some(last) = out.last_mut()
                    && last["role"] == "user"
                    && last["content"].is_array()
                {
                    last["content"].as_array_mut().expect("array").push(block);
                } else {
                    out.push(serde_json::json!({"role": "user", "content": [block]}));
                }
            }
            _ => {}
        }
    }
    (system, out)
}

/// Anthropic document block: bare base64 (not a data URL) plus the file name.
fn anthropic_document_block(document: &ChatDocument) -> serde_json::Value {
    serde_json::json!({
        "type": "document",
        "source": {
            "type": "base64",
            "media_type": document.media_type,
            "data": document.data_base64,
        },
        "title": document.filename,
    })
}

fn anthropic_citation_value(
    citation: &TextCitation,
    require_encrypted: bool,
) -> Option<serde_json::Value> {
    if require_encrypted && citation.encrypted_index.is_none() {
        return None;
    }
    let mut value = serde_json::json!({
        "type": "web_search_result_location",
        "url": citation.url,
        "title": citation.title,
    });
    if let Some(encrypted) = &citation.encrypted_index {
        value["encrypted_index"] = serde_json::Value::String(encrypted.clone());
    }
    if let Some(text) = &citation.cited_text {
        value["cited_text"] = serde_json::Value::String(text.clone());
    }
    Some(value)
}

/// One Anthropic `citations_delta.citation` -> typed citation.
fn anthropic_citation(value: &serde_json::Value) -> Option<TextCitation> {
    let url = value["url"].as_str()?.trim();
    if url.is_empty() {
        return None;
    }
    Some(TextCitation {
        url: url.to_string(),
        title: value["title"].as_str().unwrap_or_default().to_string(),
        snippet: value["cited_text"].as_str().map(str::to_string),
        start_index: None,
        end_index: None,
        encrypted_index: value["encrypted_index"]
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        cited_text: value["cited_text"].as_str().map(str::to_string),
    })
}

fn push_unique_citation(citations: &mut Vec<TextCitation>, citation: TextCitation) {
    if citations.iter().any(|existing| {
        existing.url == citation.url
            && existing.start_index == citation.start_index
            && existing.end_index == citation.end_index
            && existing.encrypted_index == citation.encrypted_index
    }) {
        return;
    }
    citations.push(citation);
}

/// Official replay requires every `web_search_result` to carry the opaque
/// `encrypted_content` credential; an error object is always replayable.
fn web_search_result_has_credentials(content: &serde_json::Value) -> bool {
    let Some(results) = content.as_array() else {
        return true;
    };
    results.iter().all(|result| {
        result["type"] != "web_search_result"
            || result["encrypted_content"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
    })
}

pub(crate) fn to_anthropic_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            let function = &tool["function"];
            serde_json::json!({
                "name": function["name"],
                "description": function["description"],
                "input_schema": function["parameters"],
            })
        })
        .collect()
}

/// Request-level tool list assembly (shared by streaming/non-streaming, to prevent drift between the two):
/// OpenAI wire shape -> Anthropic shape + the server-side search tool when the capability is on.
/// Strict support is an explicit endpoint opt-in, subject to model vetoes.
pub(crate) fn anthropic_request_tools(
    config: &ResolvedModel,
    tools: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let capabilities = claude_capabilities(&config.model);
    let support = if !config.cap_strict_tools {
        crate::tool_settings::StrictSupport::Disabled
    } else {
        match capabilities.strict {
            ClaudeStrictSupport::Supported | ClaudeStrictSupport::Unknown => {
                crate::tool_settings::StrictSupport::Supported
            }
            ClaudeStrictSupport::Unsupported => {
                crate::tool_settings::StrictSupport::UnsupportedModel
            }
        }
    };
    let mut out = to_anthropic_tools(tools);
    for (tool, source) in out.iter_mut().zip(tools.iter()) {
        let strict_schema = crate::tool_settings::strict_schema(
            support,
            crate::tool_settings::declared_strict(source),
            &tool["input_schema"],
            tool["name"].as_str().unwrap_or_default(),
            &config.model,
        );
        if let Some(strict_schema) = strict_schema {
            tool["input_schema"] = strict_schema;
            tool["strict"] = serde_json::Value::Bool(true);
        }
    }
    if let Some(tool) = anthropic_web_search_tool(config) {
        out.push(tool);
    }
    out
}

/// Anthropic endpoint: inject the server-side search tool when the capability is on (web_search_tool can be customized,
/// defaulting to web_search_20250305). The server-produced web_search_tool_result block is
/// ignored by the SSE parser's fallthrough.
pub fn anthropic_web_search_tool(config: &ResolvedModel) -> Option<serde_json::Value> {
    if !config.cap_web_search {
        return None;
    }
    Some(config.web_search_tool.clone().unwrap_or_else(
        || serde_json::json!({"type": "web_search_20250305", "name": "web_search"}),
    ))
}

/// Where the explicit Anthropic cache breakpoints go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CachePolicy {
    Off,
    /// Normal request: tools + system + the latest message's cacheable tail.
    Main,
    /// Compaction sidecar: tools + system + the last original message before
    /// the appended instruction (never the instruction itself), so the
    /// previous turn's live tail breakpoint is preserved.
    Sidecar,
}

fn ephemeral() -> serde_json::Value {
    serde_json::json!({"type": "ephemeral"})
}

/// Pure Anthropic request body builder shared by streaming and sidecars.
pub(crate) fn build_anthropic_body(
    config: &ResolvedModel,
    messages: &[ChatMsg],
    tools: &[serde_json::Value],
    stream: bool,
    cache: CachePolicy,
) -> Result<serde_json::Value, CoreError> {
    crate::chat::validate_documents(config, messages)?;
    let (system, messages) = to_anthropic_messages(config, messages);
    let tools = anthropic_request_tools(config, tools);
    let mut body = serde_json::json!({
        "model": config.model,
        "max_tokens": config.max_output_tokens,
        "stream": stream,
        "messages": messages,
    });
    if !system.is_empty() {
        body["system"] = serde_json::Value::String(system);
    }
    let parallel_choice =
        crate::tool_settings::anthropic_parallel_choice(&config.tool_settings, &tools);
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools);
    }
    if let Some(choice) = parallel_choice {
        body["tool_choice"] = choice;
    }
    merge_reasoning_params(&mut body, config);
    drop_impossible_sidecar_thinking(&mut body, stream);
    let cache = if config.anthropic_cache_control {
        cache
    } else {
        CachePolicy::Off
    };
    apply_anthropic_cache_control(&mut body, cache);
    Ok(body)
}

/// A summary sidecar often runs with a small output cap while the user's
/// thinking budget can be larger. Anthropic rejects `max_tokens <= budget`; in
/// that case drop thinking for this one-shot request only (the main request is
/// untouched), because thinking parameters are not part of the cached message
/// prefix.
fn drop_impossible_sidecar_thinking(body: &mut serde_json::Value, stream: bool) {
    if stream {
        return;
    }
    let Some(budget) = body
        .pointer("/thinking/budget_tokens")
        .and_then(serde_json::Value::as_u64)
    else {
        return;
    };
    let max_tokens = body
        .get("max_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if budget >= max_tokens {
        tracing::warn!(
            budget_tokens = budget,
            max_tokens,
            "dropping thinking from the summary sidecar: max_tokens must exceed budget_tokens"
        );
        if let Some(object) = body.as_object_mut() {
            object.remove("thinking");
        }
    }
}

/// At most three 5-minute `{"type":"ephemeral"}` breakpoints: the last custom
/// tool, the last system block, and one cacheable block in the selected
/// message. Hosted tools and thinking blocks are never marked; no TTL is sent.
pub(crate) fn apply_anthropic_cache_control(body: &mut serde_json::Value, policy: CachePolicy) {
    if policy == CachePolicy::Off {
        return;
    }
    // The provider owns the breakpoint layout: clear any inherited marker so a
    // request never carries more than the three below.
    strip_cache_control(body);
    // 1. The last custom (untyped) tool. A hosted web_search definition is
    // small and toggle-sensitive, so it never carries the breakpoint.
    if let Some(tools) = body["tools"].as_array_mut()
        && let Some(tool) = tools
            .iter_mut()
            .rev()
            .find(|tool| tool.get("type").is_none())
    {
        tool["cache_control"] = ephemeral();
    }
    // 2. The last system block. Convert the legacy string form so the live and
    // sidecar requests carry the same bytes.
    match body.get_mut("system") {
        Some(serde_json::Value::String(text)) => {
            let text = std::mem::take(text);
            if !text.is_empty() {
                body["system"] = serde_json::json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": ephemeral(),
                }]);
            }
        }
        Some(serde_json::Value::Array(blocks)) => {
            if let Some(block) = blocks.last_mut() {
                block["cache_control"] = ephemeral();
            }
        }
        _ => {}
    }
    // 3. The selected message's last cacheable block.
    let Some(messages) = body["messages"].as_array_mut() else {
        return;
    };
    let target = match policy {
        CachePolicy::Main => messages.len().checked_sub(1),
        // The compaction instruction is the final user message; mark the last
        // original message instead.
        CachePolicy::Sidecar => messages.len().checked_sub(2),
        CachePolicy::Off => None,
    };
    if let Some(message) = target.and_then(|index| messages.get_mut(index)) {
        mark_anthropic_message_tail(message);
    }
}

/// Remove only the protocol cache anchors, never tool schemas or tool inputs:
/// a schema property or a tool_use argument may legitimately be named
/// `cache_control`, and rewriting it would change the model-visible context.
fn strip_cache_control(body: &mut serde_json::Value) {
    if let Some(tools) = body
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    {
        for tool in tools {
            if let Some(object) = tool.as_object_mut() {
                object.remove("cache_control");
            }
        }
    }
    if let Some(blocks) = body
        .get_mut("system")
        .and_then(serde_json::Value::as_array_mut)
    {
        for block in blocks {
            if let Some(object) = block.as_object_mut() {
                object.remove("cache_control");
            }
        }
    }
    if let Some(messages) = body
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    {
        for message in messages {
            if let Some(blocks) = message
                .get_mut("content")
                .and_then(serde_json::Value::as_array_mut)
            {
                for block in blocks {
                    if let Some(object) = block.as_object_mut() {
                        object.remove("cache_control");
                    }
                }
            }
        }
    }
}

fn mark_anthropic_message_tail(message: &mut serde_json::Value) {
    match message.get_mut("content") {
        Some(serde_json::Value::String(text)) => {
            let text = std::mem::take(text);
            if !text.is_empty() {
                message["content"] = serde_json::json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": ephemeral(),
                }]);
            }
        }
        Some(serde_json::Value::Array(blocks)) => {
            if let Some(block) = blocks.iter_mut().rev().find(|block| {
                !matches!(
                    block["type"].as_str(),
                    Some(
                        "thinking"
                            | "redacted_thinking"
                            | "server_tool_use"
                            | "web_search_tool_result"
                    )
                )
            }) {
                block["cache_control"] = ephemeral();
            }
        }
        _ => {}
    }
}

pub(crate) async fn stream_anthropic(
    config: &ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    control: &CallControl,
) -> Result<(), CallError> {
    let pdf = messages.iter().any(|message| !message.documents.is_empty());
    let body = build_anthropic_body(config, &messages, &tools, true, CachePolicy::Main)
        .map_err(CallError::from)?;

    let url = crate::endpoint::anthropic_messages_url(&config.base_url);
    // The interleaved-thinking beta header rides along when reasoning params enabled
    // thinking for this request (same condition the SDK's client would send it under;
    // an explicit thinking.type=disabled must not trigger it)
    let thinking =
        body.get("thinking").and_then(|t| t.get("type")) == Some(&serde_json::json!("enabled"));
    // API wire log (PIG_LOG_API): no-op accumulator when disabled
    let mut api =
        crate::api_log::ApiCall::new("anthropic.messages", &config.provider_name, &config.model);
    api.request(&url, &body);

    let client = http_client();
    let response = match send_with_retry(
        |retry| {
            apply_sdk_headers(
                client
                    .post(&url)
                    .header("x-api-key", &config.api_key)
                    .header("anthropic-version", "2023-06-01"),
                true,
                thinking,
                pdf,
                retry,
            )
            .json(&body)
        },
        control,
        tx,
    )
    .await?
    {
        SendOutcome::Response(response) => response,
        SendOutcome::Cancelled => {
            api.finish("cancelled", &[]);
            return Ok(());
        }
    };

    let mut response_metadata = crate::ProviderResponseMetadata::from_response(&response);
    let _ = tx.send(ProviderEvent::ResponseMetadata(response_metadata.clone()));
    let status = response.status();
    let idle = control.idle_timeout();
    if !status.is_success() {
        // The error body is a response body like any other: bound it with the
        // same per-chunk idle deadline and let cancellation win.
        let detail =
            match crate::transport::read_body_with_idle(response, control.cancel_token(), idle)
                .await
            {
                Ok(crate::transport::BodyRead::Text(text)) => text,
                Ok(crate::transport::BodyRead::Cancelled) => {
                    api.finish("cancelled", &[]);
                    return Ok(());
                }
                Err(failure) => failure.partial,
            };
        let detail: String = detail.chars().take(500).collect();
        let error = crate::context_window::http_error(status, &detail);
        api.fail(&crate::core_error_en(&error));
        return Err(error.into());
    }

    let mut byte_stream = response.bytes_stream();
    // Spec-level SSE decoding (three line terminators, deferred trailing CR,
    // multi-line data, comments, incremental UTF-8) — same semantics as the
    // eventsource-parser package under the Vercel AI SDK
    let mut sse = crate::sse::SseDecoder::new();
    let mut tool_stream = crate::tool_stream::ToolCallStream::new();
    let mut replay_blocks = AnthropicReplayBlocks::default();
    let mut usage = StreamUsage::default();
    let mut finish_reason = None;

    loop {
        let chunk = tokio::select! {
            chunk = tokio::time::timeout(idle, byte_stream.next()) => match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    let detail = crate::transport::idle_detail("Stream", idle);
                    api.fail(&detail);
                    return Err(CoreError::StreamRead { detail }.into());
                }
            },
            _ = control.cancelled() => {
                api.finish("cancelled", tool_stream.calls());
                return Ok(());
            }
        };
        let Some(chunk) = chunk else { break };
        let bytes = chunk.map_err(|e| {
            CallError::from(CoreError::StreamRead {
                detail: e.to_string(),
            })
        })?;

        for event in sse.push(&bytes) {
            // Raw wire log: the payload verbatim, plus the event: line when
            // present (dispatch below keys off the payload's own type, same
            // as the SDK — the event: field is redundant in Anthropic frames)
            if event.event.is_empty() {
                api.raw_line(&event.data);
            } else {
                api.raw_line(&format!("event: {}\ndata: {}", event.event, event.data));
            }
            let json = match serde_json::from_str::<serde_json::Value>(event.data.trim()) {
                Ok(json) => json,
                Err(error) => {
                    let detail = format!("Invalid JSON in Anthropic stream: {error}");
                    api.fail(&detail);
                    return Err(CoreError::Internal { detail }.into());
                }
            };
            match json["type"].as_str().unwrap_or("") {
                "message_start" => {
                    if response_metadata.response_id.is_none()
                        && let Some(id) = json["message"]["id"].as_str()
                        && !id.is_empty()
                    {
                        response_metadata.response_id = Some(id.to_string());
                        let _ = tx.send(ProviderEvent::ResponseMetadata(response_metadata.clone()));
                    }
                    usage.merge(&json["message"]["usage"]);
                }
                "content_block_start" => {
                    let index = json["index"].as_u64().unwrap_or(0) as usize;
                    replay_blocks.start(index, &json["content_block"], &mut tool_stream, tx);
                }
                "content_block_delta" => {
                    let index = json["index"].as_u64().unwrap_or(0) as usize;
                    let delta = &json["delta"];
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(text) = delta["text"].as_str()
                                && !text.is_empty()
                            {
                                replay_blocks.text(index, text);
                                api.text(text);
                                let _ = tx.send(ProviderEvent::Text(text.to_string()));
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(thinking) = delta["thinking"].as_str()
                                && !thinking.is_empty()
                            {
                                replay_blocks.thinking(index, thinking);
                                api.reasoning(thinking);
                                let _ = tx.send(ProviderEvent::Reasoning(thinking.to_string()));
                            }
                        }
                        Some("signature_delta") => {
                            if let Some(signature) = delta["signature"].as_str() {
                                replay_blocks.signature(index, signature);
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(partial) = delta["partial_json"].as_str()
                                && !replay_blocks.server_delta(index, partial)
                            {
                                tool_stream.push(index, None, None, Some(partial), tx);
                            }
                        }
                        Some("citations_delta") => {
                            replay_blocks.citation(index, &delta["citation"]);
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    usage.merge(&json["usage"]);
                    if let Some(raw_reason) = json["delta"]["stop_reason"].as_str() {
                        let reason = match raw_reason {
                            "end_turn" | "stop_sequence" => FinishReason::Stop,
                            "pause_turn" => FinishReason::Pause,
                            "tool_use" => FinishReason::ToolCalls,
                            "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
                            "refusal" => FinishReason::ContentFilter,
                            other => {
                                // Strict rejection: streamed text stays
                                // visible, but an unknown label never becomes
                                // a healthy stop and pending tool calls never
                                // reach execution.
                                let detail = format!("Unknown Anthropic stop reason: {other}");
                                api.fail(&detail);
                                return Err(CoreError::Internal { detail }.into());
                            }
                        };
                        let thinking = json["usage"]["output_tokens_details"]["thinking_tokens"]
                            .as_u64()
                            .unwrap_or(0);
                        if let Some(normalized) = usage.normalized(thinking, config.context_window)
                        {
                            api.usage(
                                normalized.input,
                                normalized.cache_read,
                                normalized.output,
                                normalized.used,
                                normalized.total,
                                normalized.reasoning_output,
                            );
                            let _ = tx.send(ProviderEvent::Usage {
                                input: normalized.input,
                                cache_read: normalized.cache_read,
                                output: normalized.output,
                                used: normalized.used,
                                total: normalized.total,
                                reasoning_output: normalized.reasoning_output,
                            });
                        }
                        finish_reason = Some(reason);
                    }
                }
                "error" => {
                    // In-band 200 error. The upstream error body text goes
                    // into detail verbatim (without a message field, fall back
                    // semantically to Unknown). Retryability follows the SDK's
                    // getAnthropicStreamErrorMetadata mapping (overloaded→529,
                    // rate_limit→429, api_error→500, all retryable); the silent
                    // retry itself is gated on "no content forwarded yet" in
                    // stream_chat
                    let error_type = json["error"]["type"].as_str().unwrap_or("");
                    let retryable = matches!(
                        error_type,
                        "overloaded_error" | "rate_limit_error" | "api_error"
                    );
                    let message = json["error"]["message"].as_str().map(str::to_string);
                    let detail = match message {
                        Some(message) => crate::context_window::detail_with_code(
                            "Anthropic error: ",
                            &message,
                            Some(error_type),
                        ),
                        None => "Anthropic error: unknown".to_string(),
                    };
                    let error = crate::context_window::stream_error(detail);
                    api.fail(&crate::core_error_en(&error));
                    return Err(CallError { error, retryable });
                }
                _ => {}
            }
            if finish_reason.is_some() {
                if control.is_cancelled() {
                    api.finish("cancelled", tool_stream.calls());
                    return Ok(());
                }
                return finish_anthropic_stream(
                    tool_stream,
                    replay_blocks,
                    finish_reason,
                    config,
                    &mut api,
                    tx,
                );
            }
        }
    }
    // Cancellation that raced the final chunk still must not report success.
    if control.is_cancelled() {
        api.finish("cancelled", tool_stream.calls());
        return Ok(());
    }
    finish_anthropic_stream(
        tool_stream,
        replay_blocks,
        finish_reason,
        config,
        &mut api,
        tx,
    )
}

#[derive(Default)]
struct AnthropicReplayBlocks {
    items: std::collections::BTreeMap<usize, AssistantReplayItem>,
    /// Hosted `server_tool_use.input` accumulates here, never in the local
    /// tool-call stream (it must not execute).
    server_inputs: std::collections::BTreeMap<usize, String>,
}

impl AnthropicReplayBlocks {
    fn start(
        &mut self,
        index: usize,
        block: &serde_json::Value,
        tool_stream: &mut crate::tool_stream::ToolCallStream,
        tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    ) {
        match block["type"].as_str() {
            Some("text") => {
                let citations = block["citations"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(anthropic_citation)
                    .fold(Vec::new(), |mut citations, citation| {
                        push_unique_citation(&mut citations, citation);
                        citations
                    });
                self.items.insert(
                    index,
                    AssistantReplayItem::Text {
                        text: block["text"].as_str().unwrap_or_default().to_string(),
                        citations,
                    },
                );
            }
            Some("thinking") => {
                self.items.insert(
                    index,
                    AssistantReplayItem::AnthropicThinking {
                        thinking: block["thinking"].as_str().unwrap_or_default().to_string(),
                        signature: block["signature"]
                            .as_str()
                            .filter(|value| !value.is_empty())
                            .map(str::to_string),
                    },
                );
            }
            Some("redacted_thinking") => {
                self.items.insert(
                    index,
                    AssistantReplayItem::AnthropicRedactedThinking {
                        data: block["data"].as_str().unwrap_or_default().to_string(),
                    },
                );
            }
            Some("tool_use") => {
                tool_stream.push(
                    index,
                    block["id"].as_str(),
                    block["name"].as_str(),
                    None,
                    tx,
                );
                self.items.insert(
                    index,
                    AssistantReplayItem::ToolCall {
                        call: crate::chat::ToolCall {
                            id: block["id"].as_str().unwrap_or_default().to_string(),
                            name: block["name"].as_str().unwrap_or_default().to_string(),
                            arguments: String::new(),
                        }
                        .to_wire(),
                    },
                );
            }
            Some("server_tool_use") => {
                self.items.insert(
                    index,
                    AssistantReplayItem::AnthropicServerToolUse {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        input: serde_json::json!({}),
                    },
                );
                self.server_inputs.entry(index).or_default();
            }
            Some("web_search_tool_result") => {
                self.items.insert(
                    index,
                    AssistantReplayItem::AnthropicWebSearchResult {
                        tool_use_id: block["tool_use_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        content: block
                            .get("content")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({})),
                    },
                );
            }
            _ => {}
        }
    }

    fn text(&mut self, index: usize, delta: &str) {
        if let AssistantReplayItem::Text { text, .. } =
            self.items
                .entry(index)
                .or_insert_with(|| AssistantReplayItem::Text {
                    text: String::new(),
                    citations: Vec::new(),
                })
        {
            text.push_str(delta);
        }
    }

    fn citation(&mut self, index: usize, citation: &serde_json::Value) {
        let Some(citation) = anthropic_citation(citation) else {
            return;
        };
        if let AssistantReplayItem::Text { citations, .. } =
            self.items
                .entry(index)
                .or_insert_with(|| AssistantReplayItem::Text {
                    text: String::new(),
                    citations: Vec::new(),
                })
        {
            push_unique_citation(citations, citation);
        }
    }

    fn server_delta(&mut self, index: usize, delta: &str) -> bool {
        if let Some(input) = self.server_inputs.get_mut(&index) {
            input.push_str(delta);
            return true;
        }
        false
    }

    fn thinking(&mut self, index: usize, delta: &str) {
        if let AssistantReplayItem::AnthropicThinking { thinking, .. } = self
            .items
            .entry(index)
            .or_insert_with(|| AssistantReplayItem::AnthropicThinking {
                thinking: String::new(),
                signature: None,
            })
        {
            thinking.push_str(delta);
        }
    }

    fn signature(&mut self, index: usize, delta: &str) {
        if let AssistantReplayItem::AnthropicThinking { signature, .. } = self
            .items
            .entry(index)
            .or_insert_with(|| AssistantReplayItem::AnthropicThinking {
                thinking: String::new(),
                signature: None,
            })
        {
            signature.get_or_insert_with(String::new).push_str(delta);
        }
    }

    fn into_items(
        self,
        tool_stream: &crate::tool_stream::ToolCallStream,
    ) -> Vec<AssistantReplayItem> {
        let Self {
            items,
            server_inputs,
        } = self;
        items
            .into_iter()
            .filter_map(|(index, item)| match item {
                AssistantReplayItem::ToolCall { .. } => tool_stream
                    .calls()
                    .get(index)
                    .filter(|call| !call.name.trim().is_empty())
                    .map(|call| AssistantReplayItem::ToolCall {
                        call: call.to_wire(),
                    }),
                AssistantReplayItem::AnthropicServerToolUse { id, name, .. } => {
                    let input = server_inputs
                        .get(&index)
                        .filter(|raw| !raw.trim().is_empty())
                        .and_then(|raw| serde_json::from_str(raw).ok())
                        .unwrap_or_else(|| serde_json::json!({}));
                    Some(AssistantReplayItem::AnthropicServerToolUse { id, name, input })
                }
                other => Some(other),
            })
            .collect()
    }
}

fn finish_anthropic_stream(
    mut tool_stream: crate::tool_stream::ToolCallStream,
    replay_blocks: AnthropicReplayBlocks,
    finish_reason: Option<FinishReason>,
    config: &ResolvedModel,
    api: &mut crate::api_log::ApiCall,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
) -> Result<(), CallError> {
    let Some(reason) = finish_reason else {
        let detail = "Anthropic stream ended without a stop reason".to_string();
        api.fail(&detail);
        return Err(CoreError::Internal { detail }.into());
    };
    let replay = AssistantReplay::new(config, replay_blocks.into_items(&tool_stream));
    let _ = tx.send(ProviderEvent::AssistantReplay(replay));
    api.finish(reason.as_str(), tool_stream.calls());
    tool_stream.finish(tx, reason);
    Ok(())
}

/// Running usage for one streamed message. Per the Anthropic spec, input/cache
/// fields arrive in message_start and output in message_delta. The merge is
/// per-field last-write-wins — a field present in a later frame overwrites,
/// including an explicit zero — matching the Vercel AI SDK's anthropic
/// provider (anthropic-language-model.ts; opencode, kimi-code and ZCode all
/// share the same semantics, audited 2026-10-16). This absorbs both
/// compatible-provider quirks seen so far: GLM's Anthropic endpoint sends
/// zeros in message_start and the real numbers in message_delta (the delta
/// overwrites them, observed 2026-10-09), while StepFun's message_delta
/// restates input_tokens as the cache-inclusive total and zeroes the cache
/// fields (start {input_tokens: 1, cache_read_input_tokens: 12119} → delta
/// {input_tokens: 12120, cache_read_input_tokens: 0}, observed 2026-10-16) —
/// the delta overwrites, so the total stays right and the cache-hit split is
/// lost, which is a StepFun wire bug. The previous per-field max merge (kept
/// for GLM) combined those two contradictory frames into a double count
/// (12120 uncached + 12119 hit).
#[derive(Default)]
struct StreamUsage {
    input: u64,
    cache_creation: u64,
    cache_read: u64,
    output: u64,
}

impl StreamUsage {
    fn merge(&mut self, usage: &serde_json::Value) {
        if let Some(v) = usage["input_tokens"].as_u64() {
            self.input = v;
        }
        if let Some(v) = usage["cache_creation_input_tokens"].as_u64() {
            self.cache_creation = v;
        }
        if let Some(v) = usage["cache_read_input_tokens"].as_u64() {
            self.cache_read = v;
        }
        if let Some(v) = usage["output_tokens"].as_u64() {
            self.output = v;
        }
    }

    fn normalized(
        &self,
        reasoning_output: u64,
        context_window: u64,
    ) -> Option<crate::ProviderUsage> {
        // Anthropic input excludes the cached portion; cache creation is newly
        // written input and therefore counts as uncached.
        let input = self.input + self.cache_creation;
        let used = input + self.cache_read + self.output;
        (used > 0).then_some(crate::ProviderUsage {
            input,
            cache_read: self.cache_read,
            output: self.output,
            used,
            total: context_window,
            reasoning_output,
        })
    }
}

pub(crate) fn usage_from_json(
    usage: &serde_json::Value,
    context_window: u64,
) -> Option<crate::ProviderUsage> {
    let mut normalized = StreamUsage::default();
    normalized.merge(usage);
    let reasoning_output = usage["output_tokens_details"]["thinking_tokens"]
        .as_u64()
        .unwrap_or(0);
    normalized.normalized(reasoning_output, context_window)
}

#[cfg(test)]
mod tests {

    use super::{StreamUsage, to_anthropic_messages};
    use crate::chat::{ChatDocument, ChatImage, ChatMsg};

    /// Spec-compliant Anthropic: input/cache in message_start, output in
    /// message_delta (which carries no input fields)
    #[test]
    fn usage_start_then_delta() {
        let mut usage = StreamUsage::default();
        usage.merge(&serde_json::json!({"input_tokens": 100, "cache_creation_input_tokens": 20, "cache_read_input_tokens": 30, "output_tokens": 1}));
        assert_eq!(
            (
                usage.input + usage.cache_creation,
                usage.cache_read,
                usage.output
            ),
            (120, 30, 1)
        );
        usage.merge(&serde_json::json!({"output_tokens": 55}));
        assert_eq!(
            (
                usage.input + usage.cache_creation,
                usage.cache_read,
                usage.output
            ),
            (120, 30, 55)
        );
    }

    /// GLM's Anthropic endpoint (observed 2026-10-09): message_start carries
    /// zeros, the real input/cache numbers arrive in message_delta
    #[test]
    fn usage_glm_real_numbers_in_delta() {
        let mut usage = StreamUsage::default();
        usage.merge(&serde_json::json!({"input_tokens": 0, "output_tokens": 0}));
        assert_eq!(
            (
                usage.input + usage.cache_creation,
                usage.cache_read,
                usage.output
            ),
            (0, 0, 0)
        );
        usage.merge(&serde_json::json!({"input_tokens": 9697, "output_tokens": 112, "cache_read_input_tokens": 0}));
        assert_eq!(
            (
                usage.input + usage.cache_creation,
                usage.cache_read,
                usage.output
            ),
            (9697, 0, 112)
        );
    }

    /// StepFun (observed 2026-10-16): message_delta restates input_tokens as
    /// the cache-inclusive total and zeroes the cache fields. Last-write-wins
    /// keeps the total right (no double count) at the cost of the cache-hit
    /// split — same outcome as the Vercel AI SDK
    #[test]
    fn usage_stepfun_delta_restates_total() {
        let mut usage = StreamUsage::default();
        usage.merge(&serde_json::json!({"input_tokens": 1, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 12119, "output_tokens": 0}));
        usage.merge(&serde_json::json!({"input_tokens": 12120, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 267}));
        assert_eq!(
            (
                usage.input + usage.cache_creation,
                usage.cache_read,
                usage.output
            ),
            (12120, 0, 267)
        );
    }

    #[test]
    fn replay_blocks_keep_signatures_redaction_and_order() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tool_stream = crate::tool_stream::ToolCallStream::new();
        let mut blocks = super::AnthropicReplayBlocks::default();
        blocks.start(
            0,
            &serde_json::json!({"type": "thinking", "thinking": "a"}),
            &mut tool_stream,
            &tx,
        );
        blocks.thinking(0, "b");
        blocks.signature(0, "sig-");
        blocks.signature(0, "a");
        blocks.start(
            1,
            &serde_json::json!({"type": "text", "text": "ans"}),
            &mut tool_stream,
            &tx,
        );
        blocks.text(1, "wer");
        blocks.start(
            2,
            &serde_json::json!({"type": "redacted_thinking", "data": "redacted"}),
            &mut tool_stream,
            &tx,
        );
        blocks.start(
            3,
            &serde_json::json!({"type": "thinking", "thinking": "second"}),
            &mut tool_stream,
            &tx,
        );
        blocks.signature(3, "sig-b");
        blocks.start(
            4,
            &serde_json::json!({"type": "tool_use", "id": "toolu_1", "name": "Read"}),
            &mut tool_stream,
            &tx,
        );
        tool_stream.push(4, None, None, Some("{}"), &tx);
        let items = blocks.into_items(&tool_stream);

        assert_eq!(items.len(), 5);
        match &items[..] {
            [
                crate::chat::AssistantReplayItem::AnthropicThinking {
                    thinking: first,
                    signature: first_signature,
                },
                crate::chat::AssistantReplayItem::Text { text, .. },
                crate::chat::AssistantReplayItem::AnthropicRedactedThinking { data },
                crate::chat::AssistantReplayItem::AnthropicThinking {
                    thinking: second,
                    signature: second_signature,
                },
                crate::chat::AssistantReplayItem::ToolCall { call },
            ] => {
                assert_eq!(first, "ab");
                assert_eq!(first_signature.as_deref(), Some("sig-a"));
                assert_eq!(text, "answer");
                assert_eq!(data, "redacted");
                assert_eq!(second, "second");
                assert_eq!(second_signature.as_deref(), Some("sig-b"));
                assert_eq!(call.id, "toolu_1");
                assert_eq!(call.function.arguments, "{}");
            }
            other => panic!("unexpected replay blocks: {other:?}"),
        }
    }

    #[test]
    fn text_block_start_keeps_completed_citations() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tool_stream = crate::tool_stream::ToolCallStream::new();
        let mut blocks = super::AnthropicReplayBlocks::default();
        blocks.start(
            0,
            &serde_json::json!({
                "type": "text",
                "text": "answer",
                "citations": [{
                    "type": "web_search_result_location",
                    "url": "https://example.com/a",
                    "title": "A",
                    "encrypted_index": "index",
                    "cited_text": "answer",
                }],
            }),
            &mut tool_stream,
            &tx,
        );
        let items = blocks.into_items(&tool_stream);
        match &items[0] {
            crate::chat::AssistantReplayItem::Text { citations, .. } => {
                assert_eq!(citations.len(), 1);
                assert_eq!(citations[0].url, "https://example.com/a");
                assert_eq!(citations[0].encrypted_index.as_deref(), Some("index"));
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn citations_dedupe_between_block_start_and_delta_events() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tool_stream = crate::tool_stream::ToolCallStream::new();
        let mut blocks = super::AnthropicReplayBlocks::default();
        let citation = serde_json::json!({
            "type": "web_search_result_location",
            "url": "https://example.com/a",
            "title": "A",
            "encrypted_index": "index",
            "cited_text": "answer",
        });
        blocks.start(
            0,
            &serde_json::json!({"type": "text", "text": "a", "citations": [citation.clone()]}),
            &mut tool_stream,
            &tx,
        );
        blocks.citation(0, &citation);
        blocks.citation(0, &citation);
        let items = blocks.into_items(&tool_stream);
        match &items[0] {
            crate::chat::AssistantReplayItem::Text { citations, .. } => {
                assert_eq!(
                    citations.len(),
                    1,
                    "start+delta must not duplicate: {citations:?}"
                );
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn text_delta_without_start_creates_a_replay_block() {
        let (_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::ProviderEvent>();
        let tool_stream = crate::tool_stream::ToolCallStream::new();
        let mut blocks = super::AnthropicReplayBlocks::default();
        blocks.text(0, "answer");
        let items = blocks.into_items(&tool_stream);

        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            crate::chat::AssistantReplayItem::Text { text, .. } if text == "answer"
        ));
    }

    fn server_only_replay(config: &crate::chat::ResolvedModel) -> crate::chat::AssistantReplay {
        crate::chat::AssistantReplay::new(
            config,
            vec![
                crate::chat::AssistantReplayItem::AnthropicServerToolUse {
                    id: "srvtoolu_1".into(),
                    name: "web_search".into(),
                    input: serde_json::json!({"query": "pig"}),
                },
                crate::chat::AssistantReplayItem::AnthropicWebSearchResult {
                    tool_use_id: "srvtoolu_1".into(),
                    content: serde_json::json!([{
                        "type": "web_search_result",
                        "url": "https://example.com/a",
                        "title": "A",
                        "encrypted_content": "credential",
                    }]),
                },
            ],
        )
    }

    #[test]
    fn server_only_replay_never_emits_an_empty_assistant() {
        let mut compatible = crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages);
        compatible.cap_web_search = true;
        compatible.replay_native_search = false;
        let messages = |config: &crate::chat::ResolvedModel| {
            vec![
                ChatMsg::user("search".into()),
                ChatMsg::assistant_with_replay(
                    String::new(),
                    vec![],
                    None,
                    Some(server_only_replay(config)),
                ),
                ChatMsg::user("continue".into()),
            ]
        };
        let (_, out) = to_anthropic_messages(&compatible, &messages(&compatible));
        assert_eq!(
            out.len(),
            2,
            "the unreplayable pause step is skipped: {out:?}"
        );
        assert!(out.iter().all(|message| message["role"] == "user"));
        assert!(
            out.iter()
                .all(|message| message["content"].as_str().is_some()
                    || message["content"]
                        .as_array()
                        .is_some_and(|blocks| !blocks.is_empty())),
            "no empty content array is emitted: {out:?}"
        );

        let mut official = compatible.clone();
        official.base_url = "https://api.anthropic.com".into();
        official.replay_native_search = true;
        let official_messages = messages(&official);
        let (_, out) = to_anthropic_messages(&official, &official_messages);
        assert_eq!(out.len(), 3);
        let blocks = out[1]["content"].as_array().expect("server blocks");
        assert_eq!(blocks[0]["type"], "server_tool_use");
        assert_eq!(blocks[1]["type"], "web_search_tool_result");
        assert_eq!(blocks[1]["content"][0]["encrypted_content"], "credential");

        // The same durable replay after a JSON round-trip projects identically.
        let replay = server_only_replay(&official);
        let restored: crate::chat::AssistantReplay =
            serde_json::from_str(&serde_json::to_string(&replay).expect("serialize"))
                .expect("restore replay");
        let restored_messages = vec![
            ChatMsg::user("search".into()),
            ChatMsg::assistant_with_replay(String::new(), vec![], None, Some(restored)),
            ChatMsg::user("continue".into()),
        ];
        let (_, restored_out) = to_anthropic_messages(&official, &restored_messages);
        assert_eq!(restored_out, out, "live and restart projections agree");
    }

    #[test]
    fn empty_or_unreplayable_replays_do_not_create_an_assistant() {
        let config = crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages);
        let empty = crate::chat::AssistantReplay::new(&config, vec![]);
        let (_, out) = to_anthropic_messages(
            &config,
            &[
                ChatMsg::user("u".into()),
                ChatMsg::assistant_with_replay(String::new(), vec![], None, Some(empty)),
                ChatMsg::user("next".into()),
            ],
        );
        assert_eq!(out.len(), 2, "empty replay: {out:?}");

        let mut official = config.clone();
        official.base_url = "https://api.anthropic.com".into();
        let unsigned = crate::chat::AssistantReplay::new(
            &official,
            vec![crate::chat::AssistantReplayItem::AnthropicThinking {
                thinking: "unsigned".into(),
                signature: None,
            }],
        );
        let (_, out) = to_anthropic_messages(
            &official,
            &[
                ChatMsg::user("u".into()),
                ChatMsg::assistant_with_replay(String::new(), vec![], None, Some(unsigned)),
                ChatMsg::user("next".into()),
            ],
        );
        assert_eq!(out.len(), 2, "unsigned thinking-only: {out:?}");

        let empty_signed = crate::chat::AssistantReplay::new(
            &official,
            vec![crate::chat::AssistantReplayItem::AnthropicThinking {
                thinking: String::new(),
                signature: Some("signature".into()),
            }],
        );
        let (_, out) = to_anthropic_messages(
            &official,
            &[
                ChatMsg::user("u".into()),
                ChatMsg::assistant_with_replay(String::new(), vec![], None, Some(empty_signed)),
                ChatMsg::user("next".into()),
            ],
        );
        assert_eq!(out.len(), 2, "empty signed thinking-only: {out:?}");

        let cross_origin = crate::chat::AssistantReplay::new(
            &crate::chat::test_model(pig_protocol::ApiFormat::OpenAiResponses),
            vec![crate::chat::AssistantReplayItem::ResponsesReasoning {
                id: Some("rs".into()),
                summary: vec!["summary".into()],
                content: vec![],
                encrypted_content: None,
            }],
        );
        let (_, out) = to_anthropic_messages(
            &config,
            &[
                ChatMsg::user("u".into()),
                ChatMsg::assistant_with_replay(String::new(), vec![], None, Some(cross_origin)),
                ChatMsg::user("next".into()),
            ],
        );
        assert_eq!(out.len(), 2, "cross-origin replay: {out:?}");

        let (_, out) = to_anthropic_messages(
            &config,
            &[
                ChatMsg::user("u".into()),
                ChatMsg::assistant(String::new(), vec![], None),
                ChatMsg::user("next".into()),
            ],
        );
        assert_eq!(out.len(), 2, "legacy empty assistant: {out:?}");
    }

    #[test]
    fn empty_text_with_citations_emits_no_empty_block() {
        let config = crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages);
        let replay = crate::chat::AssistantReplay::new(
            &config,
            vec![
                crate::chat::AssistantReplayItem::Text {
                    text: String::new(),
                    citations: vec![crate::chat::TextCitation {
                        url: "https://example.com/a".into(),
                        title: "A".into(),
                        snippet: None,
                        start_index: None,
                        end_index: None,
                        encrypted_index: Some("index".into()),
                        cited_text: None,
                    }],
                },
                crate::chat::AssistantReplayItem::Text {
                    text: "answer".into(),
                    citations: Vec::new(),
                },
            ],
        );
        let (_, out) = to_anthropic_messages(
            &config,
            &[
                ChatMsg::user("u".into()),
                ChatMsg::assistant_with_replay(String::new(), vec![], None, Some(replay)),
            ],
        );
        let blocks = out[1]["content"].as_array().expect("assistant blocks");
        assert_eq!(
            blocks.len(),
            1,
            "the empty text block is dropped: {blocks:?}"
        );
        assert_eq!(blocks[0]["text"], "answer");

        let only_empty = crate::chat::AssistantReplay::new(
            &config,
            vec![crate::chat::AssistantReplayItem::Text {
                text: String::new(),
                citations: Vec::new(),
            }],
        );
        let (_, out) = to_anthropic_messages(
            &config,
            &[
                ChatMsg::user("u".into()),
                ChatMsg::assistant_with_replay(String::new(), vec![], None, Some(only_empty)),
                ChatMsg::user("next".into()),
            ],
        );
        assert_eq!(out.len(), 2, "empty-text-only replay is skipped: {out:?}");
    }

    #[test]
    fn official_anthropic_drops_unsigned_reasoning() {
        let mut config = crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages);
        config.base_url = "https://api.anthropic.com".into();
        let replay = crate::chat::AssistantReplay::new(
            &config,
            vec![
                crate::chat::AssistantReplayItem::AnthropicThinking {
                    thinking: "unsigned".into(),
                    signature: None,
                },
                crate::chat::AssistantReplayItem::AnthropicThinking {
                    thinking: "signed".into(),
                    signature: Some("signature".into()),
                },
                crate::chat::AssistantReplayItem::Text {
                    text: "answer".into(),
                    citations: Vec::new(),
                },
            ],
        );
        let messages = vec![ChatMsg::assistant_with_replay(
            String::new(),
            vec![],
            Some("signed".into()),
            Some(replay),
        )];
        let (_, out) = to_anthropic_messages(&config, &messages);
        let blocks = &out[0]["content"];
        assert_eq!(blocks.as_array().map(Vec::len), Some(2));
        assert_eq!(blocks[0]["thinking"], "signed");
        assert_eq!(blocks[0]["signature"], "signature");
        assert_eq!(blocks[1]["type"], "text");

        let (_, legacy) = to_anthropic_messages(
            &config,
            &[ChatMsg::assistant(
                "answer".into(),
                vec![],
                Some("legacy unsigned".into()),
            )],
        );
        let legacy_blocks = &legacy[0]["content"];
        assert_eq!(legacy_blocks.as_array().map(Vec::len), Some(1));
        assert_eq!(legacy_blocks[0]["type"], "text");
    }

    #[test]
    fn tool_result_is_error_rides_the_block() {
        let model = crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages);
        // Success: the field is omitted (the smallest valid shape)
        let (_system, out) =
            to_anthropic_messages(&model, &[ChatMsg::tool_result("c1", "ok".into())]);
        assert!(out[0]["content"][0].get("is_error").is_none());
        // Failure/stopped: the flag sits on the tool_result block itself, not
        // inside content
        let (_system, out) = to_anthropic_messages(
            &model,
            &[ChatMsg::tool_result("c2", "boom".into()).with_error(true)],
        );
        let block = &out[0]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["is_error"], true);
        assert_eq!(block["content"], "boom", "content stays the plain output");
        // The image-block form keeps the same field placement
        let (_system, out) = to_anthropic_messages(
            &model,
            &[ChatMsg::tool_result_with_images(
                "c3",
                "image failed".into(),
                vec![ChatImage {
                    media_type: "image/png".into(),
                    data_base64: "QUJD".into(),
                    label: None,
                }],
            )
            .with_error(true)],
        );
        let block = &out[0]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["is_error"], true);
        assert!(block["content"].is_array());
    }

    #[test]
    fn media_only_turns_never_emit_empty_text_blocks() {
        let model = crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages);
        let mut user = ChatMsg::user(String::new());
        user.images = vec![ChatImage {
            media_type: "image/png".into(),
            data_base64: "QUJD".into(),
            label: None,
        }];
        let (_system, out) = to_anthropic_messages(&model, &[user]);
        let blocks = out[0]["content"].as_array().expect("user blocks");
        assert_eq!(blocks.len(), 1, "image only: {blocks:?}");
        assert_eq!(blocks[0]["type"], "image");
        assert!(
            blocks.iter().all(|block| block["type"] != "text"),
            "no empty text block: {blocks:?}"
        );

        let mut tool = ChatMsg::tool_result_with_images(
            "call-1",
            String::new(),
            vec![ChatImage {
                media_type: "image/png".into(),
                data_base64: "REVG".into(),
                label: None,
            }],
        );
        tool.documents.push(ChatDocument {
            media_type: "application/pdf".into(),
            data_base64: "JVBERi0=".into(),
            filename: "result.pdf".into(),
            label: None,
            token_estimate: None,
        });
        let (_system, out) = to_anthropic_messages(&model, &[tool]);
        let result = &out[0]["content"][0];
        assert_eq!(result["type"], "tool_result");
        let content = result["content"].as_array().expect("block array");
        assert_eq!(
            content
                .iter()
                .map(|block| block["type"].as_str().unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["image", "document"],
            "media only, no empty text: {content:?}"
        );
    }

    #[test]
    fn tool_result_documents_stay_inside_their_own_content() {
        let model = crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages);
        let mut first = ChatMsg::tool_result("call-1", String::new());
        first.documents.push(ChatDocument {
            media_type: "application/pdf".into(),
            data_base64: "JVBERi0xLjQK".into(),
            filename: "first.pdf".into(),
            label: None,
            token_estimate: None,
        });
        let mut second = ChatMsg::tool_result("call-2", "second".into());
        second.documents.push(ChatDocument {
            media_type: "application/pdf".into(),
            data_base64: "JVBERi0xLjUK".into(),
            filename: "second.pdf".into(),
            label: None,
            token_estimate: None,
        });
        let (_system, out) = to_anthropic_messages(&model, &[first, second]);
        assert_eq!(out.len(), 1, "consecutive results share one user turn");
        let blocks = out[0]["content"].as_array().expect("user blocks");
        assert_eq!(blocks.len(), 2, "no sibling documents: {blocks:?}");
        for (block, expected_id, expected_title, expected_text) in [
            (&blocks[0], "call-1", "first.pdf", None),
            (&blocks[1], "call-2", "second.pdf", Some("second")),
        ] {
            assert_eq!(block["type"], "tool_result");
            assert_eq!(block["tool_use_id"], expected_id);
            let content = block["content"].as_array().expect("tool_result content");
            assert_eq!(content[0]["type"], "document");
            assert_eq!(content[0]["title"], expected_title);
            match expected_text {
                Some(text) => {
                    assert_eq!(content.len(), 2, "document + text: {content:?}");
                    assert_eq!(content[1]["type"], "text");
                    assert_eq!(content[1]["text"], text);
                }
                None => assert_eq!(
                    content.len(),
                    1,
                    "document-only result has no empty text block: {content:?}"
                ),
            }
        }
    }

    #[test]
    fn user_images_get_label_markers() {
        let mut msg = ChatMsg::user("look".into());
        msg.images = vec![
            ChatImage {
                media_type: "image/png".into(),
                data_base64: "QUJD".into(),
                label: Some("Image 1".into()),
            },
            ChatImage {
                media_type: "image/png".into(),
                data_base64: "REVG".into(),
                label: None,
            },
        ];
        let (_system, messages) = to_anthropic_messages(
            &crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages),
            &[msg],
        );
        let blocks = &messages[0]["content"];
        // Labeled image: "[Image 1]" marker block before its image block;
        // unlabeled: no marker
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "[Image 1]");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[2]["type"], "image");
        assert_eq!(blocks[3]["type"], "text");
        assert_eq!(blocks[3]["text"], "look");
    }

    #[test]
    fn cache_breakpoints_skip_hosted_tools_and_thinking() {
        let mut body = serde_json::json!({
            "system": "frozen",
            "tools": [
                {"name": "Read", "input_schema": {"type": "object"}},
                {"type": "web_search_20250305", "name": "web_search"},
            ],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t"},
                    {"type": "redacted_thinking", "data": "r"},
                ]},
            ],
        });
        super::apply_anthropic_cache_control(&mut body, super::CachePolicy::Main);
        assert_eq!(
            body["tools"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        assert!(
            body["tools"][1].get("cache_control").is_none(),
            "hosted tools never carry the breakpoint: {body}"
        );
        assert_eq!(
            body["system"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        assert!(
            body["messages"][1]["content"]
                .as_array()
                .unwrap()
                .iter()
                .all(|block| block.get("cache_control").is_none()),
            "thinking blocks cannot be cache anchors: {body}"
        );
        assert!(
            !body.to_string().contains("ttl"),
            "no TTL may be sent: {body}"
        );

        let mut with_schema_property = serde_json::json!({
            "system": "s",
            "tools": [{
                "name": "CacheAdmin",
                "input_schema": {
                    "type": "object",
                    "properties": {"cache_control": {"type": "string"}},
                    "required": ["cache_control"],
                },
            }],
            "messages": [
                {"role": "user", "content": "admin"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "CacheAdmin", "input": {"cache_control": "keep"}},
                    {"type": "text", "text": "done"},
                ]},
            ],
        });
        super::apply_anthropic_cache_control(&mut with_schema_property, super::CachePolicy::Main);
        assert_eq!(
            with_schema_property["tools"][0]["input_schema"]["properties"]["cache_control"]["type"],
            "string",
            "a schema property named cache_control survives: {with_schema_property}"
        );
        assert_eq!(
            with_schema_property["tools"][0]["input_schema"]["required"][0],
            "cache_control"
        );
        assert_eq!(
            with_schema_property["messages"][1]["content"][0]["input"]["cache_control"], "keep",
            "tool_use arguments are context and must not be rewritten: {with_schema_property}"
        );
        assert_eq!(
            with_schema_property["tools"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );
        assert_eq!(
            with_schema_property["messages"][1]["content"][1]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );

        let mut inherited = serde_json::json!({
            "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "Read", "input_schema": {}, "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "old", "cache_control": {"type": "ephemeral"}}]},
                {"role": "user", "content": "tail"},
            ],
        });
        super::apply_anthropic_cache_control(&mut inherited, super::CachePolicy::Main);
        assert_eq!(
            inherited.to_string().matches("\"cache_control\"").count(),
            3,
            "inherited markers are normalized to at most three: {inherited}"
        );
    }

    #[test]
    fn sidecar_cache_marks_the_last_original_message_not_the_instruction() {
        let mut body = serde_json::json!({
            "system": "frozen",
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "answer"},
                {"role": "user", "content": "summarize this conversation"},
            ],
        });
        super::apply_anthropic_cache_control(&mut body, super::CachePolicy::Sidecar);
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"}),
            "the last original message keeps the live tail breakpoint: {body}"
        );
        assert!(
            !body["messages"][2]["content"]
                .to_string()
                .contains("cache_control"),
            "the appended instruction must stay unmarked: {body}"
        );

        let mut only_instruction = serde_json::json!({
            "system": "frozen",
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "summarize this conversation"}],
        });
        super::apply_anthropic_cache_control(&mut only_instruction, super::CachePolicy::Sidecar);
        assert!(
            !only_instruction["messages"][0]["content"]
                .to_string()
                .contains("cache_control"),
            "an instruction-only history gets no message breakpoint: {only_instruction}"
        );
    }

    #[test]
    fn model_or_credential_change_degrades_anthropic_native_replay() {
        let mut config = crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages);
        config.base_url = "https://api.anthropic.com".into();
        config.api_key = "key-a".into();
        config.model = "claude-sonnet-4-5".into();
        let replay = crate::chat::AssistantReplay::new(
            &config,
            vec![
                crate::chat::AssistantReplayItem::AnthropicThinking {
                    thinking: "signed reasoning".into(),
                    signature: Some("signature".into()),
                },
                crate::chat::AssistantReplayItem::Text {
                    text: "answer".into(),
                    citations: Vec::new(),
                },
            ],
        );
        let messages = vec![ChatMsg::assistant_with_replay(
            "answer".into(),
            vec![],
            Some("signed reasoning".into()),
            Some(replay),
        )];

        let (_, same) = to_anthropic_messages(&config, &messages);
        assert_eq!(same[0]["content"][0]["type"], "thinking");

        let mut model_changed = config.clone();
        model_changed.model = "claude-opus-4-5".into();
        let (_, degraded) = to_anthropic_messages(&model_changed, &messages);
        assert_eq!(
            degraded[0]["content"][0]["type"], "text",
            "a model change keeps only the deterministic text projection: {degraded:?}"
        );

        let mut key_changed = config.clone();
        key_changed.api_key = "key-b".into();
        let (_, degraded) = to_anthropic_messages(&key_changed, &messages);
        assert_eq!(degraded[0]["content"][0]["type"], "text");
    }

    #[test]
    fn native_search_replay_requires_the_hosted_search_capability() {
        let mut config = crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages);
        config.replay_native_search = true;
        let replay = crate::chat::AssistantReplay::new(
            &config,
            vec![
                crate::chat::AssistantReplayItem::AnthropicServerToolUse {
                    id: "srvtoolu_1".into(),
                    name: "web_search".into(),
                    input: serde_json::json!({"query": "pig"}),
                },
                crate::chat::AssistantReplayItem::AnthropicWebSearchResult {
                    tool_use_id: "srvtoolu_1".into(),
                    content: serde_json::json!([{
                        "type": "web_search_result",
                        "url": "https://example.com/a",
                        "title": "A",
                        "encrypted_content": "credential",
                    }]),
                },
                crate::chat::AssistantReplayItem::Text {
                    text: "answer".into(),
                    citations: vec![crate::chat::TextCitation {
                        url: "https://example.com/a".into(),
                        title: "A".into(),
                        snippet: None,
                        start_index: None,
                        end_index: None,
                        encrypted_index: Some("index".into()),
                        cited_text: None,
                    }],
                },
            ],
        );
        let messages = vec![ChatMsg::assistant_with_replay(
            String::new(),
            vec![],
            None,
            Some(replay),
        )];

        let (_, without_capability) = to_anthropic_messages(&config, &messages);
        let blocks = without_capability[0]["content"].as_array().unwrap();
        assert!(
            blocks.iter().all(|block| block["type"] != "server_tool_use"
                && block["type"] != "web_search_tool_result"),
            "cap_web_search=false degrades to text-only: {blocks:?}"
        );
        assert!(blocks[0].get("citations").is_none(), "{blocks:?}");
        assert_eq!(blocks[0]["text"], "answer");

        config.cap_web_search = true;
        let (_, with_capability) = to_anthropic_messages(&config, &messages);
        let blocks = with_capability[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "server_tool_use");
        assert_eq!(blocks[1]["type"], "web_search_tool_result");
        assert_eq!(blocks[2]["citations"][0]["encrypted_index"], "index");
    }

    #[test]
    fn compact_sidecar_drops_a_thinking_budget_that_cannot_fit() {
        let mut config = crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages);
        config.max_output_tokens = 2048;
        config.reasoning_params = Some(serde_json::json!({
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "output_config": {"effort": "high"},
        }));
        let messages = vec![
            ChatMsg::system("s".into()),
            ChatMsg::user("history".into()),
            ChatMsg::user("summarize".into()),
        ];
        let body = super::build_anthropic_body(
            &config,
            &messages,
            &[],
            false,
            super::CachePolicy::Sidecar,
        )
        .expect("sidecar body");
        assert!(
            body.get("thinking").is_none(),
            "an impossible thinking budget is dropped only for the sidecar: {body}"
        );
        assert_eq!(body["output_config"]["effort"], "high");

        let live =
            super::build_anthropic_body(&config, &messages, &[], true, super::CachePolicy::Main)
                .expect("live body");
        assert_eq!(
            live["thinking"]["budget_tokens"], 4096,
            "the main request keeps its configured thinking: {live}"
        );

        config.max_output_tokens = 8192;
        let valid = super::build_anthropic_body(
            &config,
            &messages,
            &[],
            false,
            super::CachePolicy::Sidecar,
        )
        .expect("sidecar body");
        assert_eq!(
            valid["thinking"]["budget_tokens"], 4096,
            "a fitting budget stays in the sidecar: {valid}"
        );
    }

    #[test]
    fn official_replay_drops_uncredentialed_search_blocks_and_citations() {
        let mut config = crate::chat::test_model(pig_protocol::ApiFormat::AnthropicMessages);
        config.base_url = "https://api.anthropic.com".into();
        config.cap_web_search = true;
        config.replay_native_search = true;
        let replay = crate::chat::AssistantReplay::new(
            &config,
            vec![
                crate::chat::AssistantReplayItem::AnthropicServerToolUse {
                    id: "srvtoolu_1".into(),
                    name: "web_search".into(),
                    input: serde_json::json!({"query": "pig"}),
                },
                crate::chat::AssistantReplayItem::AnthropicWebSearchResult {
                    tool_use_id: "srvtoolu_1".into(),
                    content: serde_json::json!([{
                        "type": "web_search_result",
                        "url": "https://example.com/a",
                        "title": "A",
                    }]),
                },
                crate::chat::AssistantReplayItem::Text {
                    text: "answer".into(),
                    citations: vec![crate::chat::TextCitation {
                        url: "https://example.com/a".into(),
                        title: "A".into(),
                        snippet: None,
                        start_index: None,
                        end_index: None,
                        encrypted_index: None,
                        cited_text: None,
                    }],
                },
            ],
        );
        let messages = vec![ChatMsg::assistant_with_replay(
            String::new(),
            vec![],
            None,
            Some(replay),
        )];
        let (_, out) = to_anthropic_messages(&config, &messages);
        let blocks = out[0]["content"].as_array().unwrap();
        assert!(
            blocks
                .iter()
                .all(|block| block["type"] != "server_tool_use"),
            "an orphaned server tool call is not replayed: {blocks:?}"
        );
        assert!(
            blocks
                .iter()
                .all(|block| block["type"] != "web_search_tool_result"),
            "a result without encrypted_content cannot be replayed: {blocks:?}"
        );
        assert_eq!(blocks.last().unwrap()["type"], "text");
        assert!(
            blocks.last().unwrap().get("citations").is_none(),
            "an unsigned citation is dropped while the text remains: {blocks:?}"
        );

        // A credentialed result and citation replay verbatim.
        let replay = crate::chat::AssistantReplay::new(
            &config,
            vec![
                crate::chat::AssistantReplayItem::AnthropicServerToolUse {
                    id: "srvtoolu_1".into(),
                    name: "web_search".into(),
                    input: serde_json::json!({"query": "pig"}),
                },
                crate::chat::AssistantReplayItem::AnthropicWebSearchResult {
                    tool_use_id: "srvtoolu_1".into(),
                    content: serde_json::json!([{
                        "type": "web_search_result",
                        "url": "https://example.com/a",
                        "title": "A",
                        "encrypted_content": "credential",
                    }]),
                },
                crate::chat::AssistantReplayItem::Text {
                    text: "answer".into(),
                    citations: vec![crate::chat::TextCitation {
                        url: "https://example.com/a".into(),
                        title: "A".into(),
                        snippet: None,
                        start_index: None,
                        end_index: None,
                        encrypted_index: Some("index".into()),
                        cited_text: Some("answer".into()),
                    }],
                },
            ],
        );
        let (_, out) = to_anthropic_messages(
            &config,
            &[ChatMsg::assistant_with_replay(
                String::new(),
                vec![],
                None,
                Some(replay),
            )],
        );
        let blocks = out[0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "server_tool_use");
        assert_eq!(blocks[0]["input"]["query"], "pig");
        assert_eq!(blocks[1]["type"], "web_search_tool_result");
        assert_eq!(blocks[1]["content"][0]["encrypted_content"], "credential");
        assert_eq!(blocks[2]["citations"][0]["encrypted_index"], "index");
    }
}

// ---------------- One-shot requests (compact summary / connectivity test) ----------------
