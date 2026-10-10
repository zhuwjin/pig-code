//! OpenAI Responses wire format (`POST {base}/responses`): stateless
//! full-history replay (`store: false`, no `previous_response_id` — the
//! rollout/fork story requires self-contained input, same choice codex-rs
//! makes), SSE events dispatched by the payload's own `type` (the `event:`
//! line is redundant, same decision as the Anthropic path), live deltas for
//! rendering while `response.output_item.done` carries the authoritative
//! items (tool calls complete — codex-rs pattern: argument deltas are ignored
//! and the whole function_call item is taken from the done event).
//!
//! Reasoning replay v1 mirrors codex-rs's compat-endpoint mode: assistant
//! reasoning is echoed as a `reasoning` item with a plaintext `summary` and
//! NO `encrypted_content` (only OpenAI's own endpoint can decrypt it; pig's
//! sessions stay on one provider so requesting it is a follow-up gated on a
//! persistence story — plaintext reasoning is session-memory-only today).
//! Caveat recorded from the Vercel AI SDK source: official OpenAI drops
//! reasoning items without encrypted_content under store:false, so the
//! plaintext replay targets the compat endpoints (Kimi-class) that accept it.
//!
//! Tool results carrying images (ReadMediaFile) send `output` as an array of
//! INPUT-style items — [{input_text}, {input_image}] — the shape all three
//! audited implementations agree on (kimi-code's default lowering, the Vercel
//! AI SDK's convertFunctionToolResultOutput, codex-rs's content-items form).

use futures_util::StreamExt as _;

use pig_protocol::CoreError;

use crate::api_log;
use crate::chat::{ChatMsg, ResolvedModel, ToolCall, finish, merge_reasoning_params};
use crate::events::{CallControl, CallError, ProviderEvent};
use crate::identity::apply_sdk_headers;
use crate::openai::business_error_detail;
use crate::retry::{SendOutcome, http_client, send_with_retry};
use crate::sse::SseDecoder;

/// ChatMsg list -> (top-level instructions, input items). system messages
/// hoist into `instructions` (the documented parameter; codex-rs instead
/// splices a prefix message — both are accepted, the top-level field is the
/// simpler projection and what the Vercel AI SDK sends).
pub(crate) fn to_responses_input(messages: &[ChatMsg]) -> (String, Vec<serde_json::Value>) {
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
                let mut parts = vec![serde_json::json!({
                    "type": "input_text",
                    "text": msg.content.clone().unwrap_or_default(),
                })];
                for img in &msg.images {
                    parts.push(serde_json::json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", img.media_type, img.data_base64),
                    }));
                }
                input.push(serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": parts,
                }));
            }
            "assistant" => {
                // Reasoning first (it preceded the answer), plaintext summary
                // only — see the module docs for the encrypted-content caveat
                if let Some(reasoning) = &msg.reasoning
                    && !reasoning.is_empty()
                {
                    input.push(serde_json::json!({
                        "type": "reasoning",
                        "summary": [{"type": "summary_text", "text": reasoning}],
                    }));
                }
                if let Some(content) = &msg.content {
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
            // Tool output is a plain string, or — when the result carries
            // images — an array of input-style items (see the module docs)
            "tool" => {
                let call_id = msg.tool_call_id.clone().unwrap_or_default();
                if msg.images.is_empty() {
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": msg.content.clone().unwrap_or_default(),
                    }));
                } else {
                    let mut items = vec![serde_json::json!({
                        "type": "input_text",
                        "text": msg.content.clone().unwrap_or_default(),
                    })];
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
            }
            _ => {}
        }
    }
    (instructions, input)
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
/// parameters directly on the item, `strict: false` — codex-rs sends the same
/// flat form), plus the server-side search tool per the capability.
pub(crate) fn responses_request_tools(
    config: &ResolvedModel,
    tools: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = tools
        .iter()
        .map(|tool| {
            let function = &tool["function"];
            // Strict only when the schema strictified (per-tool degradation
            // for inexpressible MCP shapes)
            let strict_params = config
                .cap_strict_tools
                .then(|| crate::strict_tools::strictify_tool_schema(&function["parameters"]))
                .flatten();
            serde_json::json!({
                "type": "function",
                "name": function["name"],
                "description": function["description"],
                "parameters": strict_params
                    .clone()
                    .unwrap_or_else(|| function["parameters"].clone()),
                "strict": strict_params.is_some(),
            })
        })
        .collect();
    if let Some(tool) = responses_web_search_tool(config) {
        out.push(tool);
    }
    out
}

/// `response.completed` usage -> the unified event fields (input, cache_read,
/// output, used, total, reasoning_output). Responses counts `input_tokens` as
/// the whole input (cached included) and `total_tokens` as the request total —
/// the same normalization the Chat Completions path applies (input = the
/// uncached portion).
fn usage_fields(
    usage: &serde_json::Value,
    context_window: u64,
) -> Option<(u64, u64, u64, u64, u64, u64)> {
    let count = |pointer: &str| usage.pointer(pointer).and_then(|v| v.as_u64());
    let total_input = count("/input_tokens")?;
    let cache_read = count("/input_tokens_details/cached_tokens").unwrap_or(0);
    let output = count("/output_tokens").unwrap_or(0);
    let used = count("/total_tokens").unwrap_or(total_input + output);
    if used == 0 {
        return None;
    }
    let reasoning_output = count("/output_tokens_details/reasoning_tokens").unwrap_or(0);
    Some((
        total_input.saturating_sub(cache_read),
        cache_read,
        output,
        used,
        context_window,
        reasoning_output,
    ))
}

/// One SSE data frame -> streaming side effects. Returns Ok(true) when the
/// response ended (completed/incomplete-tolerated/[DONE]), Ok(false) to keep
/// reading. Pure apart from `tx`/`api`.
fn handle_frame(
    data: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    tool_calls: &mut Vec<ToolCall>,
    config: &ResolvedModel,
    api: &mut api_log::ApiCall,
) -> Result<bool, CallError> {
    if data.trim() == "[DONE]" {
        return Ok(true);
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return Ok(false);
    };
    // A 200 body that is an error envelope instead of an event (xAI-style
    // gateways; same class as the Chat Completions business error)
    if value.get("type").is_none()
        && value.get("output").is_none()
        && let Some(detail) = business_error_detail(&value)
    {
        api.fail(&detail);
        return Err(CoreError::Internal { detail }.into());
    }
    match value["type"].as_str().unwrap_or("") {
        "response.output_text.delta" => {
            if let Some(delta) = value["delta"].as_str()
                && !delta.is_empty()
            {
                api.text(delta);
                let _ = tx.send(ProviderEvent::Text(delta.to_string()));
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = value["delta"].as_str()
                && !delta.is_empty()
            {
                api.reasoning(delta);
                let _ = tx.send(ProviderEvent::Reasoning(delta.to_string()));
            }
        }
        // The authoritative items: tool calls arrive complete (argument
        // deltas are ignored upstream, codex-rs does the same); message and
        // reasoning items are redundant with the already-streamed deltas
        "response.output_item.done" => {
            let item = &value["item"];
            if item["type"] == "function_call" {
                tool_calls.push(ToolCall {
                    id: item["call_id"].as_str().unwrap_or_default().to_string(),
                    name: item["name"].as_str().unwrap_or_default().to_string(),
                    arguments: item["arguments"].as_str().unwrap_or_default().to_string(),
                });
            }
        }
        "response.completed" | "response.incomplete" => {
            let incomplete = value["type"] == "response.incomplete";
            if incomplete {
                let reason = value["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("unknown");
                // "interrupted" = the turn was cut (client disconnect/token
                // budget on the server): treat the partial output as the turn
                if reason != "interrupted" {
                    let detail = format!("Incomplete response returned, reason: {reason}");
                    api.fail(&detail);
                    return Err(CoreError::Internal { detail }.into());
                }
            }
            let usage = value["response"].get("usage").filter(|u| !u.is_null());
            if let Some((input, cache_read, output, used, total, reasoning_output)) =
                usage.and_then(|u| usage_fields(u, config.context_window))
            {
                api.usage(input, cache_read, output, used, total, reasoning_output);
                let _ = tx.send(ProviderEvent::Usage {
                    input,
                    cache_read,
                    output,
                    used,
                    total,
                    reasoning_output,
                });
            }
            return Ok(true);
        }
        "response.failed" => {
            let message = value["response"]["error"]["message"]
                .as_str()
                .unwrap_or("unknown response failure");
            let detail = format!("Response failed: {message}");
            api.fail(&detail);
            return Err(CoreError::Internal { detail }.into());
        }
        // In-band stream error event (terminal; the Chat Completions path
        // treats its equivalents the same way)
        "error" => {
            let message = value["error"]["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| {
                    value["error"]
                        .as_str()
                        .unwrap_or("stream error")
                        .to_string()
                });
            let detail = format!("Stream error: {message}");
            api.fail(&detail);
            return Err(CoreError::Internal { detail }.into());
        }
        _ => {}
    }
    Ok(false)
}

pub(crate) async fn stream_responses(
    config: &ResolvedModel,
    messages: Vec<ChatMsg>,
    tools: Vec<serde_json::Value>,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    control: &CallControl,
) -> Result<(), CallError> {
    let (instructions, input) = to_responses_input(&messages);
    let tools = responses_request_tools(config, &tools);
    let mut body = serde_json::json!({
        "model": config.model,
        "input": input,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        // Stateless replay: the whole history rides every request (the
        // rollout/fork story requires self-contained input)
        "store": false,
        "stream": true,
        "max_output_tokens": config.max_output_tokens,
    });
    if !instructions.is_empty() {
        body["instructions"] = serde_json::Value::String(instructions);
    }
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools);
    }
    merge_reasoning_params(&mut body, config);

    let url = format!("{}/responses", config.base_url.trim_end_matches('/'));
    let mut api = api_log::ApiCall::new("openai.responses", &config.provider_name, &config.model);
    api.request(&url, &body);

    let client = http_client();
    let response = match send_with_retry(
        |retry| {
            apply_sdk_headers(
                client.post(&url).bearer_auth(&config.api_key),
                false,
                false,
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

    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        let detail: String = detail.chars().take(500).collect();
        api.fail(&format!("HTTP {status}: {detail}"));
        return Err(CoreError::Internal {
            detail: format!("HTTP {status}: {detail}"),
        }
        .into());
    }

    let mut byte_stream = response.bytes_stream();
    let mut sse = SseDecoder::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    loop {
        let chunk = tokio::select! {
            chunk = byte_stream.next() => chunk,
            _ = control.cancelled() => {
                api.finish("cancelled", &tool_calls);
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
            api.raw_line(&event.data);
            if handle_frame(event.data.trim(), tx, &mut tool_calls, config, &mut api)? {
                let outcome = if tool_calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                };
                api.finish(outcome, &tool_calls);
                finish(tx, &mut tool_calls);
                return Ok(());
            }
        }
    }
    let outcome = if tool_calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    api.finish(outcome, &tool_calls);
    finish(tx, &mut tool_calls);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{to_responses_input, usage_fields};
    use crate::chat::{ChatImage, ChatMsg, ToolCall};

    #[test]
    fn input_assembly_hoists_system_and_replays_turns() {
        let messages = vec![
            ChatMsg::system("be brief".into()),
            ChatMsg::user("hi".into()),
            ChatMsg::assistant(
                "thinking about it".into(),
                vec![ToolCall {
                    id: "c1".into(),
                    name: "Read".into(),
                    arguments: "{\"path\":\"a\"}".into(),
                }],
                Some("pondered".into()),
            ),
            ChatMsg::tool_result("c1", "ok".into()),
        ];
        let (instructions, input) = to_responses_input(&messages);
        assert_eq!(instructions, "be brief");
        assert_eq!(
            input.len(),
            5,
            "user + reasoning + assistant message + function_call + output"
        );
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        // Reasoning item precedes the assistant message, plaintext summary only
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["summary"][0]["text"], "pondered");
        assert!(input[1].get("encrypted_content").is_none());
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[2]["content"][0]["type"], "output_text");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["call_id"], "c1");
        assert_eq!(input[3]["name"], "Read");
        assert_eq!(input[3]["arguments"], "{\"path\":\"a\"}");
        assert_eq!(input[4]["type"], "function_call_output");
        assert_eq!(input[4]["call_id"], "c1");
        assert_eq!(input[4]["output"], "ok");
    }

    #[test]
    fn responses_request_tools_strict_flag() {
        let mut model = super::ResolvedModel {
            base_url: "http://localhost".into(),
            api_key: String::new(),
            model: "m".into(),
            context_window: 0,
            max_output_tokens: 0,
            api_format: pig_protocol::ApiFormat::OpenAiResponses,
            reasoning_params: None,
            cap_structured: false,
            cap_strict_tools: false,
            cap_web_search: false,
            web_search_tool: None,
            input_image: false,
            provider_name: "p".into(),
        };
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "Read", "description": "d", "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }}
        })];
        let off = super::responses_request_tools(&model, &tools);
        assert_eq!(off[0]["strict"], false, "off: strict stays false");
        model.cap_strict_tools = true;
        let on = super::responses_request_tools(&model, &tools);
        assert_eq!(on[0]["strict"], true);
        assert_eq!(on[0]["parameters"]["additionalProperties"], false);
    }

    #[test]
    fn responses_web_search_default_injection() {
        use pig_protocol::ApiFormat;
        let model = |cap: bool, tool: Option<serde_json::Value>| super::ResolvedModel {
            base_url: "http://localhost".into(),
            api_key: String::new(),
            model: "m".into(),
            context_window: 0,
            max_output_tokens: 0,
            api_format: ApiFormat::OpenAiResponses,
            reasoning_params: None,
            cap_structured: false,
            cap_strict_tools: false,
            cap_web_search: cap,
            web_search_tool: tool,
            input_image: false,
            provider_name: "p".into(),
        };
        // cap off -> no injection
        assert!(super::responses_web_search_tool(&model(false, None)).is_none());
        // cap on, no customization -> the official bare web_search tool
        assert_eq!(
            super::responses_web_search_tool(&model(true, None)).unwrap(),
            serde_json::json!({"type": "web_search"})
        );
        // cap on with customization -> the custom JSON verbatim
        let custom = serde_json::json!({"type": "web_search", "search_context_size": "low"});
        assert_eq!(
            super::responses_web_search_tool(&model(true, Some(custom.clone()))).unwrap(),
            custom
        );
    }

    #[test]
    fn tool_result_images_ride_the_output_array() {
        let messages = vec![ChatMsg::tool_result_with_images(
            "c1",
            "Read image x.png".into(),
            vec![ChatImage {
                media_type: "image/png".into(),
                data_base64: "QUJD".into(),
                label: Some("x.png".into()),
            }],
        )];
        let (_instructions, input) = to_responses_input(&messages);
        assert_eq!(input.len(), 1);
        let output = &input[0]["output"];
        assert!(
            output.is_array(),
            "an image-carrying result sends the array form: {output}"
        );
        assert_eq!(output[0]["type"], "input_text");
        assert_eq!(output[0]["text"], "Read image x.png");
        assert_eq!(output[1]["type"], "input_image");
        assert_eq!(output[1]["image_url"], "data:image/png;base64,QUJD");
        // The imageless path stays a plain string (regression-safe)
        let (_i, input) = to_responses_input(&[ChatMsg::tool_result("c2", "ok".into())]);
        assert!(input[0]["output"].is_string());
    }

    #[test]
    fn user_images_become_input_image_parts() {
        let mut msg = ChatMsg::user("look".into());
        msg.images = vec![ChatImage {
            media_type: "image/png".into(),
            data_base64: "QUJD".into(),
            label: Some("x.png".into()),
        }];
        let (_instructions, input) = to_responses_input(&[msg]);
        let parts = &input[0]["content"];
        assert_eq!(parts[0]["type"], "input_text");
        assert_eq!(parts[1]["type"], "input_image");
        assert_eq!(parts[1]["image_url"], "data:image/png;base64,QUJD");
    }

    /// Responses counts input_tokens as the whole input (cached included) and
    /// reports the reasoning slice separately — the same normalization the
    /// Chat Completions path applies
    #[test]
    fn usage_normalization() {
        let usage = serde_json::json!({
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 400},
            "output_tokens": 42,
            "output_tokens_details": {"reasoning_tokens": 20},
            "total_tokens": 1042,
        });
        assert_eq!(
            usage_fields(&usage, 128_000),
            Some((600, 400, 42, 1042, 128_000, 20))
        );
        // Zero usage does not emit (same guard as the other formats)
        assert!(usage_fields(&serde_json::json!({"input_tokens": 0}), 0).is_none());
    }
}
