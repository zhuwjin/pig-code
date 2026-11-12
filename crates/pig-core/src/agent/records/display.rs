use super::*;

/// Subagent context -> projection for the right panel's read-only display (A3b):
/// returns (title=meta.description, subtitle="{provider} · {model}", items).
/// user -> task line; assistant -> reasoning line (when present) + body line
/// + one tool line per tool call (summary from tool::summarize); tool results
/// backfill the matching tool line's output and edit by tool_call_id (output
/// truncated to 2000 chars at a char boundary); system is skipped.
/// The tool row's is_error comes from the result message's execution status.
pub fn display_items(
    meta: &AgentMeta,
    msgs: &[pig_provider::ChatMsg],
) -> (String, String, Vec<pig_protocol::SubagentItem>) {
    (meta.description.clone(), subtitle(meta), project_display_items(msgs))
}

/// The stored variant used by the history loader: it keeps the record's display
/// metadata (timestamps, reasoning duration, tool edits).
pub(crate) fn display_stored_items(
    meta: &AgentMeta,
    stored: &[StoredAgentMsg],
) -> (String, String, Vec<pig_protocol::SubagentItem>) {
    (
        meta.description.clone(),
        subtitle(meta),
        project_stored_items(stored, &HashMap::new()),
    )
}

fn subtitle(meta: &AgentMeta) -> String {
    format!("{} · {}", meta.provider, meta.model)
}

/// A batch of messages -> display items (A3d: shared by full loading and SubagentActivity
/// incremental projection). Tool results backfill the matching tool line's output by
/// tool_call_id **within this batch** — in the incremental case a batch = one step's
/// assistant (with tool_calls) + the tool results that immediately follow, so matching
/// naturally lands inside the batch.
pub fn project_display_items(msgs: &[pig_provider::ChatMsg]) -> Vec<pig_protocol::SubagentItem> {
    project_display_items_with_edits(msgs, &HashMap::new())
}

/// The live path's variant: tool edits are not in the message yet, so they come
/// from the drive's call-id map.
pub(crate) fn project_display_items_with_edits(
    msgs: &[pig_provider::ChatMsg],
    edits: &HashMap<String, pig_protocol::EditDiff>,
) -> Vec<pig_protocol::SubagentItem> {
    let stored: Vec<StoredAgentMsg> = msgs
        .iter()
        .map(|msg| StoredAgentMsg {
            msg: msg.clone(),
            images: Vec::new(),
            documents: Vec::new(),
            ts_ms: 0,
            reasoning_ms: None,
            edit: None,
        })
        .collect();
    project_stored_items(&stored, edits)
}

fn project_stored_items(
    stored: &[StoredAgentMsg],
    edits: &HashMap<String, pig_protocol::EditDiff>,
) -> Vec<pig_protocol::SubagentItem> {
    let mut items: Vec<pig_protocol::SubagentItem> = Vec::new();
    // Tool-row indexes indexed by call id: for backfilling output from tool result messages
    let mut tool_row_by_call: HashMap<String, usize> = HashMap::new();
    for entry in stored {
        let msg = &entry.msg;
        match msg.role.as_str() {
            "user" => items.push(pig_protocol::SubagentItem {
                role: "user".to_string(),
                text: msg.content.clone().unwrap_or_default(),
                tool: None,
                output: None,
                detail: None,
                ts_ms: entry.ts_ms,
                duration_ms: None,
                edit: None,
                sources: Vec::new(),
                is_error: false,
                stopped: false,
            }),
            "assistant" => {
                if let Some(reasoning) = msg.reasoning.as_ref().filter(|text| !text.is_empty()) {
                    items.push(pig_protocol::SubagentItem {
                        role: "reasoning".to_string(),
                        text: reasoning.clone(),
                        tool: None,
                        output: None,
                        detail: None,
                        ts_ms: entry.ts_ms,
                        duration_ms: entry.reasoning_ms,
                        edit: None,
                        sources: Vec::new(),
                        is_error: false,
                        stopped: false,
                    });
                }
                if let Some(text) = msg.content.as_ref().filter(|t| !t.is_empty()) {
                    let sources = msg
                        .replay
                        .as_ref()
                        .map(pig_provider::replay_sources)
                        .unwrap_or_default();
                    items.push(pig_protocol::SubagentItem {
                        role: "assistant".to_string(),
                        text: text.clone(),
                        tool: None,
                        output: None,
                        detail: None,
                        ts_ms: entry.ts_ms,
                        duration_ms: None,
                        edit: None,
                        sources,
                        is_error: false,
                        stopped: false,
                    });
                }
                for wire in msg.tool_calls.iter().flatten() {
                    let call = pig_provider::ToolCall {
                        id: wire.id.clone(),
                        name: wire.function.name.clone(),
                        arguments: wire.function.arguments.clone(),
                    };
                    tool_row_by_call.insert(call.id.clone(), items.len());
                    items.push(pig_protocol::SubagentItem {
                        role: "tool".to_string(),
                        text: crate::tool::summarize(&call),
                        tool: Some(call.name.clone()),
                        output: None,
                        detail: Some(call.arguments.clone()),
                        ts_ms: entry.ts_ms,
                        duration_ms: None,
                        edit: edits.get(&call.id).cloned(),
                        sources: Vec::new(),
                        is_error: false,
                        stopped: false,
                    });
                }
            }
            "tool" => {
                let output: String = msg
                    .content
                    .clone()
                    .unwrap_or_default()
                    .chars()
                    .take(2000)
                    .collect();
                if let Some(ix) = msg
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| tool_row_by_call.get(id))
                {
                    items[*ix].output = Some(output);
                    items[*ix].is_error = msg.is_error;
                    items[*ix].stopped = msg.stopped;
                    items[*ix].edit = entry.edit.clone().or_else(|| {
                        msg.tool_call_id
                            .as_ref()
                            .and_then(|id| edits.get(id))
                            .cloned()
                    });
                }
                // No matching call (truncated/out-of-order history): drop the result
            }
            // system prompts are not displayed
            _ => {}
        }
    }
    items
}
