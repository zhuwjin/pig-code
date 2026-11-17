use super::*;
use pig_provider::ChatMsg;

use super::AgentWriterGuard;
use crate::task::SessionToolState;

mod compact;
mod conversation;
mod display;
mod jsonl;
mod media;
mod store;

#[cfg(test)]
mod tests;

pub(crate) use conversation::AgentConversationStore;
pub(crate) use display::display_stored_items;
pub(crate) use display::project_display_items_with_metadata;
#[allow(unused_imports)]
pub use display::{display_items, project_display_items};
pub use jsonl::append_agent_conversation_record;
#[cfg(test)]
pub(crate) use jsonl::append_agent_record;
pub(crate) use jsonl::sync_agent_context;
pub(crate) use media::fill_document_token_estimates;
pub(crate) use media::project_history_media;
pub use media::{append_agent_message, append_agent_message_meta, append_agent_tool_result};
pub(crate) use store::read_agent_stored;
pub(crate) use store::reconcile_tool_results;
pub(crate) use store::recover_agent_context;
pub use store::{AgentMeta, read_agent};

/// Canonical message metadata: display timing/edit fields, nested-agent cards,
/// and the assistant's tool execution ids.
#[derive(Clone, Debug, Default)]
pub(crate) struct AgentMsgMeta {
    pub(crate) reasoning_ms: Option<u64>,
    pub(crate) edit: Option<pig_protocol::EditDiff>,
    pub(crate) tool_execution_ids: Vec<String>,
    pub(crate) agent_card: Option<crate::rollout::AgentCardRecord>,
    pub(crate) agent_cards: Vec<crate::rollout::AgentCardRecord>,
}

/// One canonical message before media rehydration. Compaction boundaries
/// are applied on this structure so discarded history never reads media.
#[derive(Clone)]
pub(crate) struct StoredAgentMsg {
    pub(crate) msg: pig_provider::ChatMsg,
    pub(crate) images: Vec<crate::rollout::ImageRef>,
    pub(crate) documents: Vec<crate::rollout::DocumentRef>,
    pub(crate) ts_ms: u64,
    pub(crate) reasoning_ms: Option<u64>,
    pub(crate) edit: Option<pig_protocol::EditDiff>,
    pub(crate) agent_card: Option<crate::rollout::AgentCardRecord>,
    pub(crate) agent_cards: Vec<crate::rollout::AgentCardRecord>,
}

/// Subagent context directory: {sessions_dir}/{session_id}.agents/ (one {agent_id}.jsonl per subagent)
pub fn agents_dir(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.agents"))
}

fn session_component_valid(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id != "."
        && session_id != ".."
        && !session_id.contains(['/', '\\'])
        && !session_id.chars().any(char::is_control)
}

/// Resolve the session's agent directory for fresh writes: it must be a real
/// directory directly inside the sessions directory, never a symlink/reparse
/// redirect planted between runs. Missing directories are created.
pub(crate) fn prepared_agent_dir(sessions_dir: &Path, session_id: &str) -> Result<PathBuf, String> {
    if !session_component_valid(session_id) {
        return Err("Invalid session id for a subagent context".to_string());
    }
    std::fs::create_dir_all(sessions_dir)
        .map_err(|error| format!("Failed to create sessions directory: {error}"))?;
    let canonical_sessions = sessions_dir
        .canonicalize()
        .map_err(|error| format!("Failed to resolve sessions directory: {error}"))?;
    let raw = agents_dir(sessions_dir, session_id);
    match std::fs::symlink_metadata(&raw) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "The subagent context directory {} is a symbolic link; refusing to write \
                     outside the session directory",
                    raw.display()
                ));
            }
            if !metadata.is_dir() {
                return Err(format!(
                    "The subagent context path {} is not a directory",
                    raw.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&raw).map_err(|error| {
                format!(
                    "Failed to create subagent directory {}: {error}",
                    raw.display()
                )
            })?;
        }
        Err(error) => {
            return Err(format!(
                "Failed to inspect subagent directory {}: {error}",
                raw.display()
            ));
        }
    }
    let canonical = raw
        .canonicalize()
        .map_err(|error| format!("Failed to resolve subagent directory: {error}"))?;
    if canonical.parent() != Some(canonical_sessions.as_path()) {
        return Err(format!(
            "The subagent context directory {} escapes this session's directory",
            raw.display()
        ));
    }
    Ok(canonical)
}

/// A resume target must be a bounded safe ASCII path component, never a path.
/// The whitelist keeps generated ids (`a<seconds>-<sequence>`), future
/// UUID-like ids and ordinary names working while rejecting `.`/`..`,
/// separators, controls and non-ASCII; canonical containment, non-symlink
/// checks and metadata-id equality are enforced separately by
/// [`read_agent_by_id`].
pub(crate) fn validate_agent_id(agent_id: &str) -> Result<(), String> {
    let shown: String = agent_id.chars().take(80).collect();
    let invalid = || {
        format!(
            "Invalid resume agent id {shown:?}: resume accepts a safe ASCII path component \
             ([A-Za-z0-9._-], at most 64 bytes), never a path or file name"
        )
    };
    if agent_id.is_empty() || agent_id.len() > 64 {
        return Err(invalid());
    }
    if agent_id == "." || agent_id == ".." {
        return Err(invalid());
    }
    if !agent_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(invalid());
    }
    Ok(())
}

/// Safe-component ids of this session's readable agent contexts, sorted.
/// Symlinks and names that are not a bounded safe path component are excluded
/// from the listing.
fn listed_agent_ids(sessions_dir: &Path, session_id: &str) -> Vec<String> {
    if !session_component_valid(session_id) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(agents_dir(sessions_dir, session_id)) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file() && !kind.is_symlink())
                .unwrap_or(false)
        })
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .filter(|id| validate_agent_id(id).is_ok())
        .collect();
    ids.sort();
    ids
}

fn missing_agent(agent_id: &str, sessions_dir: &Path, session_id: &str) -> String {
    let ids = listed_agent_ids(sessions_dir, session_id);
    let available = if ids.is_empty() {
        "(none)".to_string()
    } else {
        ids.join(", ")
    };
    format!("Subagent \"{agent_id}\" does not exist. Available: {available}")
}

/// Read one resume target by generated id. The id, the canonical context
/// directory (a direct child of the session directory) and the context file
/// (a direct child, not a symlink) must all agree, and the first-line metadata
/// must name the same agent id; a planted path or renamed file cannot redirect
/// a resume to another context.
fn resolve_agent_path(
    sessions_dir: &Path,
    session_id: &str,
    agent_id: &str,
) -> Result<PathBuf, String> {
    validate_agent_id(agent_id)?;
    if !session_component_valid(session_id) {
        return Err("Invalid session id for a subagent context".to_string());
    }
    let canonical_sessions = sessions_dir
        .canonicalize()
        .map_err(|_| missing_agent(agent_id, sessions_dir, session_id))?;
    let raw_dir = agents_dir(sessions_dir, session_id);
    // A link that stays inside the sessions directory would still cross into
    // another session's contexts (and its media root); only a real directory
    // named for this session is the owning directory.
    match std::fs::symlink_metadata(&raw_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "The subagent context directory {} is a symbolic link; resume is refused",
                raw_dir.display()
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(missing_agent(agent_id, sessions_dir, session_id));
        }
        Err(_) => return Err(missing_agent(agent_id, sessions_dir, session_id)),
        Ok(_) => {}
    }
    let canonical_dir = raw_dir
        .canonicalize()
        .map_err(|_| missing_agent(agent_id, sessions_dir, session_id))?;
    if canonical_dir.parent() != Some(canonical_sessions.as_path()) {
        return Err(format!(
            "The subagent context directory {} escapes this session's directory; resume is refused",
            raw_dir.display()
        ));
    }
    let raw_path = canonical_dir.join(format!("{agent_id}.jsonl"));
    let metadata = std::fs::symlink_metadata(&raw_path)
        .map_err(|_| missing_agent(agent_id, sessions_dir, session_id))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "Subagent context {} is a symbolic link; resume only accepts generated context files",
            raw_path.display()
        ));
    }
    let canonical = raw_path
        .canonicalize()
        .map_err(|_| missing_agent(agent_id, sessions_dir, session_id))?;
    if canonical.parent() != Some(canonical_dir.as_path()) {
        return Err(format!(
            "Subagent context {} escapes this session's agent directory; resume is refused",
            raw_path.display()
        ));
    }
    Ok(canonical)
}

fn check_agent_meta(meta: &AgentMeta, agent_id: &str, path: &Path) -> Result<(), String> {
    if meta.agent_id != agent_id {
        return Err(format!(
            "Subagent context {} belongs to agent_id \"{}\" (metadata), not \"{agent_id}\"",
            path.display(),
            meta.agent_id
        ));
    }
    Ok(())
}

pub(crate) fn recover_agent_by_id(
    sessions_dir: &Path,
    session_id: &str,
    agent_id: &str,
) -> Result<(), String> {
    let canonical = resolve_agent_path(sessions_dir, session_id, agent_id)?;
    recover_agent_context(&canonical)
}

pub(crate) fn read_agent_by_id(
    sessions_dir: &Path,
    session_id: &str,
    agent_id: &str,
) -> Result<(AgentMeta, Vec<ChatMsg>), String> {
    let canonical = resolve_agent_path(sessions_dir, session_id, agent_id)?;
    let (meta, history) = read_agent(&canonical)?;
    check_agent_meta(&meta, agent_id, &canonical)?;
    Ok((meta, history))
}

/// The history loader's variant: keeps each message's display metadata.
pub(crate) fn read_agent_by_id_stored(
    sessions_dir: &Path,
    session_id: &str,
    agent_id: &str,
) -> Result<(AgentMeta, Vec<StoredAgentMsg>), String> {
    let canonical = resolve_agent_path(sessions_dir, session_id, agent_id)?;
    let (meta, stored) = read_agent_stored(&canonical)?;
    check_agent_meta(&meta, agent_id, &canonical)?;
    Ok((meta, stored))
}

/// Claim the exclusive writer for a resume target. A Running registry entry or
/// a still-closing driver rejects the resume before any read or append, so a
/// stopped driver can never race a second JSONL writer.
pub(crate) fn claim_resume_writer(
    state: &SessionToolState,
    agent_id: &str,
) -> Result<AgentWriterGuard, String> {
    if let Some(task_id) = state.running_agent_task(agent_id) {
        return Err(format!(
            "This subagent is still running (task_id {task_id}); stop it with TaskStop before \
             resuming"
        ));
    }
    match state.try_claim_agent_writer(agent_id) {
        Some(guard) => {
            if let Some(task_id) = state.running_agent_task(agent_id) {
                drop(guard);
                return Err(format!(
                    "This subagent is still running (task_id {task_id}); stop it with TaskStop \
                     before resuming"
                ));
            }
            Ok(guard)
        }
        None => Err(
            "This subagent is still shutting down after a stop; wait for its terminal \
             notification, then resume"
                .to_string(),
        ),
    }
}

/// Allocate a fresh generated agent id whose context path does not yet exist,
/// and claim its writer before the first persistence call. A colliding file
/// (including a planted symlink) advances the sequence instead of overwriting.
pub(crate) fn allocate_agent_id(
    state: &SessionToolState,
    agents_dir: &Path,
    agent_seq: &mut u64,
) -> Result<(String, AgentWriterGuard), String> {
    for _ in 0..1000 {
        *agent_seq += 1;
        let agent_id = format!("a{}-{}", crate::rollout::now_secs(), *agent_seq);
        let path = agents_dir.join(format!("{agent_id}.jsonl"));
        if std::fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        if let Some(guard) = state.try_claim_agent_writer(&agent_id) {
            return Ok((agent_id, guard));
        }
    }
    Err(
        "Could not allocate a subagent id (too many colliding or active contexts); retry"
            .to_string(),
    )
}
