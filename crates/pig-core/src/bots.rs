//! Persistent Bot entities, built-in presets, validation, and capability gate
//! helpers.
//!
//! A Bot is a durable identity decoupled from any specific device or workspace:
//! it carries a stable `bot_id`, persona (`system_prompt`), default model
//! preference, explicit [`BotCapabilities`], and personal long-term memory
//! under `{data_dir}/memories/bots/{bot_id}/`.

use std::time::{SystemTime, UNIX_EPOCH};

use pig_protocol::{BotCapabilities, BotProfile};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Built-in default Bots seeded into `store.sqlite` on first open.
pub fn builtin_bots() -> Vec<BotProfile> {
    let ts = 1_u64;
    vec![
        BotProfile {
            bot_id: "pig-coder".into(),
            name: "Pig Coder".into(),
            avatar: Some("code".into()),
            description: "General-purpose software engineering bot with full workspace tools and personal memory.".into(),
            system_prompt: "You are Pig Coder, a pragmatic full-stack software engineer. \
                Read code before editing, write clean idiomatic implementations, verify changes \
                with builds and tests, and record durable user preferences in your personal memory."
                .into(),
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            capabilities: BotCapabilities::coding(),
            builtin: true,
            created_at_ms: ts,
            updated_at_ms: ts,
        },
        BotProfile {
            bot_id: "pig-architect".into(),
            name: "Architect".into(),
            avatar: Some("brain".into()),
            description: "Systems architect focused on design reviews, trade-off analysis, and cross-repo conventions.".into(),
            system_prompt: "You are Architect, a staff systems engineer. Lead with clear architectural \
                trade-offs, explicit invariants, failure modes, and minimal interfaces. Keep durable \
                design principles and user preferences in your personal memory."
                .into(),
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            capabilities: BotCapabilities::coding(),
            builtin: true,
            created_at_ms: ts,
            updated_at_ms: ts,
        },
        BotProfile {
            bot_id: "pig-orchestrator".into(),
            name: "Orchestrator".into(),
            avatar: Some("waypoints".into()),
            description: "Global control-plane coordinator with explicit control_plane capability and no direct workspace shell access.".into(),
            system_prompt: "You are Orchestrator, a global multi-device coordinator. Help the user \
                plan work across projects and devices, track ongoing goals in your personal memory, \
                and coordinate specialized bots cleanly without directly mutating local workspaces."
                .into(),
            provider_id: None,
            model_id: None,
            reasoning_level: None,
            capabilities: BotCapabilities::orchestrator(),
            builtin: true,
            created_at_ms: ts,
            updated_at_ms: ts,
        },
    ]
}

/// Validate and normalize a stable `bot_id` slug (`[a-z0-9_-]`, 2..=64 chars).
pub fn validate_bot_id(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.len() < 2 || trimmed.len() > 64 {
        return Err("bot_id must be between 2 and 64 characters".into());
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
    {
        return Err("bot_id may only contain lowercase letters, digits, '-' and '_'".into());
    }
    if trimmed.starts_with('-') || trimmed.starts_with('_') {
        return Err("bot_id must start with a letter or digit".into());
    }
    Ok(trimmed)
}

/// Validate and normalize a [`BotProfile`] before persisting it.
pub fn normalize_bot_profile(mut bot: BotProfile) -> Result<BotProfile, String> {
    bot.bot_id = validate_bot_id(&bot.bot_id)?;
    bot.name = bot.name.trim().to_string();
    if bot.name.is_empty() {
        return Err("bot name cannot be empty".into());
    }
    if bot.name.len() > 80 {
        return Err("bot name is too long (max 80 chars)".into());
    }
    bot.description = bot.description.trim().to_string();
    bot.system_prompt = bot.system_prompt.trim().to_string();
    bot.avatar = bot
        .avatar
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    bot.provider_id = bot
        .provider_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    bot.model_id = bot
        .model_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    bot.reasoning_level = bot
        .reasoning_level
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let now = now_ms();
    if bot.created_at_ms == 0 {
        bot.created_at_ms = now;
    }
    if bot.updated_at_ms == 0 {
        bot.updated_at_ms = now;
    }
    Ok(bot)
}

pub fn is_control_plane_tool(name: &str) -> bool {
    name.starts_with("ControlPlane") || name.starts_with("control_plane__")
}

pub fn is_workspace_tool(name: &str) -> bool {
    matches!(
        name,
        "Read"
            | "Write"
            | "Edit"
            | "ApplyPatch"
            | "Glob"
            | "Grep"
            | "ExecCommand"
            | "WriteStdin"
            | "ReadMediaFile"
            | "EnterPlanMode"
            | "ExitPlanMode"
    )
}

pub fn is_memory_io_tool(name: &str) -> bool {
    matches!(name, "Read" | "Write" | "Edit")
}

/// Enforce a Bot's explicit capability boundary and global-session workspace
/// isolation inside the execution gate.
pub fn check_tool_capability(
    tool_name: &str,
    is_mcp: bool,
    capabilities: Option<&BotCapabilities>,
    global_session: bool,
    target_in_memory_root: bool,
) -> Result<(), String> {
    let caps = capabilities.copied().unwrap_or_default();

    if is_control_plane_tool(tool_name) && !caps.control_plane {
        return Err(format!(
            "Blocked: tool `{tool_name}` requires the `control_plane` capability, which is not granted to this bot."
        ));
    }
    if is_mcp && !caps.mcp {
        return Err(format!(
            "Blocked: MCP tool `{tool_name}` is disabled because this bot does not have the `mcp` capability."
        ));
    }
    if tool_name == "FetchURL" && !caps.network {
        return Err(
            "Blocked: FetchURL is disabled because this bot does not have the `network` capability."
                .into(),
        );
    }
    if tool_name == "Skill" && !caps.skills {
        return Err(
            "Blocked: Skill is disabled because this bot does not have the `skills` capability."
                .into(),
        );
    }
    if matches!(tool_name, "Agent" | "AgentSwarm") && !caps.subagents {
        return Err(format!(
            "Blocked: `{tool_name}` is disabled because this bot does not have the `subagents` capability."
        ));
    }
    if is_workspace_tool(tool_name) && (global_session || !caps.workspace_tools) {
        if is_memory_io_tool(tool_name) && target_in_memory_root {
            return Ok(());
        }
        if global_session {
            return Err(format!(
                "Blocked: `{tool_name}` requires a bound workspace. This is a global (non-project) session — bind a workspace first, or access only your personal memory directory."
            ));
        }
        return Err(format!(
            "Blocked: `{tool_name}` is disabled because this bot does not have the `workspace_tools` capability."
        ));
    }
    Ok(())
}

/// Render a Bot's persona and active capabilities for injection into the
/// session-frozen system prompt.
pub fn bot_persona_section(bot: &BotProfile) -> String {
    let caps = bot.capabilities;
    let mut granted = Vec::new();
    if caps.workspace_tools {
        granted.push("workspace_tools");
    }
    if caps.control_plane {
        granted.push("control_plane");
    }
    if caps.mcp {
        granted.push("mcp");
    }
    if caps.skills {
        granted.push("skills");
    }
    if caps.network {
        granted.push("network");
    }
    if caps.subagents {
        granted.push("subagents");
    }
    let cap_list = if granted.is_empty() {
        "none (chat + personal memory only)".to_string()
    } else {
        granted.join(", ")
    };
    let mut out = format!(
        "## Bot identity: {} (`{}`)\n",
        bot.name, bot.bot_id
    );
    if !bot.description.is_empty() {
        out.push_str(&format!("Role: {}\n", bot.description));
    }
    out.push_str(&format!("Granted capabilities: {cap_list}\n"));
    if !bot.system_prompt.is_empty() {
        out.push('\n');
        out.push_str(&bot.system_prompt);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_bot_cannot_invoke_control_plane_tools() {
        let normal = BotCapabilities::coding();
        assert!(
            check_tool_capability("ControlPlaneDispatch", false, Some(&normal), false, false)
                .is_err()
        );
        assert!(
            check_tool_capability("control_plane__list_devices", false, Some(&normal), false, false)
                .is_err()
        );
        let orch = BotCapabilities::orchestrator();
        assert!(
            check_tool_capability("ControlPlaneDispatch", false, Some(&orch), true, false).is_ok()
        );
    }

    #[test]
    fn global_session_blocks_workspace_tools_except_personal_memory_io() {
        let caps = BotCapabilities::coding();
        assert!(check_tool_capability("ExecCommand", false, Some(&caps), true, false).is_err());
        assert!(check_tool_capability("Read", false, Some(&caps), true, false).is_err());
        assert!(check_tool_capability("Read", false, Some(&caps), true, true).is_ok());
        assert!(check_tool_capability("Write", false, Some(&caps), true, true).is_ok());
    }

    #[test]
    fn fine_grained_capabilities_enforce_network_skills_mcp_and_subagents() {
        let mut caps = BotCapabilities::coding();
        caps.network = false;
        caps.skills = false;
        caps.mcp = false;
        caps.subagents = false;
        assert!(check_tool_capability("FetchURL", false, Some(&caps), false, false).is_err());
        assert!(check_tool_capability("Skill", false, Some(&caps), false, false).is_err());
        assert!(check_tool_capability("mcp__github__issues", true, Some(&caps), false, false).is_err());
        assert!(check_tool_capability("Agent", false, Some(&caps), false, false).is_err());
        assert!(check_tool_capability("AgentSwarm", false, Some(&caps), false, false).is_err());
    }
}
