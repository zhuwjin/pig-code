use std::path::Path;

/// Append one line to the subagent context JSONL (a meta / msg record); creates
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
