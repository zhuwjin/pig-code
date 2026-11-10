use super::*;

/// Subagent context -> projection for the right panel's read-only display (A3b):
/// returns (title=meta.description, subtitle="{provider} · {model}", items).
/// user -> task line; assistant -> reasoning line (when present) + body line
/// + one tool line per tool call (summary from tool::summarize); tool results
/// backfill the matching tool line's output by tool_call_id (truncated to 2000
/// chars at a char boundary); system is skipped.
/// The tool row's is_error comes from the result message's execution status.
pub fn display_items(
    meta: &AgentMeta,
    msgs: &[pig_provider::ChatMsg],
) -> (String, String, Vec<pig_protocol::SubagentItem>) {
    let title = meta.description.clone();
    let subtitle = format!("{} · {}", meta.provider, meta.model);
    (title, subtitle, project_display_items(msgs))
}

/// A batch of messages -> display items (A3d: shared by full loading and SubagentActivity
/// incremental projection). Tool results backfill the matching tool line's output by
/// tool_call_id **within this batch** — in the incremental case a batch = one step's
/// assistant (with tool_calls) + the tool results that immediately follow, so matching
/// naturally lands inside the batch.
pub fn project_display_items(msgs: &[pig_provider::ChatMsg]) -> Vec<pig_protocol::SubagentItem> {
    let mut items: Vec<pig_protocol::SubagentItem> = Vec::new();
    // Tool-row indexes indexed by call id: for backfilling output from tool result messages
    let mut tool_row_by_call: HashMap<String, usize> = HashMap::new();
    for msg in msgs {
        match msg.role.as_str() {
            "user" => items.push(pig_protocol::SubagentItem {
                role: "user".to_string(),
                text: msg.content.clone().unwrap_or_default(),
                tool: None,
                output: None,
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
                        is_error: false,
                        stopped: false,
                    });
                }
                if let Some(text) = msg.content.as_ref().filter(|t| !t.is_empty()) {
                    items.push(pig_protocol::SubagentItem {
                        role: "assistant".to_string(),
                        text: text.clone(),
                        tool: None,
                        output: None,
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
                }
                // No matching call (truncated/out-of-order history): drop the result
            }
            // system prompts are not displayed
            _ => {}
        }
    }
    items
}
