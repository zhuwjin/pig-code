use std::path::Path;

use crate::rollout::RolloutRecord;

/// Append one canonical conversation record to a subagent context JSONL.
pub fn append_agent_conversation_record(path: &Path, record: &RolloutRecord) -> Result<(), String> {
    try_append_agent_conversation_record(path, record)
        .map_err(|error| pig_provider::core_error_en(&error))
}

pub(crate) fn try_append_agent_conversation_record(
    path: &Path,
    record: &RolloutRecord,
) -> Result<(), pig_protocol::CoreError> {
    let tool_result = matches!(record, RolloutRecord::ToolCall { .. });
    let mut store = super::AgentConversationStore::new(path);
    crate::rollout::ConversationStore::append(&mut store, record)?;
    if tool_result {
        crate::rollout::ConversationStore::sync(&mut store)?;
    }
    Ok(())
}

/// Append one raw JSONL line to the subagent context; creates
/// the directory when missing. The shared tail repair keeps a crash-torn final
/// line from gluing the next record onto it; the child's exclusive writer
/// claim makes that single-writer repair safe. Returns Err on failure. Callers
/// must treat an error as sticky for that child: a live conversation must not
/// advance past a context the JSONL does not contain (the managed driver stops
/// the child with resume guidance; preparation fails the item).
pub fn append_agent_record(path: &Path, line: &serde_json::Value) -> Result<(), String> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Failed to create subagent directory {}: {e}", dir.display()))?;
    }
    crate::rollout::jsonl::repair_tail_before_append(path, "subagent context")?;
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
