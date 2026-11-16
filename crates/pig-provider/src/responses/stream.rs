use futures_util::StreamExt as _;

use pig_protocol::{CoreError, FinishReason};

use crate::api_log;
use crate::chat::{AssistantReplay, AssistantReplayItem, ChatMsg, ResolvedModel, ToolCall};
use crate::events::{CallControl, CallError, ProviderEvent};
use crate::identity::apply_sdk_headers;
use crate::openai::business_error_detail;
use crate::retry::{SendOutcome, http_client, send_with_retry};
use crate::sse::SseDecoder;

use super::live::{LiveKind, LiveResponsesText};
use super::request::{build_responses_body, citation_from_annotation, citations_from_message};
use super::search::{
    merge_completed_annotations, merge_completed_native_items, nonempty_string, response_text_parts,
};

/// `response.completed` usage -> the unified event fields (input, cache_read,
/// output, used, total, reasoning_output). Responses counts `input_tokens` as
/// the whole input (cached included) and `total_tokens` as the request total —
/// the same normalization the Chat Completions path applies (input = the
/// uncached portion).
pub(crate) fn usage_fields(
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

/// One SSE data frame -> streaming side effects. Returns Ok(true) at a
/// terminal frame; the caller validates that it carried a real finish reason.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_frame(
    data: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
    tool_stream: &mut crate::tool_stream::ToolCallStream,
    replay_items: &mut Vec<(usize, AssistantReplayItem)>,
    live: &mut LiveResponsesText,
    finish_reason: &mut Option<FinishReason>,
    config: &ResolvedModel,
    api: &mut api_log::ApiCall,
    response_metadata: &mut crate::ProviderResponseMetadata,
    completed_output: &mut Option<Vec<serde_json::Value>>,
) -> Result<bool, CallError> {
    if data.trim() == "[DONE]" {
        return Ok(true);
    }
    let value = match serde_json::from_str::<serde_json::Value>(data) {
        Ok(value) => value,
        Err(error) => {
            let detail = format!("Invalid JSON in Responses stream: {error}");
            api.fail(&detail);
            return Err(CoreError::Internal { detail }.into());
        }
    };
    // A 200 body that is an error envelope instead of an event (xAI-style
    // gateways; same class as the Chat Completions business error)
    if value.get("type").is_none()
        && value.get("output").is_none()
        && let Some(detail) = business_error_detail(&value)
    {
        let error = crate::context_window::stream_error(detail);
        api.fail(&crate::core_error_en(&error));
        return Err(error.into());
    }
    if response_metadata.response_id.is_none()
        && matches!(
            value["type"].as_str(),
            Some("response.created") | Some("response.completed") | Some("response.incomplete")
        )
        && let Some(id) = value["response"]["id"].as_str()
        && !id.is_empty()
    {
        response_metadata.response_id = Some(id.to_string());
        let _ = tx.send(ProviderEvent::ResponseMetadata(response_metadata.clone()));
    }
    match value["type"].as_str().unwrap_or("") {
        "response.output_text.delta" => {
            if let Some(delta) = value["delta"].as_str()
                && !delta.is_empty()
            {
                live.text_delta(
                    value["output_index"].as_u64().map(|index| index as usize),
                    value["item_id"].as_str(),
                    delta,
                );
                api.text(delta);
                let _ = tx.send(ProviderEvent::Text(delta.to_string()));
            }
        }
        "response.output_text.annotation.added" => {
            if let Some(citation) = citation_from_annotation(&value["annotation"]) {
                live.annotation_added(
                    value["output_index"].as_u64().map(|index| index as usize),
                    value["item_id"].as_str(),
                    citation,
                );
            }
        }
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = value["delta"].as_str()
                && !delta.is_empty()
            {
                live.reasoning_summary_delta(
                    value["output_index"].as_u64().map(|index| index as usize),
                    value["item_id"].as_str(),
                    delta,
                );
                api.reasoning(delta);
                let _ = tx.send(ProviderEvent::Reasoning(delta.to_string()));
            }
        }
        "response.reasoning_text.delta" => {
            if let Some(delta) = value["delta"].as_str()
                && !delta.is_empty()
            {
                live.reasoning_content_delta(
                    value["output_index"].as_u64().map(|index| index as usize),
                    value["item_id"].as_str(),
                    delta,
                );
                api.reasoning(delta);
                let _ = tx.send(ProviderEvent::Reasoning(delta.to_string()));
            }
        }
        "response.output_item.added" => {
            let item = &value["item"];
            if item["type"] == "function_call" {
                tool_stream.push(
                    value["output_index"].as_u64().unwrap_or(0) as usize,
                    item["call_id"].as_str(),
                    item["name"].as_str(),
                    None,
                    tx,
                );
            }
        }
        "response.function_call_arguments.delta" => {
            tool_stream.push(
                value["output_index"].as_u64().unwrap_or(0) as usize,
                None,
                None,
                value["delta"].as_str(),
                tx,
            );
        }
        "response.output_item.done" => {
            let item = &value["item"];
            let kind = match item["type"].as_str() {
                Some("reasoning") => LiveKind::Reasoning,
                Some("message") => LiveKind::Text,
                _ => LiveKind::Other,
            };
            let output_index = live.note_done(
                value["output_index"].as_u64().map(|index| index as usize),
                item["id"].as_str(),
                kind,
            );
            match item["type"].as_str() {
                Some("function_call") => {
                    let call = ToolCall {
                        id: item["call_id"].as_str().unwrap_or_default().to_string(),
                        name: item["name"].as_str().unwrap_or_default().to_string(),
                        arguments: item["arguments"].as_str().unwrap_or_default().to_string(),
                    };
                    // The effective call keeps streamed arguments when the
                    // done item carries an empty value. A repeated done item is
                    // ignored by the stream and must not add a second durable
                    // call either.
                    let already_replayed = replay_items.iter().any(|(index, item)| {
                        *index == output_index
                            && matches!(item, AssistantReplayItem::ToolCall { .. })
                    });
                    let call = tool_stream.finalize(output_index, call, tx);
                    if !already_replayed && !call.name.trim().is_empty() {
                        replay_items.push((
                            output_index,
                            AssistantReplayItem::ToolCall {
                                call: call.to_wire(),
                            },
                        ));
                    }
                }
                Some("reasoning") => {
                    replay_items.push((
                        output_index,
                        AssistantReplayItem::ResponsesReasoning {
                            id: nonempty_string(item["id"].as_str()),
                            summary: response_text_parts(item, "summary", "summary_text"),
                            content: response_text_parts(item, "content", "reasoning_text"),
                            encrypted_content: nonempty_string(item["encrypted_content"].as_str()),
                        },
                    ));
                }
                Some("message") => {
                    let text = response_text_parts(item, "content", "output_text").join("");
                    if !text.is_empty() {
                        let mut citations = citations_from_message(item);
                        if citations.is_empty() {
                            citations = live.take_citations(
                                value["output_index"].as_u64().map(|index| index as usize),
                                item["id"].as_str(),
                            );
                        }
                        replay_items
                            .push((output_index, AssistantReplayItem::Text { text, citations }));
                    }
                }
                Some("web_search_call") => {
                    replay_items.push((
                        output_index,
                        AssistantReplayItem::ResponsesWebSearchCall {
                            id: nonempty_string(item["id"].as_str()),
                            status: nonempty_string(item["status"].as_str()),
                            action: item
                                .get("action")
                                .cloned()
                                .unwrap_or_else(|| serde_json::json!({})),
                            encrypted_content: nonempty_string(item["encrypted_content"].as_str()),
                        },
                    ));
                }
                _ => {}
            }
        }
        "response.completed" | "response.incomplete" => {
            let reason = if value["type"] == "response.incomplete" {
                let raw_reason = value["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("unknown");
                match raw_reason {
                    "max_output_tokens" => FinishReason::Length,
                    "content_filter" => FinishReason::ContentFilter,
                    other => {
                        // Strict rejection: keep the already-streamed text
                        // visible, but never synthesize a healthy completion
                        // or execute the step's uncertain function calls.
                        let detail = format!("Incomplete response returned, reason: {other}");
                        api.fail(&detail);
                        return Err(CoreError::Internal { detail }.into());
                    }
                }
            } else if tool_stream.calls().is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            };
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
            *completed_output = value["response"]["output"].as_array().cloned();
            *finish_reason = Some(reason);
            return Ok(true);
        }
        "response.failed" => {
            let error = &value["response"]["error"];
            let message = error["message"]
                .as_str()
                .unwrap_or("unknown response failure");
            let detail = crate::context_window::detail_with_code(
                "Response failed: ",
                message,
                error["code"].as_str(),
            );
            let error = crate::context_window::stream_error(detail);
            api.fail(&crate::core_error_en(&error));
            return Err(error.into());
        }
        "error" => {
            let message = value["error"]["message"]
                .as_str()
                .or_else(|| value["message"].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    value["error"]
                        .as_str()
                        .unwrap_or("stream error")
                        .to_string()
                });
            let detail = crate::context_window::detail_with_code(
                "Stream error: ",
                &message,
                value["error"]["code"]
                    .as_str()
                    .or_else(|| value["error"]["type"].as_str()),
            );
            let error = crate::context_window::stream_error(detail);
            api.fail(&crate::core_error_en(&error));
            return Err(error.into());
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
    let mut body =
        build_responses_body(config, &messages, &tools, true).map_err(CallError::from)?;
    if let Some(output) = control.structured_output() {
        crate::sidecar::apply_structured(&mut body, config.api_format, output);
    }

    let url = crate::endpoint::responses_url(&config.base_url);
    let mut api = api_log::ApiCall::new("openai.responses", &config.provider_name, &config.model);
    api.request(&url, &body);

    let client = http_client();
    let response = match send_with_retry(
        |retry| {
            apply_sdk_headers(
                client.post(&url).bearer_auth(&config.api_key),
                false,
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
    let mut sse = SseDecoder::new();
    let mut tool_stream = crate::tool_stream::ToolCallStream::new();
    let mut replay_items = Vec::new();
    let mut live = LiveResponsesText::default();
    let mut finish_reason = None;
    let mut completed_output = None;
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
            api.raw_line(&event.data);
            if handle_frame(
                event.data.trim(),
                tx,
                &mut tool_stream,
                &mut replay_items,
                &mut live,
                &mut finish_reason,
                config,
                &mut api,
                &mut response_metadata,
                &mut completed_output,
            )? {
                if control.is_cancelled() {
                    api.finish("cancelled", tool_stream.calls());
                    return Ok(());
                }
                return finish_responses_stream(
                    tool_stream,
                    replay_items,
                    live,
                    finish_reason,
                    completed_output,
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
    finish_responses_stream(
        tool_stream,
        replay_items,
        live,
        finish_reason,
        completed_output,
        config,
        &mut api,
        tx,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn finish_responses_stream(
    mut tool_stream: crate::tool_stream::ToolCallStream,
    mut replay_items: Vec<(usize, AssistantReplayItem)>,
    live: LiveResponsesText,
    finish_reason: Option<FinishReason>,
    completed_output: Option<Vec<serde_json::Value>>,
    config: &ResolvedModel,
    api: &mut api_log::ApiCall,
    tx: &tokio::sync::mpsc::UnboundedSender<ProviderEvent>,
) -> Result<(), CallError> {
    let Some(reason) = finish_reason else {
        let detail = "Responses stream ended without a terminal response".to_string();
        api.fail(&detail);
        return Err(CoreError::Internal { detail }.into());
    };
    for (index, call) in tool_stream.calls().iter().enumerate() {
        if call.name.trim().is_empty() {
            continue;
        }
        let already_replayed = replay_items.iter().any(|(item_index, item)| {
            *item_index == index && matches!(item, AssistantReplayItem::ToolCall { .. })
        });
        if !already_replayed {
            replay_items.push((
                index,
                AssistantReplayItem::ToolCall {
                    call: call.to_wire(),
                },
            ));
        }
    }
    if let Some(output) = &completed_output {
        merge_completed_native_items(&mut replay_items, output);
    }
    live.apply_fallback(&mut replay_items);
    if let Some(output) = &completed_output {
        merge_completed_annotations(&mut replay_items, output);
    }
    replay_items.sort_by_key(|(index, _)| *index);
    let replay = AssistantReplay::new(
        config,
        replay_items
            .into_iter()
            .map(|(_, item)| item)
            .collect::<Vec<_>>(),
    );
    let _ = tx.send(ProviderEvent::AssistantReplay(replay));
    api.finish(reason.as_str(), tool_stream.calls());
    tool_stream.finish(tx, reason);
    Ok(())
}
