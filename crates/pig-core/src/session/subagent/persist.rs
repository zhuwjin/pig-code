use super::*;

use super::state::SubagentDrive;

/// Append one line to a subagent context JSONL for test fixtures. The live
/// child driver persists through [`persist_child_message`], which is sticky.
#[cfg(test)]
pub(super) fn persist_agent_line(jsonl: &Path, line: &serde_json::Value) {
    if let Err(error) = crate::agent::append_agent_record(jsonl, line) {
        tracing::error!("failed to persist subagent context: {error}");
    }
}
#[cfg(test)]
const TEST_CONTEXT_WINDOW: u64 = 128_000;

#[cfg(test)]
pub(super) fn persist_agent_msg(jsonl: &Path, msg: &mut ChatMsg) {
    if let Err(error) = crate::agent::append_agent_message(jsonl, msg, TEST_CONTEXT_WINDOW) {
        tracing::error!("failed to persist subagent message: {error}");
    }
}

/// Persist one child message with sticky failure semantics: the first append
/// failure is remembered and every later append fails fast, so the driver
/// never advances live history the JSONL cannot reproduce.
pub(super) fn persist_child_message(
    run: &mut SubagentDrive,
    msg: &mut ChatMsg,
) -> Result<(), String> {
    if let Some(error) = &run.persist_failure {
        return Err(error.clone());
    }
    match crate::agent::append_agent_message(&run.jsonl, msg, run.child_config.context_window) {
        Ok(()) => Ok(()),
        Err(error) => {
            run.persist_failure = Some(error.clone());
            Err(error)
        }
    }
}

/// Terminal child result for a sticky context-write failure.
pub(super) fn child_persist_stop(
    run: &SubagentDrive,
    error: &str,
    turns: usize,
    usage: (u64, u64, u64),
) -> SubagentDriveResult {
    SubagentDriveResult {
        status_line: "failed: subagent context write failed".to_string(),
        result_text: crate::agent::child_persist_failure_note(
            &run.agent_id,
            error,
            true,
            crate::agent::ChildPersistPhase::Stopped,
        ),
        turns,
        completed: false,
        is_error: true,
        usage,
        cancelled: false,
    }
}
