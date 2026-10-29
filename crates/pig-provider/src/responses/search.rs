use crate::chat::{AssistantReplayItem, ResolvedModel};

use super::request::citations_from_message;

/// Merge the requested include values without duplicating reasoning or search
/// sources. `reasoning.encrypted_content` follows the existing official
/// reasoning rule; `web_search_call.action.sources` is an OpenAI-specific
/// include enum and is sent only to the first-party host, even when a
/// compatible endpoint accepts the hosted search tool.
pub(super) fn ensure_include(
    body: &mut serde_json::Value,
    config: &ResolvedModel,
    web_search: bool,
) {
    let mut wanted: Vec<&str> = Vec::new();
    let already_requested = body["include"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item == "reasoning.encrypted_content")
    });
    if already_requested
        || (crate::endpoint::is_official_openai(&config.base_url)
            && body.get("reasoning").is_some())
    {
        wanted.push("reasoning.encrypted_content");
    }
    if web_search && crate::endpoint::is_official_openai(&config.base_url) {
        // OpenAI-specific include enum; compatible Responses endpoints may
        // reject unknown values, so only the first-party host gets it.
        wanted.push("web_search_call.action.sources");
    }
    if wanted.is_empty() {
        return;
    }
    if body["include"].is_null() {
        body["include"] = serde_json::json!([]);
    }
    let Some(items) = body["include"].as_array_mut() else {
        return;
    };
    for value in wanted {
        if !items.iter().any(|item| item == value) {
            items.push(serde_json::Value::String(value.to_string()));
        }
    }
}

pub(super) fn nonempty_string(value: Option<&str>) -> Option<String> {
    value.filter(|text| !text.is_empty()).map(str::to_string)
}

pub(super) fn response_text_parts(item: &serde_json::Value, key: &str, kind: &str) -> Vec<String> {
    item[key]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part["type"] == kind)
                .filter_map(|part| part["text"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `response.completed.response.output` is the missing-item fallback: some
/// gateways omit `output_item.done` for hosted search calls. Run this before
/// the live fallback so native items keep their output order. An item matched
/// by id is merged field-by-field (action sources/status/credential) instead
/// of being skipped wholesale.
pub(super) fn merge_completed_native_items(
    replay_items: &mut Vec<(usize, AssistantReplayItem)>,
    output: &[serde_json::Value],
) {
    for (index, item) in output.iter().enumerate() {
        if item["type"] != "web_search_call" {
            continue;
        }
        let existing = replay_items
            .iter_mut()
            .find(|(_, existing)| match existing {
                AssistantReplayItem::ResponsesWebSearchCall { id, action, .. } => {
                    search_call_matches(id.as_deref(), action, item)
                }
                _ => false,
            });
        if let Some((
            _,
            AssistantReplayItem::ResponsesWebSearchCall {
                id,
                status,
                action,
                encrypted_content,
            },
        )) = existing
        {
            merge_web_search_call(id, status, action, encrypted_content, item);
            continue;
        }
        replay_items.push((
            index,
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
}

/// Match one completed search item to a done item: ids when both exist,
/// otherwise a non-empty action equality (id-less compatible gateways).
fn search_call_matches(
    id: Option<&str>,
    action: &serde_json::Value,
    completed: &serde_json::Value,
) -> bool {
    match (id, completed["id"].as_str()) {
        (Some(existing), Some(completed)) => existing == completed,
        _ => {
            completed.get("action").is_some_and(|value| {
                value.as_object().is_none_or(|object| !object.is_empty()) && action == value
            }) && action.as_object().is_none_or(|object| !object.is_empty())
        }
    }
}

fn merge_web_search_call(
    id: &mut Option<String>,
    status: &mut Option<String>,
    action: &mut serde_json::Value,
    encrypted_content: &mut Option<String>,
    completed: &serde_json::Value,
) {
    if id.is_none() {
        *id = nonempty_string(completed["id"].as_str());
    }
    if status.is_none() {
        *status = nonempty_string(completed["status"].as_str());
    }
    if encrypted_content.is_none() {
        *encrypted_content = nonempty_string(completed["encrypted_content"].as_str());
    }
    let Some(completed_action) = completed.get("action") else {
        return;
    };
    let Some(completed_object) = completed_action.as_object() else {
        if action.is_null() {
            *action = completed_action.clone();
        }
        return;
    };
    let Some(existing) = action.as_object_mut() else {
        if action.is_null() {
            *action = completed_action.clone();
        }
        return;
    };
    for (key, value) in completed_object {
        match existing.get(key) {
            None | Some(serde_json::Value::Null) => {
                existing.insert(key.clone(), value.clone());
            }
            Some(_) => {}
        }
    }
}

/// Fill annotations that the done stream or delta events did not provide.
/// Text is only materialized by the live fallback, so run this after it. The
/// output position is the primary key (the same text can legitimately appear
/// twice); a text-only match is accepted only when it is unique.
pub(super) fn merge_completed_annotations(
    replay_items: &mut [(usize, AssistantReplayItem)],
    output: &[serde_json::Value],
) {
    for (index, item) in output.iter().enumerate() {
        if item["type"] != "message" {
            continue;
        }
        let text = response_text_parts(item, "content", "output_text").join("");
        let citations = citations_from_message(item);
        if text.is_empty() || citations.is_empty() {
            continue;
        }
        let position = replay_items
            .iter()
            .position(|(replay_index, replay)| {
                *replay_index == index
                    && matches!(replay, AssistantReplayItem::Text { text: recorded, .. } if *recorded == text)
            })
            .or_else(|| {
                let positions: Vec<usize> = replay_items
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, replay))| {
                        matches!(replay, AssistantReplayItem::Text { text: recorded, .. } if *recorded == text)
                    })
                    .map(|(position, _)| position)
                    .collect();
                (positions.len() == 1).then_some(positions[0])
            });
        if let Some(position) = position
            && let (
                _,
                AssistantReplayItem::Text {
                    citations: existing,
                    ..
                },
            ) = &mut replay_items[position]
            && existing.is_empty()
        {
            *existing = citations;
        }
    }
}
