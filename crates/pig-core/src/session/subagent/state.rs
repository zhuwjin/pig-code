use super::*;

use crate::agent::AgentWriterGuard;

/// Whether the child is driven inside the parent turn (foreground) or spawned
/// as a detached task (background). The only difference the child Session sees
/// is whose change tracker it inherits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubagentMode {
    Foreground,
    Background,
}

/// Child-agent payload of a `Session`. The shared agent context (identity,
/// model, history, tools) lives in `Session::agent`; everything here is the
/// child orchestration state: persistence target, narrowed policy and the
/// parent conversation this child reports into.
pub(crate) struct SubagentState {
    /// The root conversation id this child belongs to (events, store rows and
    /// approval dialogs). A child's own identity is `Session::agent.id`.
    pub(in crate::session) parent_session_id: String,
    pub(in crate::session) parent_agent_id: String,
    pub(in crate::session) profile: crate::agent::AgentProfile,
    pub(in crate::session) jsonl: PathBuf,
    pub(in crate::session) max_turns: usize,
    pub(in crate::session) description: String,
    pub(in crate::session) output_schema: Option<serde_json::Value>,
    /// Child plugin-tool selection (updated once per step).
    pub(in crate::session) plugin_tool_filter: crate::plugins::PluginToolFilter,
    /// Inheritance rule snapshot (decided from the profile-narrowed result at
    /// preparation): true = inherit all connected MCP/plugin tools; false =
    /// read-only profile, inherit only tools marked read-only
    pub(in crate::session) mcp_inherits_all: bool,
    /// Workspace-trust decision for project skills: the same value that made
    /// a project skill visible in the child prompt.
    pub(in crate::session) trusted: bool,
    /// Sticky first child-context append failure. Once set, every later
    /// append fails fast and the driver returns a failure result instead of
    /// advancing live history past what the JSONL contains.
    pub(in crate::session) persist_failure: Option<String>,
    /// Exclusive JSONL writer claim; moved into the managed lifecycle before
    /// the drive starts and released when it ends.
    pub(in crate::session) writer: Option<AgentWriterGuard>,
    /// Where live progress and child tool item ids are reported.
    pub(in crate::session) progress: ProgressSink,
    /// Whether a pause continuation must retain the paused segment (and any
    /// steer after it) when compacting.
    pub(in crate::session) preserve_latest: bool,
    /// Index of the paused assistant/server round a continuation must retain.
    pub(in crate::session) pause_pin: Option<usize>,
    /// Reactive typed-overflow recoveries used by this child (bounded).
    pub(in crate::session) overflow_recoveries: usize,
    /// History prefix already projected as shared conversation items.
    pub(in crate::session) emitted_up_to: usize,
    /// Tool-call id -> edit diff captured by the child gate (display only).
    pub(in crate::session) tool_edits: HashMap<String, pig_protocol::EditDiff>,
    /// Tool-call id -> nested Agent cards captured by the session-layer driver.
    pub(in crate::session) tool_agents: HashMap<String, Vec<crate::rollout::AgentCardRecord>>,
}

impl SubagentState {
    pub(super) fn take_writer(&mut self) -> Option<AgentWriterGuard> {
        self.writer.take()
    }
}

/// Model segment of the agent card subtitle: "{provider_name} · {model}"
/// (optionally with a thinking-level suffix)
pub(super) fn agent_card_model(session: &Session) -> String {
    let config = session.agent.config();
    let mut model = format!("{} · {}", config.provider_name, config.model);
    if let Some(level) = session
        .subagent
        .as_ref()
        .and_then(|subagent| subagent.profile.thought_level.as_ref())
    {
        model = format!("{model} · {level}");
    }
    model
}

/// Sink for subagent progress reporting
pub(in crate::session) enum ProgressSink {
    /// Foreground: SubagentProgress streamed live to the parent session's
    /// Agent tool card
    Foreground { parent_item_id: String },
    /// Background: writes the task registry's output (the parent card already
    /// ended, no SubagentProgress)
    Background { task_id: String },
}

impl ProgressSink {
    /// Ownership prefix for child-tool item_ids / approval request_ids.
    /// Foreground swarm children share one parent tool card, so the agent_id
    /// is part of the prefix: two children's identical provider call ids must
    /// not merge into one approval dialog.
    pub(super) fn ownership_prefix(&self, agent_id: &str) -> String {
        match self {
            ProgressSink::Foreground { parent_item_id } => {
                format!("{parent_item_id}-{agent_id}")
            }
            ProgressSink::Background { task_id } => task_id.clone(),
        }
    }
}

impl Session {
    /// Child-local tool state: tasks, steers, writers, read states and roots are
    /// shared with the session, but the todo list is a fresh per-child handle.
    /// Waiting in a child is a query, not delivery into the root model's history.
    pub(super) fn child_local_state(
        state: &crate::task::SessionToolState,
        agent_id: &str,
    ) -> crate::task::SessionToolState {
        let mut state = state.clone();
        state.todos = Default::default();
        state.task_store = None;
        state.owner_agent_id = Some(agent_id.to_string());
        state
    }
}
