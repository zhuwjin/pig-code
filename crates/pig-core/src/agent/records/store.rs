use super::*;

use super::compact::{AgentCompact, apply_agent_compact};
use super::media::rehydrate_agent_media;

/// First-line meta record of the subagent context JSONL
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentMeta {
    pub agent_id: String,
    pub profile: String,
    pub description: String,
    pub model: String,
    pub provider: String,
    pub created_at: u64,
}

/// Read a subagent context: first-line meta + the remaining msg/compact lines
/// rebuild the history (for resume). Compaction markers rewrite the history at
/// complete-boundary indexes, exactly like the root rollout rebuild. Media is
/// rehydrated only for messages that survive every marker, through the owning
/// child media directory (`{jsonl}.media`), so a tampered or corrupt ref cannot
/// point the model at another file. Bad lines error with the line number; a
/// torn partial final line (a crash during append) keeps the last valid prefix
/// and is repaired before the next append.
pub fn read_agent(path: &Path) -> Result<(AgentMeta, Vec<pig_provider::ChatMsg>), String> {
    let (meta, stored) = read_agent_stored(path)?;
    let media_root = path.with_extension("media");
    let mut history = Vec::with_capacity(stored.len());
    for mut item in stored {
        rehydrate_agent_media(&mut item, &media_root);
        history.push(item.msg);
    }
    reconcile_tool_results(&mut history);
    Ok((meta, history))
}

/// Read a subagent context with its display metadata, before media rehydration.
pub(crate) fn read_agent_stored(path: &Path) -> Result<(AgentMeta, Vec<StoredAgentMsg>), String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("Failed to read subagent context {}: {e}", path.display()))?;
    let mut lines = crate::rollout::jsonl::lines(&bytes);
    let Some(first) = lines.next() else {
        return Err(format!(
            "Subagent context {} is empty: missing meta line",
            path.display()
        ));
    };
    let first_line = std::str::from_utf8(first.body)
        .map_err(|e| format!("Failed to parse subagent context line 1 (meta): {e}"))?;
    let first: serde_json::Value = serde_json::from_str(first_line)
        .map_err(|e| format!("Failed to parse subagent context line 1 (meta): {e}"))?;
    if first["type"].as_str() != Some("meta") {
        return Err("Subagent context line 1 is not a meta record".to_string());
    }
    let meta: AgentMeta = serde_json::from_value(first)
        .map_err(|e| format!("Failed to parse subagent context meta record: {e}"))?;
    let mut stored: Vec<StoredAgentMsg> = Vec::new();
    for line in lines {
        let line_no = line.number;
        let value: serde_json::Value = match serde_json::from_slice(line.body) {
            Ok(value) => value,
            // A crash during an append can leave a partial final line without
            // its newline. Keep the last valid prefix instead of discarding
            // the whole child context; a malformed line before EOF is still a
            // hard error (it cannot be told apart from real corruption).
            Err(_) if !line.terminated => {
                tracing::warn!(
                    path = %path.display(),
                    line = line_no,
                    "subagent context ends with a torn final line; ignoring it"
                );
                break;
            }
            Err(e) => {
                return Err(format!(
                    "Failed to parse subagent context line {line_no}: {e}"
                ));
            }
        };
        match value["type"].as_str() {
            Some("msg") => stored.push(parse_agent_msg(&value, line_no)?),
            Some("compact") => {
                let record: AgentCompact = serde_json::from_value(value).map_err(|e| {
                    format!("Failed to parse subagent context line {line_no} compact: {e}")
                })?;
                apply_agent_compact(&mut stored, &record, line_no);
            }
            _ => {
                return Err(format!(
                    "Subagent context line {line_no} is not a msg or compact record"
                ));
            }
        }
    }
    Ok((meta, stored))
}

fn parse_agent_msg(value: &serde_json::Value, line_no: usize) -> Result<StoredAgentMsg, String> {
    let mut msg: pig_provider::ChatMsg = serde_json::from_value(value["msg"].clone())
        .map_err(|e| format!("Failed to parse subagent context line {line_no} message: {e}"))?;
    msg.replay = serde_json::from_value(value["replay"].clone())
        .map_err(|e| format!("Failed to parse subagent context line {line_no} replay: {e}"))?;
    if !value["reasoning"].is_null() {
        msg.reasoning = serde_json::from_value(value["reasoning"].clone()).map_err(|e| {
            format!("Failed to parse subagent context line {line_no} reasoning: {e}")
        })?;
    }
    let images = if value["images"].is_null() {
        Vec::new()
    } else {
        serde_json::from_value(value["images"].clone())
            .map_err(|e| format!("Failed to parse subagent context line {line_no} images: {e}"))?
    };
    let documents = match value.get("documents").filter(|value| !value.is_null()) {
        Some(documents) => serde_json::from_value(documents.clone()).map_err(|e| {
            format!("Failed to parse subagent context line {line_no} documents: {e}")
        })?,
        None => Vec::new(),
    };
    Ok(StoredAgentMsg {
        msg,
        images,
        documents,
        ts_ms: value["ts_ms"].as_u64().unwrap_or(0),
        reasoning_ms: value["reasoning_ms"].as_u64(),
        edit: value
            .get("edit")
            .filter(|edit| !edit.is_null())
            .and_then(|edit| serde_json::from_value(edit.clone()).ok()),
    })
}

fn reconcile_tool_results(history: &mut [pig_provider::ChatMsg]) {
    let mut index = 0;
    while index < history.len() {
        if history[index].role != "assistant" {
            index += 1;
            continue;
        }
        let mut result_index = index + 1;
        let mut answered = std::collections::HashSet::new();
        while result_index < history.len() && history[result_index].role == "tool" {
            if let Some(id) = &history[result_index].tool_call_id {
                answered.insert(id.clone());
            }
            result_index += 1;
        }
        let msg = &mut history[index];
        let clear_calls = if let Some(calls) = msg.tool_calls.as_mut() {
            calls.retain(|call| answered.contains(&call.id));
            calls.is_empty()
        } else {
            false
        };
        if clear_calls {
            msg.tool_calls = None;
        }
        if let Some(replay) = msg.replay.as_mut() {
            replay.items.retain(|item| match item {
                pig_provider::AssistantReplayItem::ToolCall { call } => answered.contains(&call.id),
                _ => true,
            });
        }
        index = result_index;
    }
}
