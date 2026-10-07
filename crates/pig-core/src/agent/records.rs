use super::*;

/// Subagent context directory: {sessions_dir}/{session_id}.agents/ (one {agent_id}.jsonl per subagent)
pub fn agents_dir(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.agents"))
}

/// Append one line to the subagent context JSONL (a meta / msg record); creates the
/// directory when missing. Returns Err on failure; the caller decides hot or cold (the
/// session layer currently treats it as non-fatal, same policy as rollout.append).
pub fn append_agent_record(path: &Path, line: &serde_json::Value) -> Result<(), String> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Failed to create subagent directory {}: {e}", dir.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("Failed to open subagent context {}: {e}", path.display()))?;
    let mut line = serde_json::to_string(line).map_err(|e| e.to_string())?;
    line.push('\n');
    file.write_all(line.as_bytes())
        .map_err(|e| format!("Failed to write subagent context {}: {e}", path.display()))
}

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

/// Read a subagent context: first-line meta + the remaining msg lines rebuild the history
/// (for resume). Bad lines error with the line number; serde ignores the meta line's "type"
/// field by default.
pub fn read_agent(path: &Path) -> Result<(AgentMeta, Vec<crate::provider::ChatMsg>), String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read subagent context {}: {e}", path.display()))?;
    let mut lines = raw.lines().enumerate();
    let Some((_, first)) = lines.next() else {
        return Err(format!(
            "Subagent context {} is empty: missing meta line",
            path.display()
        ));
    };
    let first: serde_json::Value = serde_json::from_str(first)
        .map_err(|e| format!("Failed to parse subagent context line 1 (meta): {e}"))?;
    if first["type"].as_str() != Some("meta") {
        return Err("Subagent context line 1 is not a meta record".to_string());
    }
    let meta: AgentMeta = serde_json::from_value(first)
        .map_err(|e| format!("Failed to parse subagent context meta record: {e}"))?;
    let mut history = Vec::new();
    for (ix, line) in lines {
        let line_no = ix + 1;
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("Failed to parse subagent context line {line_no}: {e}"))?;
        if value["type"].as_str() != Some("msg") {
            return Err(format!(
                "Subagent context line {line_no} is not a msg record"
            ));
        }
        let msg: crate::provider::ChatMsg = serde_json::from_value(value["msg"].clone())
            .map_err(|e| format!("Failed to parse subagent context line {line_no} message: {e}"))?;
        history.push(msg);
    }
    Ok((meta, history))
}

/// Subagent context -> projection for the right panel's read-only display (A3b):
/// returns (title=meta.description, subtitle="{provider} · {model}", items).
/// user -> task line; assistant -> body line + one tool line per tool call (summary from
/// tool::summarize); tool results backfill the matching tool line's output by tool_call_id
/// (truncated to 2000 chars at a char boundary); system is skipped.
/// ChatMsg has no tool-error marker, so is_error is always false.
pub fn display_items(
    meta: &AgentMeta,
    msgs: &[crate::provider::ChatMsg],
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
pub fn project_display_items(msgs: &[crate::provider::ChatMsg]) -> Vec<pig_protocol::SubagentItem> {
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
            }),
            "assistant" => {
                if let Some(text) = msg.content.as_ref().filter(|t| !t.is_empty()) {
                    items.push(pig_protocol::SubagentItem {
                        role: "assistant".to_string(),
                        text: text.clone(),
                        tool: None,
                        output: None,
                        is_error: false,
                    });
                }
                for wire in msg.tool_calls.iter().flatten() {
                    let call = crate::provider::ToolCall {
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
                }
                // No matching call (truncated/out-of-order history): drop the result
            }
            // system prompts are not displayed
            _ => {}
        }
    }
    items
}
