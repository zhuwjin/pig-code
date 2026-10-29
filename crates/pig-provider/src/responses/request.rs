use pig_protocol::CoreError;

use crate::chat::{
    AssistantReplayItem, ChatDocument, ChatMsg, ResolvedModel, TextCitation, merge_reasoning_params,
};

use super::search::ensure_include;

/// ChatMsg list -> (top-level instructions, input items). system messages
/// hoist into `instructions` (the documented parameter; codex-rs instead
/// splices a prefix message — both are accepted, the top-level field is the
/// simpler projection and what the Vercel AI SDK sends).
pub(crate) fn to_responses_input(
    config: &ResolvedModel,
    messages: &[ChatMsg],
) -> (String, Vec<serde_json::Value>) {
    let mut instructions = String::new();
    let mut input: Vec<serde_json::Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg.role.as_str() {
            "system" => {
                if let Some(content) = &msg.content {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(content);
                }
            }
            "user" => {
                let mut parts = Vec::new();
                // A media-only user turn must not carry an empty text part.
                if let Some(text) = msg.content.as_deref().filter(|text| !text.is_empty()) {
                    parts.push(serde_json::json!({"type": "input_text", "text": text}));
                }
                for img in &msg.images {
                    if let Some(label) = &img.label {
                        parts.push(serde_json::json!({
                            "type": "input_text",
                            "text": format!("[{label}]"),
                        }));
                    }
                    parts.push(serde_json::json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", img.media_type, img.data_base64),
                    }));
                }
                parts.extend(msg.documents.iter().map(responses_document_part));
                // Preserve a genuinely empty history slot; media-only turns
                // already have content and never reach this fallback.
                if parts.is_empty() {
                    parts.push(serde_json::json!({"type": "input_text", "text": ""}));
                }
                input.push(serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": parts,
                }));
            }
            "assistant" => {
                if let Some(replay) = &msg.replay {
                    let same_origin = replay.origin_matches(config);
                    // Native items require the capability that produced them:
                    // after the user turns hosted search off, history degrades
                    // deterministically to text while the stored replay still
                    // yields UI sources.
                    let replay_native =
                        same_origin && config.replay_native_search && config.cap_web_search;
                    for item in &replay.items {
                        match item {
                            AssistantReplayItem::Text { text, citations } if !text.is_empty() => {
                                let mut content = serde_json::json!({
                                    "type": "output_text",
                                    "text": text,
                                });
                                if replay_native && !citations.is_empty() {
                                    content["annotations"] = serde_json::Value::Array(
                                        citations.iter().map(responses_citation_value).collect(),
                                    );
                                }
                                input.push(serde_json::json!({
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [content],
                                }));
                            }
                            AssistantReplayItem::ToolCall { call }
                                if !call.function.name.trim().is_empty() =>
                            {
                                input.push(serde_json::json!({
                                    "type": "function_call",
                                    "call_id": call.id,
                                    "name": call.function.name,
                                    "arguments": call.function.arguments,
                                }));
                            }
                            AssistantReplayItem::ResponsesReasoning {
                                id,
                                summary,
                                content,
                                encrypted_content,
                            } if same_origin => {
                                if crate::endpoint::is_official_openai(&config.base_url)
                                    && encrypted_content.is_none()
                                {
                                    continue;
                                }
                                let mut value = serde_json::json!({
                                    "type": "reasoning",
                                    "summary": summary.iter().map(|text| serde_json::json!({
                                        "type": "summary_text", "text": text,
                                    })).collect::<Vec<_>>(),
                                });
                                if let Some(id) = id {
                                    value["id"] = serde_json::Value::String(id.clone());
                                }
                                if !content.is_empty() {
                                    value["content"] = content
                                        .iter()
                                        .map(|text| {
                                            serde_json::json!({"type": "reasoning_text", "text": text})
                                        })
                                        .collect::<Vec<_>>()
                                        .into();
                                }
                                if let Some(encrypted) = encrypted_content {
                                    value["encrypted_content"] =
                                        serde_json::Value::String(encrypted.clone());
                                }
                                input.push(value);
                            }
                            AssistantReplayItem::ResponsesWebSearchCall {
                                id,
                                status,
                                action,
                                encrypted_content,
                            } if replay_native => {
                                // OpenAI's WebSearchCall is {id,status,type,action}
                                // and Codex replays it under store:false with the
                                // server id; encrypted_content is an optional real
                                // field when a compatible gateway returns one.
                                let mut value = serde_json::json!({
                                    "type": "web_search_call",
                                });
                                // A missing/empty action stays absent (the real
                                // item shape allows it); never invent one.
                                if !action.is_null()
                                    && action.as_object().is_none_or(|object| !object.is_empty())
                                {
                                    value["action"] = action.clone();
                                }
                                if let Some(id) = id {
                                    value["id"] = serde_json::Value::String(id.clone());
                                }
                                if let Some(status) = status {
                                    value["status"] = serde_json::Value::String(status.clone());
                                }
                                if let Some(encrypted) = encrypted_content {
                                    value["encrypted_content"] =
                                        serde_json::Value::String(encrypted.clone());
                                }
                                input.push(value);
                            }
                            _ => {}
                        }
                    }
                } else {
                    if let Some(reasoning) = &msg.reasoning
                        && !reasoning.is_empty()
                        && !crate::endpoint::is_official_openai(&config.base_url)
                    {
                        input.push(serde_json::json!({
                            "type": "reasoning",
                            "summary": [{"type": "summary_text", "text": reasoning}],
                        }));
                    }
                    if let Some(content) = &msg.content
                        && !content.is_empty()
                    {
                        input.push(serde_json::json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": content}],
                        }));
                    }
                    if let Some(calls) = &msg.tool_calls {
                        for call in calls {
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": call.id,
                                "name": call.function.name,
                                "arguments": call.function.arguments,
                            }));
                        }
                    }
                }
            }
            // Tool output is a plain string, or — when the result carries
            // images — an array of input-style items (see the module docs)
            "tool" => {
                let call_id = msg.tool_call_id.clone().unwrap_or_default();
                if msg.images.is_empty() {
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": crate::chat::tool_result_output(msg),
                    }));
                } else {
                    let mut items = Vec::new();
                    // An image-only tool result carries no empty text part.
                    let output = crate::chat::tool_result_output(msg);
                    if !output.is_empty() {
                        items.push(serde_json::json!({"type": "input_text", "text": output}));
                    }
                    for img in &msg.images {
                        items.push(serde_json::json!({
                            "type": "input_image",
                            "image_url": format!("data:{};base64,{}", img.media_type, img.data_base64),
                        }));
                    }
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": items,
                    }));
                }
                if !msg.documents.is_empty() {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "user",
                        "content": msg.documents.iter().map(responses_document_part).collect::<Vec<_>>(),
                    }));
                }
            }
            _ => {}
        }
    }
    (instructions, input)
}

/// Responses input PDF part: a data URL, not bare base64.
fn responses_document_part(document: &ChatDocument) -> serde_json::Value {
    serde_json::json!({
        "type": "input_file",
        "filename": document.filename,
        "file_data": format!("data:{};base64,{}", document.media_type, document.data_base64),
    })
}

fn responses_citation_value(citation: &TextCitation) -> serde_json::Value {
    let mut value = serde_json::json!({
        "type": "url_citation",
        "url": citation.url,
        "title": citation.title,
    });
    if let Some(start) = citation.start_index {
        value["start_index"] = serde_json::Value::from(start);
    }
    if let Some(end) = citation.end_index {
        value["end_index"] = serde_json::Value::from(end);
    }
    value
}

/// One `url_citation` annotation -> typed citation; other annotation types
/// (file citations) are not part of this batch.
pub(super) fn citation_from_annotation(annotation: &serde_json::Value) -> Option<TextCitation> {
    if annotation["type"].as_str() != Some("url_citation") {
        return None;
    }
    let url = annotation["url"].as_str()?.trim();
    if url.is_empty() {
        return None;
    }
    Some(TextCitation {
        url: url.to_string(),
        title: annotation["title"].as_str().unwrap_or_default().to_string(),
        snippet: annotation["snippet"].as_str().map(str::to_string),
        start_index: annotation["start_index"]
            .as_u64()
            .and_then(|index| u32::try_from(index).ok()),
        end_index: annotation["end_index"]
            .as_u64()
            .and_then(|index| u32::try_from(index).ok()),
        encrypted_index: None,
        cited_text: annotation["cited_text"].as_str().map(str::to_string),
    })
}

pub(super) fn citations_from_message(item: &serde_json::Value) -> Vec<TextCitation> {
    item["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "output_text")
        .flat_map(|part| part["annotations"].as_array().into_iter().flatten())
        .filter_map(citation_from_annotation)
        .fold(Vec::new(), |mut citations, citation| {
            push_unique_citation(&mut citations, citation);
            citations
        })
}

fn push_unique_citation(citations: &mut Vec<TextCitation>, citation: TextCitation) {
    if citations.iter().any(|existing| {
        existing.url == citation.url
            && existing.start_index == citation.start_index
            && existing.end_index == citation.end_index
    }) {
        return;
    }
    citations.push(citation);
}

/// Responses endpoints: the official built-in web_search when the capability
/// is on (the bare `{"type":"web_search"}` is the documented form both OpenAI
/// and Kimi accept); a configured web_search_tool replaces it wholesale.
pub fn responses_web_search_tool(config: &ResolvedModel) -> Option<serde_json::Value> {
    if !config.cap_web_search {
        return None;
    }
    Some(
        config
            .web_search_tool
            .clone()
            .unwrap_or_else(|| serde_json::json!({"type": "web_search"})),
    )
}

/// OpenAI wire tool shape -> the flat Responses shape (name/description/
/// parameters directly on the item), plus the server-side search tool per the
/// capability. Strict is a per-tool preference: Responses keeps the explicit
/// `strict: true|false` field, but only declared-and-honored tools get `true`.
pub(crate) fn responses_request_tools(
    config: &ResolvedModel,
    tools: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let support = if config.cap_strict_tools {
        crate::tool_settings::StrictSupport::Supported
    } else {
        crate::tool_settings::StrictSupport::Disabled
    };
    let mut out = tools
        .iter()
        .map(|tool| {
            let function = &tool["function"];
            let strict_params = crate::tool_settings::strict_schema(
                support,
                crate::tool_settings::declared_strict(tool),
                &function["parameters"],
                function["name"].as_str().unwrap_or_default(),
                &config.model,
            );
            let strict = strict_params.is_some();
            let parameters = strict_params.unwrap_or_else(|| function["parameters"].clone());
            serde_json::json!({
                "type": "function",
                "name": function["name"],
                "description": function["description"],
                "parameters": parameters,
                "strict": strict,
            })
        })
        .collect::<Vec<_>>();
    if let Some(tool) = responses_web_search_tool(config) {
        out.push(tool);
    }
    out
}

/// Pure Responses request body builder shared by streaming and sidecars. The
/// full history rides every request (`store: false`); `include` merges the
/// reasoning credential and native-search source requests without duplicates.
pub(crate) fn build_responses_body(
    config: &ResolvedModel,
    messages: &[ChatMsg],
    tools: &[serde_json::Value],
    stream: bool,
) -> Result<serde_json::Value, CoreError> {
    crate::chat::validate_documents(config, messages)?;
    let (instructions, input) = to_responses_input(config, messages);
    let tools = responses_request_tools(config, tools);
    let mut body = serde_json::json!({
        "model": config.model,
        "input": input,
        "store": false,
        "stream": stream,
        "max_output_tokens": config.max_output_tokens,
    });
    if !instructions.is_empty() {
        body["instructions"] = serde_json::Value::String(instructions);
    }
    let parallel = crate::tool_settings::responses_parallel(&config.tool_settings, &tools);
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools);
        body["tool_choice"] = serde_json::Value::String("auto".to_string());
        if let Some(parallel) = parallel {
            body["parallel_tool_calls"] = serde_json::Value::Bool(parallel);
        }
    }
    if let Some(key) = &config.prompt_cache_key {
        body["prompt_cache_key"] = serde_json::Value::String(key.clone());
    }
    merge_reasoning_params(&mut body, config);
    ensure_include(
        &mut body,
        config,
        responses_web_search_tool(config).is_some(),
    );
    Ok(body)
}
