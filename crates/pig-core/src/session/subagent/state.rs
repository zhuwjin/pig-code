use super::*;

use crate::task::AgentWriterGuard;

/// Subagent drive (shared by foreground/background): step loop + narrowed
/// tool gating + context persistence. Fully owned: the foreground borrows
/// Session fields to build GateCtx; in the background even GateCtx is fully
/// owned. Child-context compaction hooks in before `stream_chat` inside
/// `drive_subagent_loop`; there is no second driver.
pub(super) struct SubagentDrive {
    pub(super) agent_id: String,
    pub(super) cwd: PathBuf,
    pub(super) data_dir: PathBuf,
    pub(super) profile: crate::agent::AgentProfile,
    pub(super) child_config: ResolvedModel,
    pub(super) tools: Vec<Box<dyn tool::Tool>>,
    pub(super) schemas: Vec<serde_json::Value>,
    pub(super) history: Vec<ChatMsg>,
    pub(super) jsonl: PathBuf,
    pub(super) max_turns: usize,
    pub(super) description: String,
    /// Session MCP handle (None = not connected): backgrounds/closures fetch
    /// inherited tools at GateCtx assembly time
    pub(super) mcp: Option<std::sync::Arc<crate::mcp::McpManager>>,
    pub(super) plugin_tools: crate::plugins::PluginToolset,
    pub(super) plugin_tool_filter: crate::plugins::PluginToolFilter,
    /// Inheritance rule snapshot (decided from the profile-narrowed result at
    /// preparation): true = inherit all connected MCP/plugin tools; false =
    /// read-only profile, inherit only tools marked read-only
    pub(super) mcp_inherits_all: bool,
    /// Workspace-trust decision for project skills: the same value that made
    /// a project skill visible in the child prompt.
    pub(super) trusted: bool,
    /// Sticky first child-context append failure. Once set, every later
    /// append fails fast and the driver returns a failure result instead of
    /// advancing live history past what the JSONL contains.
    pub(super) persist_failure: Option<String>,
    /// Exclusive JSONL writer claim; moved into the managed lifecycle before
    /// the drive starts and released when it ends.
    pub(super) writer: Option<AgentWriterGuard>,
}

impl SubagentDrive {
    pub(super) fn take_writer(&mut self) -> Option<AgentWriterGuard> {
        self.writer.take()
    }

    pub(super) async fn select_plugin_tools(&mut self, tx: &async_channel::Sender<Event>) {
        self.plugin_tool_filter = self
            .plugin_tools
            .select(&self.child_config, Some(&self.agent_id), tx)
            .await;
        self.tools.retain(|tool| !tool.is_plugin());
        self.tools.extend(
            self.plugin_tools
                .tools_filtered(&self.plugin_tool_filter)
                .into_iter()
                .filter(|tool| self.mcp_inherits_all || tool.read_only()),
        );
        self.schemas = self
            .tools
            .iter()
            .map(|tool| crate::tool::schema_with_strict(tool.as_ref()))
            .collect();
    }

    /// extra_tools for the gated execution segment: MCP/plugin tools narrowed
    /// by the inheritance rule + Skill (not in the all() static table, served
    /// via extra by name, under the same trust decision as the child prompt)
    pub(super) fn extra_tools(&self) -> Vec<Box<dyn tool::Tool>> {
        let mut extra = self
            .mcp
            .as_ref()
            .map(|mcp| mcp.child_tools(self.mcp_inherits_all))
            .unwrap_or_default();
        extra.push(Box::new(tool::SkillTool::new_trusted(
            &self.cwd,
            &self.data_dir,
            self.trusted,
        )));
        extra.extend(
            self.plugin_tools
                .tools_filtered(&self.plugin_tool_filter)
                .into_iter()
                .filter(|tool| self.mcp_inherits_all || tool.read_only()),
        );
        extra
    }
}

/// Model segment of the agent card subtitle: "{provider_name} · {model}"
/// (optionally with a thinking-level suffix)
pub(super) fn agent_card_model(drive: &SubagentDrive) -> String {
    let mut model = format!(
        "{} · {}",
        drive.child_config.provider_name, drive.child_config.model
    );
    if let Some(level) = &drive.profile.thought_level {
        model = format!("{model} · {level}");
    }
    model
}

/// ChildPrep → SubagentDrive: the one structure mapping between the shared
/// preparation builder and the drive (used by single Agent and both swarm
/// branches).
impl From<crate::agent::ChildPrep> for SubagentDrive {
    fn from(prep: crate::agent::ChildPrep) -> Self {
        Self {
            agent_id: prep.agent_id,
            cwd: prep.cwd,
            data_dir: prep.data_dir,
            profile: prep.profile,
            child_config: prep.child_config,
            tools: prep.tools,
            schemas: prep.schemas,
            history: prep.history,
            jsonl: prep.jsonl,
            max_turns: prep.max_turns,
            description: prep.description,
            mcp: prep.mcp,
            plugin_tools: prep.plugin_tools,
            plugin_tool_filter: Default::default(),
            mcp_inherits_all: prep.mcp_inherits_all,
            trusted: prep.trusted,
            persist_failure: None,
            writer: Some(prep.writer),
        }
    }
}

/// Sink for subagent progress reporting
pub(super) enum ProgressSink {
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
