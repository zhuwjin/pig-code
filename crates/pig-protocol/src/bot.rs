use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// Explicit capability boundary granted to a persistent Bot.
///
/// Ordinary bots have `control_plane = false` by default. Every capability is
/// enforced both when projecting model tool schemas and inside the execution
/// gate (`exec_tool_gated_ctx`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotCapabilities {
    /// Workspace file & shell tools (`Read`, `Write`, `Edit`, `ApplyPatch`,
    /// `Glob`, `Grep`, `ExecCommand`, `WriteStdin`, `ReadMediaFile`, plan mode).
    #[serde(default = "default_true")]
    pub workspace_tools: bool,
    /// Cross-device / cross-session orchestration tools (explicitly granted
    /// only to orchestrator bots; defaults to false).
    #[serde(default)]
    pub control_plane: bool,
    /// MCP server tools (`ToolSearch` / `CallTool` targeting `mcp__*`).
    #[serde(default = "default_true")]
    pub mcp: bool,
    /// Skill discovery and `Skill` tool invocation.
    #[serde(default = "default_true")]
    pub skills: bool,
    /// Outbound web / network tools (`FetchURL`).
    #[serde(default = "default_true")]
    pub network: bool,
    /// Subagent delegation (`Agent`, `AgentSwarm`).
    #[serde(default = "default_true")]
    pub subagents: bool,
}

impl Default for BotCapabilities {
    fn default() -> Self {
        Self::coding()
    }
}

impl BotCapabilities {
    pub const fn coding() -> Self {
        Self {
            workspace_tools: true,
            control_plane: false,
            mcp: true,
            skills: true,
            network: true,
            subagents: true,
        }
    }

    pub const fn global_assistant() -> Self {
        Self {
            workspace_tools: false,
            control_plane: false,
            mcp: true,
            skills: true,
            network: true,
            subagents: false,
        }
    }

    pub const fn orchestrator() -> Self {
        Self {
            workspace_tools: false,
            control_plane: true,
            mcp: true,
            skills: true,
            network: true,
            subagents: true,
        }
    }
}

/// Persistent Bot entity decoupled from any specific device or workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotProfile {
    pub bot_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_level: Option<String>,
    #[serde(default)]
    pub capabilities: BotCapabilities,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
}

/// One personal long-term memory markdown file belonging to a `bot_id`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotMemoryFile {
    /// Relative path inside `{data_dir}/memories/bots/{bot_id}/` (e.g. `MEMORY.md` or `prefs.md`).
    pub path: String,
    pub content: String,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub deleted: bool,
}

/// Tombstone entry for deleted custom bots during multi-device account sync.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotDeletionTombstone {
    pub bot_id: String,
    pub deleted_at_ms: u64,
}

/// Account-level sync bundle exchanged between `pig-daemon` instances via `pig-relay`.
///
/// Contains ONLY bot metadata and bot personal memory files — never workspace
/// source code, tool outputs, conversation transcripts, or model API keys.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotSyncPayload {
    #[serde(default)]
    pub bots: Vec<BotProfile>,
    #[serde(default)]
    pub deletions: Vec<BotDeletionTombstone>,
    /// Map of `bot_id` -> personal memory files under `memories/bots/{bot_id}/`.
    #[serde(default)]
    pub memories: BTreeMap<String, Vec<BotMemoryFile>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_capabilities_default_disables_control_plane() {
        let caps = BotCapabilities::default();
        assert!(caps.workspace_tools);
        assert!(!caps.control_plane);
        assert!(caps.mcp);
        assert!(caps.skills);
        assert!(caps.network);
        assert!(caps.subagents);

        let orchestrator = BotCapabilities::orchestrator();
        assert!(orchestrator.control_plane);
        assert!(!orchestrator.workspace_tools);
    }

    #[test]
    fn bot_profile_round_trips_compactly() {
        let bot = BotProfile {
            bot_id: "frontend-dev".into(),
            name: "Frontend Dev".into(),
            avatar: Some("sparkles".into()),
            description: "UI specialist".into(),
            system_prompt: "Focus on clean TSX components.".into(),
            provider_id: Some("openai".into()),
            model_id: Some("gpt-5".into()),
            reasoning_level: Some("high".into()),
            capabilities: BotCapabilities::coding(),
            builtin: false,
            created_at_ms: 1000,
            updated_at_ms: 2000,
        };
        let encoded = serde_json::to_string(&bot).expect("serialize bot");
        let decoded: BotProfile = serde_json::from_str(&encoded).expect("deserialize bot");
        assert_eq!(decoded, bot);
    }
}
